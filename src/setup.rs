//! Loopback-only worker onboarding. All execution choices belong to this machine's owner.
use crate::http::{Request, Response};
use crate::worker::{Config, SnapshotMode, SnapshotNetwork, SnapshotTask, Task};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
type Result<T> = std::result::Result<T, String>;
const MANAGED_TASK: &str = "sharebox-tests";
pub const HELP: &str = "sharebox setup [--config PATH] [--no-open]\n\nOpen this machine's private worker setup page. Paste a master's registration link,\nchoose an allowed test preset, and explicitly enable Docker or trusted native snapshots.\nExisting custom tasks and pairing are preserved. The setup page only listens on loopback.";

fn nonce() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| "Secure randomness is unavailable")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Execute a fixed utility with bounded output and a whole-process deadline.
fn utility(program: &str, args: &[&str], seconds: u64) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|_| format!("{program} is not available"))?;
    let mut stdout = child.stdout.take().ok_or("Cannot read utility output")?;
    let output = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.by_ref().take(8192).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            let bytes = output.join().map_err(|_| "Utility output reader failed")?;
            return if status.success() {
                Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
            } else {
                Err(format!("{program} did not complete successfully"))
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program} timed out"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn docker_probe() -> Value {
    if utility("docker", &["--version"], 3).is_err() {
        return json!({"available":false,"ready":false,"checked":true,"message":"Docker is not installed or is not on this application's PATH."});
    }
    match utility("docker", &["info", "--format", "{{.OSType}}"], 5) {
        Ok(os) if os == "linux" => {
            json!({"available":true,"ready":true,"checked":true,"message":"Docker daemon is ready."})
        }
        Ok(_) => {
            json!({"available":true,"ready":false,"checked":true,"message":"Docker is running non-Linux containers. Switch Docker to Linux containers, then check again."})
        }
        _ => {
            json!({"available":true,"ready":false,"checked":true,"message":"Docker is installed but its daemon is unavailable. Start Docker, then check again."})
        }
    }
}
struct CapabilityCache {
    at: Option<Instant>,
    value: Value,
    generation: u64,
    refreshing: bool,
}
static CAPABILITIES: OnceLock<Mutex<CapabilityCache>> = OnceLock::new();
fn capability_cache() -> &'static Mutex<CapabilityCache> {
    CAPABILITIES.get_or_init(|| Mutex::new(CapabilityCache { at:None, value:json!({"version":env!("CARGO_PKG_VERSION"),"docker":{"available":false,"ready":false,"checked":false,"message":"Checking Docker availability…"}}),generation:0,refreshing:false }))
}
fn store_capabilities(generation: u64, value: Value) {
    if let Ok(mut cache) = capability_cache().lock() {
        if cache.generation == generation {
            cache.at = Some(Instant::now());
            cache.value = value;
            cache.refreshing = false;
        }
    }
}
/// Heartbeats never wait for Docker: refresh the bounded probe in the background.
pub fn capabilities() -> Value {
    let Ok(mut cache) = capability_cache().lock() else {
        return json!({"version":env!("CARGO_PKG_VERSION"),"docker":{"available":false,"ready":false,"checked":false,"message":"Docker check unavailable."}});
    };
    if !cache.refreshing
        && cache
            .at
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
    {
        cache.refreshing = true;
        cache.generation += 1;
        let generation = cache.generation;
        std::thread::spawn(move || {
            store_capabilities(
                generation,
                json!({"version":env!("CARGO_PKG_VERSION"),"docker":docker_probe()}),
            )
        });
    }
    cache.value.clone()
}
fn refresh_capabilities() -> Value {
    let generation = if let Ok(mut cache) = capability_cache().lock() {
        cache.refreshing = true;
        cache.generation += 1;
        cache.generation
    } else {
        0
    };
    let value = json!({"version":env!("CARGO_PKG_VERSION"),"docker":docker_probe()});
    store_capabilities(generation, value.clone());
    value
}

pub(crate) fn parse_link(link: &str) -> Result<(String, String)> {
    if link.len() > 256 {
        return Err("Registration link is too long".into());
    }
    let (server, code) = link
        .trim()
        .split_once("/#pair=")
        .ok_or("Paste the complete registration link copied from the master")?;
    crate::tray::validate_dashboard(server)?;
    if code.len() != 64 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Registration link has an invalid pairing code".into());
    }
    Ok((server.to_owned(), code.to_ascii_lowercase()))
}
fn master_health(server: &str) -> Result<Value> {
    let bytes = crate::http::client_raw(server, "/health", None, "GET", "application/json", &[])?;
    serde_json::from_slice(&bytes).map_err(|_| "Master returned invalid health information".into())
}
fn require_snapshot_master(server: &str) -> Result<()> {
    let health = master_health(server)?;
    let version = health["version"]
        .as_str()
        .ok_or("Master version is unavailable; update the master")?;
    let parts: Vec<_> = version.split('.').collect();
    let supported = parts.len() == 3
        && parts.iter().all(|p| p.parse::<u32>().is_ok())
        && (parts[0].parse::<u32>().unwrap_or(0) > 0 || parts[1].parse::<u32>().unwrap_or(0) >= 6);
    if !supported {
        return Err(format!("Master {version} does not support guided worker setup. Update the master to v0.6.0 or later before registering."));
    }
    Ok(())
}
fn image_reference(image: &str) -> Result<()> {
    if image.is_empty()
        || image.len() > 200
        || image.starts_with('-')
        || !image
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._:@-".contains(&b))
    {
        return Err("Enter a valid Docker image reference".into());
    }
    Ok(())
}
fn image_present(image: &str) -> bool {
    image_reference(image).is_ok()
        && utility(
            "docker",
            &["image", "inspect", "--format", "{{.Id}}", image],
            5,
        )
        .is_ok()
}
fn preset_argv(preset: &str) -> Result<Vec<String>> {
    let values = match preset {
        "node-test" => vec!["node", "--test"],
        "npm-test" => vec!["npm", "test"],
        "playwright" => vec!["npx", "--no-install", "playwright", "test"],
        _ => return Err("Choose an available test preset".into()),
    };
    Ok(values.into_iter().map(str::to_owned).collect())
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Settings {
    preset: String,
    mode: SnapshotMode,
    #[serde(default)]
    image: String,
    #[serde(default)]
    network: SnapshotNetwork,
    workspace: PathBuf,
    #[serde(default)]
    install_dependencies: bool,
    allow_snapshots: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairInput {
    link: String,
    name: String,
    #[serde(default)]
    replace_pairing: bool,
    #[serde(flatten)]
    settings: Settings,
}
fn configure(config: &mut Config, settings: Settings) -> Result<()> {
    if !settings.allow_snapshots {
        return Err(
            "Confirm that this worker may execute uploaded source using the selected mode".into(),
        );
    }
    let mut argv = preset_argv(&settings.preset)?;
    if cfg!(windows) && settings.mode == SnapshotMode::Native {
        if argv[0] == "npm" {
            argv[0] = "npm.cmd".into();
        }
        if argv[0] == "npx" {
            argv[0] = "npx.cmd".into();
        }
    }
    if !settings.workspace.is_absolute() || settings.workspace.as_os_str().len() > 4096 {
        return Err("Workspace must be an absolute local folder".into());
    }
    if let Some(existing) = config.tasks.get(MANAGED_TASK) {
        let mut known = config
            .setup_preset
            .as_deref()
            .and_then(|p| preset_argv(p).ok());
        if cfg!(windows)
            && existing
                .snapshot
                .as_ref()
                .is_some_and(|s| s.mode == SnapshotMode::Native)
        {
            if let Some(argv) = known.as_mut() {
                if argv[0] == "npm" {
                    argv[0] = "npm.cmd".into();
                }
                if argv[0] == "npx" {
                    argv[0] = "npx.cmd".into();
                }
            }
        }
        let expected_npm = if cfg!(windows)
            && existing
                .snapshot
                .as_ref()
                .is_some_and(|s| s.mode == SnapshotMode::Native)
        {
            "npm.cmd"
        } else {
            "npm"
        };
        let known_prepare = existing.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.prepare.is_empty()
                || snapshot.prepare
                    == vec![vec![
                        expected_npm.to_owned(),
                        "ci".into(),
                        "--ignore-scripts".into(),
                    ]]
        });
        if known.as_ref() != Some(&existing.argv)
            || !known_prepare
            || existing.timeout_seconds != 600
        {
            return Err("The sharebox-tests task was configured outside setup. Rename it or edit it locally; setup will not overwrite it.".into());
        }
    }
    if settings.mode == SnapshotMode::Docker {
        image_reference(&settings.image)?;
        if refresh_capabilities()["docker"]["ready"] != true {
            return Err("Docker is not ready. Start Docker and check again; native execution is never enabled as a fallback.".into());
        }
        if !image_present(&settings.image) {
            return Err(
                "Docker image is not downloaded. Use Download image first, then save the task."
                    .into(),
            );
        }
    } else if settings.network != SnapshotNetwork::None {
        return Err("The container network setting applies only to Docker mode".into());
    }
    std::fs::create_dir_all(&settings.workspace).map_err(|e| format!("Create workspace: {e}"))?;
    let workspace = settings
        .workspace
        .canonicalize()
        .map_err(|e| format!("Open workspace: {e}"))?;
    let prepare = if settings.install_dependencies {
        vec![vec![
            if cfg!(windows) && settings.mode == SnapshotMode::Native {
                "npm.cmd".into()
            } else {
                "npm".into()
            },
            "ci".into(),
            "--ignore-scripts".into(),
        ]]
    } else {
        vec![]
    };
    if settings.mode == SnapshotMode::Docker
        && settings.install_dependencies
        && settings.network == SnapshotNetwork::None
    {
        return Err("Installing npm dependencies in Docker requires explicitly allowing container network access".into());
    }
    config.tasks.insert(
        MANAGED_TASK.into(),
        Task {
            description: format!(
                "Uploaded source: {} ({})",
                settings.preset,
                if settings.mode == SnapshotMode::Docker {
                    "Docker sandbox"
                } else {
                    "trusted native"
                }
            ),
            argv,
            cwd: workspace,
            timeout_seconds: 600,
            snapshot: Some(SnapshotTask {
                mode: settings.mode.clone(),
                image: if settings.mode == SnapshotMode::Docker {
                    Some(settings.image)
                } else {
                    None
                },
                prepare,
                network: settings.network,
            }),
        },
    );
    config.setup_preset = Some(settings.preset);
    crate::worker::validate(config)
}

fn config_path(input: Option<PathBuf>) -> Result<PathBuf> {
    let path = if let Some(path) = input {
        path
    } else if let Some(path) = crate::service::worker_registration_config()? {
        path
    } else {
        PathBuf::from(
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .ok_or("Home directory is unavailable; supply --config")?,
        )
        .join(".sharebox")
        .join("worker.json")
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path))
    }
}
struct Setup {
    config: PathBuf,
    origin: String,
    token: String,
    closing: std::sync::atomic::AtomicBool,
}
impl Setup {
    fn service_match(&self) -> Result<Option<PathBuf>> {
        let registered = crate::service::worker_registration_config()?;
        if let Some(path) = &registered {
            if path.canonicalize().ok() != self.config.canonicalize().ok() {
                return Err("A different worker configuration is registered as this account's service. Open setup for that configuration instead.".into());
            }
        }
        Ok(registered)
    }
    fn guard_recent_foreground(&self) -> Result<()> {
        let registered = crate::service::worker_registration_config()?;
        if registered
            .as_ref()
            .is_some_and(|p| p.canonicalize().ok() == self.config.canonicalize().ok())
        {
            return Ok(());
        }
        if !self.config.exists() {
            return Ok(());
        }
        let config = crate::worker::load_config(&self.config)?;
        if config.token.is_empty() || config.worker_id.is_empty() {
            return Ok(());
        }
        if let Ok(status) = crate::http::client_request(
            &config.server,
            "/api/worker/status",
            Some(&config.token),
            &json!({}),
        ) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if status["lastPollAt"]
                .as_u64()
                .is_some_and(|last| last > 0 && now.saturating_sub(last) < 30000)
            {
                return Err("The master recently heard from this worker outside the matching service. Stop its foreground process, then wait 30 seconds before changing settings or starting a service.".into());
            }
        }
        Ok(())
    }
    fn ensure_stopped(&self) -> Result<()> {
        self.guard_recent_foreground()?;
        if crate::worker::config_running(&self.config)? {
            return Err("This worker configuration is running. Finish active tests and stop its service or foreground process before changing settings.".into());
        }
        if let Some(registered) = crate::service::worker_registration_config()? {
            if registered.canonicalize().ok() == self.config.canonicalize().ok() {
                if !crate::service::worker_registration_uses_current_executable()? {
                    return Err("The worker service uses another Sharebox executable. Update that installed binary and open its setup before changing this configuration.".into());
                }
                if crate::service::worker_service_running()? {
                    return Err("Stop the worker service before changing its task settings. Finish or cancel active tests first.".into());
                }
            }
        }
        Ok(())
    }
    fn status(&self) -> Value {
        let config = if self.config.exists() {
            crate::worker::load_config(&self.config)
        } else {
            Err("Worker is not configured yet".into())
        };
        let registration = crate::service::worker_registration_config();
        let matched = registration.as_ref().is_ok_and(|path| {
            path.as_ref()
                .is_some_and(|p| p.canonicalize().ok() == self.config.canonicalize().ok())
        });
        let conflict = registration.as_ref().is_ok_and(|path| path.is_some()) && !matched;
        let executable_error = if matched {
            match crate::service::worker_registration_uses_current_executable(){Ok(true)=>None,Ok(false)=>Some("The service uses another Sharebox executable. Update the installed binary and open its setup.".to_owned()),Err(error)=>Some(error)}
        } else {
            None
        };
        let running = if matched {
            crate::service::worker_service_running()
        } else {
            Ok(false)
        };
        let process_running = crate::worker::config_running(&self.config);
        let mut result = json!({"configPath":self.config,"configExists":self.config.exists(),"paired":false,"tasks":[],"capabilities":capabilities(),"service":{"running":running.as_ref().copied().unwrap_or(false),"processRunning":process_running.as_ref().copied().unwrap_or(false),"registered":matched,"conflict":conflict,"error":executable_error.or_else(||registration.err()).or_else(||running.err()).or_else(||process_running.err())},"defaultName":std::env::var("COMPUTERNAME").or_else(|_|std::env::var("HOSTNAME")).unwrap_or_else(|_|format!("{}-worker",std::env::consts::OS)),"defaultWorkspace":self.config.parent().unwrap_or(Path::new(".")).join("workspaces"),"master":{"reachable":false,"authenticated":false,"message":"Not paired"}});
        match config {
            Ok(config) => {
                let paired = !config.worker_id.is_empty() && !config.token.is_empty();
                result["paired"] = json!(paired);
                result["name"] = json!(config.name);
                result["server"] = json!(config.server);
                result["workerId"] = json!(config.worker_id);
                result["tasks"] = json!(crate::worker::descriptors(&config));
                result["setupPreset"] = json!(config.setup_preset);
                if let Some(task) = config.tasks.get(MANAGED_TASK) {
                    result["managedTask"] = json!({"preset":config.setup_preset,"workspace":task.cwd,"snapshot":task.snapshot});
                }
                if paired {
                    match master_health(&config.server) {
                        Ok(health) => {
                            result["master"]["reachable"] = json!(true);
                            result["master"]["version"] = health["version"].clone();
                            match crate::http::client_request(
                                &config.server,
                                "/api/worker/status",
                                Some(&config.token),
                                &json!({}),
                            ) {
                                Ok(status) => {
                                    result["master"]["authenticated"] = json!(true);
                                    result["master"]["message"] =
                                        json!("Registration accepted by master");
                                    result["master"]["worker"] = status;
                                }
                                Err(error) => {
                                    result["master"]["message"] = json!(if error
                                        .starts_with("HTTP 401")
                                    {
                                        "Pairing is revoked or no longer recognized"
                                    } else {
                                        "Worker status unavailable; update the master or check connectivity"
                                    });
                                }
                            }
                        }
                        Err(_) => {
                            result["master"]["message"] = json!(
                                "Cannot reach the master. Check the LAN address and firewall."
                            )
                        }
                    }
                }
            }
            Err(error) => result["configError"] = json!(error),
        }
        result
    }
    fn handle(&self, req: Request) -> Response {
        if req.headers.get("host").map(String::as_str) != Some(&self.origin[7..]) {
            return Response::error(403, "Unexpected local setup host");
        }
        if req.method == "GET" {
            match req.path.as_str() {
                "/" => {
                    return Response::bytes(
                        "text/html; charset=utf-8",
                        include_bytes!("setup_page.html").to_vec(),
                    )
                }
                "/setup.js" => {
                    return Response::bytes(
                        "text/javascript; charset=utf-8",
                        include_bytes!("setup_page.js").to_vec(),
                    )
                }
                "/setup.css" => {
                    return Response::bytes(
                        "text/css; charset=utf-8",
                        [
                            include_bytes!("../public/style.css").as_slice(),
                            include_bytes!("setup_page.css").as_slice(),
                        ]
                        .concat(),
                    )
                }
                _ => {}
            }
        }
        if req.headers.get("authorization").map(String::as_str)
            != Some(&format!("Bearer {}", self.token))
        {
            return Response::error(401, "Open the private setup link from this machine");
        }
        if req
            .headers
            .get("origin")
            .is_some_and(|origin| origin != &self.origin)
            || (req.method != "GET" && req.headers.get("origin") != Some(&self.origin))
        {
            return Response::error(403, "Setup requests must originate from this local page");
        }
        if req.method == "GET" && req.path == "/api/setup/ping" {
            return Response::json(200, json!({"configPath":self.config}));
        }
        if req.method == "GET" && req.path == "/api/setup/status" {
            return Response::json(200, self.status());
        }
        if req.method != "POST" {
            return Response::error(405, "Use POST for setup actions");
        }
        if req.body.len() > 16384 {
            return Response::error(413, "Setup request exceeds 16 KiB");
        }
        if req
            .headers
            .get("content-type")
            .is_none_or(|t| t.split(';').next() != Some("application/json"))
        {
            return Response::error(400, "Use application/json");
        }
        let result: Result<Value> = (|| match req.path.as_str() {
            "/api/setup/close" => {
                let _: Empty = decode(&req.body)?;
                self.closing
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(json!({"ok":true}))
            }
            "/api/setup/check-docker" => {
                let _: Empty = decode(&req.body)?;
                Ok(refresh_capabilities())
            }
            "/api/setup/pull-image" => {
                let input: ImageInput = decode(&req.body)?;
                image_reference(&input.image)?;
                if refresh_capabilities()["docker"]["ready"] != true {
                    return Err("Start Docker before downloading an image".into());
                }
                utility("docker", &["pull", &input.image], 300)?;
                if !image_present(&input.image) {
                    return Err("Docker did not report the downloaded image".into());
                }
                Ok(json!({"ok":true,"image":input.image}))
            }
            "/api/setup/pair" => {
                self.ensure_stopped()?;
                let input: PairInput = decode(&req.body)?;
                let (server, code) = parse_link(&input.link)?;
                let mut config = if self.config.exists() {
                    crate::worker::load_config(&self.config)?
                } else {
                    Config {
                        server: server.clone(),
                        token: String::new(),
                        worker_id: String::new(),
                        name: String::new(),
                        poll_seconds: 3,
                        tasks: BTreeMap::new(),
                        setup_preset: None,
                    }
                };
                let already_paired = !config.token.is_empty() || !config.worker_id.is_empty();
                if already_paired && !input.replace_pairing {
                    return Err("This configuration is already paired. Select Replace pairing to use a new registration link; other tasks remain unchanged.".into());
                }
                if input.name.trim().is_empty()
                    || input.name.len() > 100
                    || input.name.chars().any(char::is_control)
                {
                    return Err("Worker name must contain 1 to 100 printable characters".into());
                }
                require_snapshot_master(&server)?;
                config.server = server;
                config.name = input.name.trim().to_owned();
                configure(&mut config, input.settings)?;
                // Never send a previous master's credential to the new master.
                config.token.clear();
                config.worker_id.clear();
                if !already_paired {
                    crate::worker::private_write(
                        &self.config,
                        &serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?,
                    )?;
                }
                let config = crate::worker::pair_config(&self.config, config, &code)?;
                Ok(
                    json!({"ok":true,"paired":true,"workerId":config.worker_id,"task":MANAGED_TASK,"message":"Paired and saved. Install or start the worker service when ready."}),
                )
            }
            "/api/setup/task" => {
                self.ensure_stopped()?;
                let settings: Settings = decode(&req.body)?;
                let mut config = crate::worker::load_config(&self.config)?;
                require_snapshot_master(&config.server)?;
                configure(&mut config, settings)?;
                crate::worker::private_write(
                    &self.config,
                    &serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?,
                )?;
                Ok(
                    json!({"ok":true,"message":"Snapshot task saved. Other tasks and pairing are unchanged. Start the worker to advertise the new task."}),
                )
            }
            "/api/setup/service" => {
                let input: ServiceInput = decode(&req.body)?;
                let registered = self.service_match()?;
                if input.action != "stop" {
                    self.guard_recent_foreground()?;
                }
                if input.action != "stop"
                    && registered.is_some()
                    && !crate::service::worker_registration_uses_current_executable()?
                {
                    return Err("The service uses another Sharebox executable. Update the installed binary first.".into());
                }
                if input.action != "stop" && crate::worker::config_running(&self.config)? {
                    return Err("This worker is already running. Stop its foreground process before installing or starting a service.".into());
                }
                match input.action.as_str() {
                        "install" if registered.is_none()=>crate::service::run(vec!["install".into(),"worker".into(),"--config".into(),self.config.to_string_lossy().into_owned()])?,
                        "start" if registered.is_some()=>crate::service::service_action("start","worker")?,
                        "stop" if registered.is_some()=>crate::service::service_action("stop","worker")?,
                        _=>return Err("Use Install service for a new worker, or Start/Stop for its existing service".into()),
                    }
                Ok(json!({"ok":true}))
            }
            _ => Err("Unknown setup action".into()),
        })();
        match result {
            Ok(value) => Response::json(200, value),
            Err(error) => Response::error(400, &error),
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageInput {
    image: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceInput {
    action: String,
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| "Invalid setup request".into())
}
fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("/usr/bin/open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    return Err("Open the printed setup URL in a browser".into());
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
        command
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "Could not open a browser; use the printed setup link")?;
        Ok(())
    }
}
struct SetupLock(std::fs::File);
impl Drop for SetupLock {
    fn drop(&mut self) {
        let _ = &self.0;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}
fn setup_lock(path: &Path) -> Result<SetupLock> {
    let parent = path
        .parent()
        .ok_or("Configuration must have a parent folder")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let lock = path.with_file_name(format!(
        "{}.setup.lock",
        path.file_name()
            .ok_or("Invalid configuration filename")?
            .to_string_lossy()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let file = options.open(lock).map_err(|_| {
        "Worker setup is already open for this configuration, or its folder is not writable"
    })?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("Worker setup is already open for this configuration".into());
        }
    }
    Ok(SetupLock(file))
}
fn session_path(config: &Path) -> PathBuf {
    config.with_file_name(format!(
        "{}.setup-session.json",
        config.file_name().unwrap_or_default().to_string_lossy()
    ))
}
struct SessionFile(PathBuf);
impl Drop for SessionFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn reopen(config: &Path, no_open: bool) -> Result<()> {
    let path = session_path(config);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|_| "Setup is already opening. Try again in a moment.")?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 4096 {
        return Err("Invalid existing setup session file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err("Setup session file must be private to its owner".into());
        }
    }
    let bytes = std::fs::read(path).map_err(|_| "Cannot read existing setup session")?;
    let session: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid setup session file")?;
    let url = session["url"]
        .as_str()
        .ok_or("Invalid setup session link")?;
    let (origin, token) = url
        .split_once("/#token=")
        .ok_or("Invalid setup session link")?;
    let address = origin
        .strip_prefix("http://")
        .and_then(|value| value.parse::<std::net::SocketAddrV4>().ok())
        .ok_or("Invalid setup session address")?;
    if *address.ip() != Ipv4Addr::LOCALHOST
        || address.port() == 0
        || token.len() != 64
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("Invalid setup session address".into());
    }
    let reply = crate::http::client_raw(
        origin,
        "/api/setup/ping",
        Some(token),
        "GET",
        "application/json",
        &[],
    )?;
    let response: Value =
        serde_json::from_slice(&reply).map_err(|_| "Invalid existing setup response")?;
    let returned = PathBuf::from(
        response["configPath"]
            .as_str()
            .ok_or("Existing setup did not identify its configuration")?,
    );
    if returned != config {
        return Err("Existing setup has a different configuration".into());
    }
    println!("Worker setup: {url}");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    if !no_open {
        open_browser(url)?;
    }
    Ok(())
}
pub fn run(args: Vec<String>) -> Result<()> {
    if args == ["--help"] || args == ["-h"] {
        println!("{HELP}");
        return Ok(());
    }
    let mut config = None;
    let mut no_open = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" if config.is_none() => {
                config = Some(PathBuf::from(args.next().ok_or("Missing --config path")?))
            }
            "--no-open" if !no_open => no_open = true,
            _ => return Err(HELP.into()),
        }
    }
    let config = config_path(config)?;
    std::fs::create_dir_all(config.parent().ok_or("Invalid config path")?)
        .map_err(|e| e.to_string())?;
    let config = if config.exists() {
        config.canonicalize().map_err(|e| e.to_string())?
    } else {
        config
            .parent()
            .ok_or("Invalid config path")?
            .canonicalize()
            .map_err(|e| e.to_string())?
            .join(config.file_name().ok_or("Invalid config path")?)
    };
    let _lock = match setup_lock(&config) {
        Ok(lock) => lock,
        Err(error) => {
            return reopen(&config, no_open)
                .map_err(|reopen_error| format!("{error}. {reopen_error}"))
        }
    };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    let origin = format!(
        "http://{}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    let setup = Setup {
        config,
        origin: origin.clone(),
        token: nonce()?,
        closing: std::sync::atomic::AtomicBool::new(false),
    };
    let url = format!("{origin}/#token={}", setup.token);
    let _session = SessionFile(session_path(&setup.config));
    crate::worker::private_write(
        &_session.0,
        &serde_json::to_vec(&json!({"url":url})).map_err(|e| e.to_string())?,
    )?;
    println!("Worker setup: {url}");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    if !no_open {
        if let Err(error) = open_browser(&url) {
            eprintln!("{error}");
        }
    }
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let mut last_activity = Instant::now();
    while last_activity.elapsed() < Duration::from_secs(15 * 60)
        && !setup.closing.load(std::sync::atomic::Ordering::Relaxed)
    {
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        if !stream.peer_addr().is_ok_and(|peer| peer.ip().is_loopback()) {
            continue;
        }
        let response = match crate::http::read_request(&mut stream) {
            Ok(req) => {
                if req.headers.get("authorization") == Some(&format!("Bearer {}", setup.token)) {
                    last_activity = Instant::now();
                }
                setup.handle(req)
            }
            Err(_) => Response::error(400, "Invalid setup HTTP request"),
        };
        crate::http::write_response(&mut stream, response);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_links_are_private_origins_with_one_exact_code() {
        let code = "a".repeat(64);
        assert_eq!(
            parse_link(&format!("http://192.168.1.2:7331/#pair={code}"))
                .unwrap()
                .1,
            code
        );
        for url in [
            format!("https://192.168.1.2:7331/#pair={code}"),
            format!("http://8.8.8.8:7331/#pair={code}"),
            format!("http://user@127.0.0.1:7331/#pair={code}"),
            format!("http://127.0.0.1:7331/#pair={code}&mode=native"),
            "http://127.0.0.1:7331/#pair=bad".into(),
        ] {
            assert!(parse_link(&url).is_err());
        }
    }
    #[test]
    fn presets_never_accept_remote_commands() {
        assert_eq!(preset_argv("node-test").unwrap(), ["node", "--test"]);
        assert!(preset_argv("node --eval evil").is_err());
        for bad in [
            "--privileged",
            "node:latest;whoami",
            "node image",
            "",
            "$(shell)",
        ] {
            assert!(image_reference(bad).is_err());
        }
    }
    #[test]
    fn setup_requires_host_token_and_same_origin() {
        let setup = Setup {
            config: PathBuf::from("unused"),
            origin: "http://127.0.0.1:1234".into(),
            token: "test".into(),
            closing: std::sync::atomic::AtomicBool::new(false),
        };
        let make = |host: &str, token: &str, origin: &str| Request {
            method: "POST".into(),
            path: "/api/setup/nope".into(),
            headers: std::collections::HashMap::from([
                ("host".into(), host.into()),
                ("authorization".into(), token.into()),
                ("origin".into(), origin.into()),
                ("content-type".into(), "application/json".into()),
            ]),
            body: b"{}".to_vec(),
        };
        assert_eq!(
            setup
                .handle(make("evil.test", "Bearer test", "http://127.0.0.1:1234"))
                .status,
            403
        );
        assert_eq!(
            setup
                .handle(make(
                    "127.0.0.1:1234",
                    "Bearer wrong",
                    "http://127.0.0.1:1234"
                ))
                .status,
            401
        );
        assert_eq!(
            setup
                .handle(make("127.0.0.1:1234", "Bearer test", "http://evil.test"))
                .status,
            403
        );
    }
}
