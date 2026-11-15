//! Per-user background startup. Never installs elevated/system services.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, String>;

pub const HELP: &str = "sharebox service install master [--bind 0.0.0.0:7331] [--data .sharebox]\nsharebox service install worker [--config worker.json]\nsharebox service start|stop|status|uninstall master|worker\n\nInstall starts immediately and enables startup at user login. Keep the executable at its installed path.\nWindows uses user Scheduled Tasks, macOS LaunchAgents, Linux a systemd user unit.\nWindows/macOS also start a tray icon for each installed role; closing the icon leaves the service running.\nPair a worker in the foreground first. Stop foreground instances before installing.\nUninstall preserves data and worker credentials. Stop/cancel active jobs before stopping a worker.";

#[derive(Debug)]
struct Spec {
    role: String,
    exe: String,
    args: Vec<String>,
    cwd: String,
    path: String,
    dashboard: String,
}

fn safe(value: &str) -> Result<&str> {
    if value.chars().any(char::is_control) {
        return Err("Service values cannot contain control characters".into());
    }
    Ok(value)
}
fn path_text(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or("Service paths must be valid Unicode")?;
    safe(text)?;
    Ok(text.into())
}

fn parse(args: Vec<String>) -> Result<(String, String, HashMap<String, String>)> {
    let mut args = args.into_iter();
    let action = args.next().ok_or(HELP)?;
    if !["install", "start", "stop", "status", "uninstall"].contains(&action.as_str()) {
        return Err(HELP.into());
    }
    let role = args.next().ok_or(HELP)?;
    if !["master", "worker"].contains(&role.as_str()) {
        return Err("Service role must be master or worker".into());
    }
    let mut opts = HashMap::new();
    while let Some(key) = args.next() {
        let allowed = action == "install"
            && ((role == "master" && ["--bind", "--data"].contains(&key.as_str()))
                || (role == "worker" && key == "--config"));
        if !allowed {
            return Err(format!("Unsupported service option: {key}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("Missing value for {key}"))?;
        safe(&value)?;
        if opts.insert(key.clone(), value).is_some() {
            return Err(format!("Duplicate option: {key}"));
        }
    }
    Ok((action, role, opts))
}

fn spec(role: &str, opts: &HashMap<String, String>) -> Result<Spec> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut args = vec![role.into()];
    let dashboard;
    if role == "master" {
        let bind = opts
            .get("--bind")
            .map(String::as_str)
            .unwrap_or("0.0.0.0:7331");
        let mut address = bind
            .parse::<std::net::SocketAddr>()
            .map_err(|_| "--bind must be an IP address and port, e.g. 0.0.0.0:7331")?;
        if address.port() == 0 {
            return Err("Service --bind must use a fixed, nonzero port".into());
        }
        if address.ip().is_unspecified() {
            address.set_ip(if address.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        dashboard = format!("http://{address}");
        let data = PathBuf::from(
            opts.get("--data")
                .map(String::as_str)
                .unwrap_or(".sharebox"),
        );
        std::fs::create_dir_all(&data).map_err(|e| format!("Create master data directory: {e}"))?;
        let data = data.canonicalize().map_err(|e| e.to_string())?;
        args.extend([
            "--bind".into(),
            bind.into(),
            "--data".into(),
            path_text(&data)?,
        ]);
        println!(
            "Master data: {}\nAdmin token: {}",
            data.display(),
            data.join("admin-token").display()
        );
    } else {
        let config = PathBuf::from(
            opts.get("--config")
                .map(String::as_str)
                .unwrap_or("worker.json"),
        )
        .canonicalize()
        .map_err(|e| format!("Open paired worker config: {e}"))?;
        let file = std::fs::File::open(&config).map_err(|e| e.to_string())?;
        use std::io::Read;
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 1024 * 1024 {
            return Err("Worker config exceeds 1 MiB".into());
        }
        let config_value: crate::worker::Config =
            serde_json::from_slice(&bytes).map_err(|e| format!("Invalid worker config: {e}"))?;
        if config_value.token.is_empty() || config_value.worker_id.is_empty() {
            return Err("Pair this worker in the foreground before installing its service".into());
        }
        dashboard = config_value.server.trim_end_matches('/').to_owned();
        args.extend(["--config".into(), path_text(&config)?]);
    }
    #[cfg(any(windows, target_os = "macos"))]
    crate::tray::validate_dashboard(&dashboard)?;
    let path = std::env::var("PATH").map_err(|_| "PATH must be set and valid Unicode")?;
    safe(&path)?;
    Ok(Spec {
        role: role.into(),
        exe: path_text(&exe)?,
        args,
        cwd: path_text(&cwd)?,
        path,
        dashboard,
    })
}

pub fn run(args: Vec<String>) -> Result<()> {
    if args.is_empty() || args == ["--help"] || args == ["-h"] || args == ["help"] {
        println!("{HELP}");
        return Ok(());
    }
    let (action, role, opts) = parse(args)?;
    let spec = if action == "install" {
        Some(spec(&role, &opts)?)
    } else {
        None
    };
    if action == "install" {
        install(spec.as_ref().ok_or("Missing service specification")?)?;
    } else {
        service_action(&action, &role)?;
    }
    if action == "install" {
        println!("Dashboard: {}", spec.as_ref().unwrap().dashboard);
        println!("Registered {role} for startup at user login and requested start. Run `sharebox service status {role}` to check its state.");
    }
    Ok(())
}

fn valid_role(role: &str) -> Result<()> {
    if matches!(role, "master" | "worker") {
        Ok(())
    } else {
        Err("Service role must be master or worker".into())
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn companion(spec: &Spec) -> Spec {
    Spec {
        role: format!("tray.{}", spec.role),
        exe: spec.exe.clone(),
        args: vec![
            "tray".into(),
            "--role".into(),
            spec.role.clone(),
            "--dashboard".into(),
            spec.dashboard.clone(),
        ],
        cwd: spec.cwd.clone(),
        path: spec.path.clone(),
        dashboard: spec.dashboard.clone(),
    }
}

fn install(spec: &Spec) -> Result<()> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        let tray = companion(spec);
        if registered(&spec.role)? || registered(&tray.role)? {
            return Err("A service or tray registration already exists. Uninstall this role before installing again; data and worker credentials are preserved.".into());
        }
        platform("install", &spec.role, Some(spec))?;
        if let Err(error) = platform("install", &tray.role, Some(&tray)) {
            let rollback = platform("uninstall", &spec.role, None);
            return Err(format!(
                "Tray registration failed: {error}. Background service rollback: {}",
                rollback.err().unwrap_or_else(|| "completed".into())
            ));
        }
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    platform("install", &spec.role, Some(spec))
}

/// Called off the UI thread. Starting a role also restores its companion icon;
/// stopping keeps the icon available so the owner can start the role again.
pub fn service_action(action: &str, role: &str) -> Result<()> {
    valid_role(role)?;
    if !matches!(action, "start" | "stop" | "status" | "uninstall") {
        return Err("Unsupported service action".into());
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let tray = format!("tray.{role}");
        if action == "uninstall" && registered(&tray)? {
            platform("uninstall", &tray, None)?;
        }
        // launchctl bootstrap/bootout are not themselves idempotent.
        if (action != "start" || !is_running(role)?) && (action != "stop" || loaded(role)?) {
            platform(action, role, None)?;
        }
        if action == "start" && registered(&tray)? && !running_native(&tray)? {
            platform("start", &tray, None)?;
        }
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    platform(action, role, None)
}

#[cfg(any(windows, target_os = "macos"))]
fn bounded_output(program: &str, args: &[&str]) -> Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().map_err(|e| format!("Run {program}: {e}"))?;
    let stdout = child.stdout.take().ok_or("Missing service stdout")?;
    let stderr = child.stderr.take().ok_or("Missing service stderr")?;
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stream = stdout;
        let _ = stream.by_ref().take(16384).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stream, &mut std::io::sink());
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stream = stderr;
        let _ = stream.by_ref().take(16384).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stream, &mut std::io::sink());
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(std::process::Output {
                status,
                stdout: out.join().unwrap_or_default(),
                stderr: err.join().unwrap_or_default(),
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program} timed out after 15 seconds"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "macos")]
fn mac_loaded(role: &str) -> Result<Option<String>> {
    let target = format!("gui/{}/org.ossmalaysia.sharebox.{role}", unsafe {
        libc::geteuid()
    });
    let output = bounded_output("launchctl", &["print", &target])?;
    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    if matches!(output.status.code(), Some(113) | Some(3)) {
        return Ok(None);
    }
    Err(format!(
        "Cannot inspect service: {}",
        String::from_utf8_lossy(&output.stderr)
    ))
}
#[cfg(target_os = "macos")]
fn registered(role: &str) -> Result<bool> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("org.ossmalaysia.sharebox.{role}.plist"))
        .exists()
        || mac_loaded(role)?.is_some())
}
#[cfg(target_os = "macos")]
fn loaded(role: &str) -> Result<bool> {
    Ok(mac_loaded(role)?.is_some())
}

#[cfg(windows)]
fn task_state(role: &str) -> Result<String> {
    let script = format!("$ErrorActionPreference = 'Stop'; $name = {} + [Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $task = Get-ScheduledTask | Where-Object {{ $_.TaskName -eq $name }}; if ($null -eq $task) {{ Write-Output 'Missing' }} else {{ Write-Output $task.State }}", ps(&format!("Sharebox-{}-", role.replace('.', "-"))));
    let output = bounded_output(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ],
    )?;
    if !output.status.success() {
        return Err(format!(
            "Cannot inspect scheduled task: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}
#[cfg(windows)]
fn registered(role: &str) -> Result<bool> {
    Ok(task_state(role)? != "Missing")
}
#[cfg(windows)]
fn loaded(role: &str) -> Result<bool> {
    Ok(task_state(role)? == "Running")
}

/// Whether the OS service manager reports this role's process as running.
#[cfg(any(windows, target_os = "macos"))]
pub fn is_running(role: &str) -> Result<bool> {
    valid_role(role)?;
    running_native(role)
}

#[cfg(any(windows, target_os = "macos"))]
fn running_native(role: &str) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        Ok(mac_loaded(role)?
            .is_some_and(|text| text.lines().any(|line| line.trim() == "state = running")))
    }
    #[cfg(windows)]
    {
        Ok(task_state(role)? == "Running")
    }
}

fn command(program: &str, args: &[&str]) -> Result<()> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        let output = bounded_output(program, args)?;
        if output.status.success() {
            if args.contains(&"print") || args.iter().any(|arg| arg.contains("Select-Object")) {
                print!("{}", String::from_utf8_lossy(&output.stdout));
            }
            return Ok(());
        }
        Err(format!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let status = Command::new(program)
            .args(args)
            .status()
            .map_err(|e| format!("Run {program}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "{program} failed ({status}); inspect the service manager's output above"
            ))
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn home() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    if !home.is_absolute() {
        return Err("HOME must be an absolute path".into());
    }
    Ok(home)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_new(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().ok_or("Invalid service file path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            format!(
                "Create {}: {e}. Uninstall an existing service before changing its configuration.",
                path.display()
            )
        })?;
    if let Err(error) = file
        .write_all(contents.as_bytes())
        .and_then(|_| file.sync_all())
    {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
#[cfg(any(target_os = "macos", test))]
fn plist(spec: &Spec) -> String {
    let args = std::iter::once(&spec.exe)
        .chain(spec.args.iter())
        .map(|arg| format!("<string>{}</string>", xml(arg)))
        .collect::<String>();
    let keep_alive = if spec.role.starts_with("tray.") {
        "false"
    } else {
        "true"
    };
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>org.ossmalaysia.sharebox.{}</string>\n<key>ProgramArguments</key><array>{args}</array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><{keep_alive}/>\n<key>ThrottleInterval</key><integer>10</integer>\n<key>StandardOutPath</key><string>/dev/null</string>\n<key>StandardErrorPath</key><string>/dev/null</string>\n</dict></plist>\n", spec.role, xml(&spec.cwd), xml(&spec.path))
}

#[cfg(target_os = "macos")]
fn platform(action: &str, role: &str, spec: Option<&Spec>) -> Result<()> {
    let label = format!("org.ossmalaysia.sharebox.{role}");
    let file = home()?
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    let file_text = path_text(&file)?;
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let target = format!("{domain}/{label}");
    match action {
        "install" => {
            write_new(&file, &plist(spec.ok_or("Missing service specification")?))?;
            if let Err(error) = command("launchctl", &["bootstrap", &domain, &file_text]) {
                let _ = std::fs::remove_file(&file);
                return Err(error);
            }
            Ok(())
        }
        "start" => {
            if loaded(role)? {
                command("launchctl", &["kickstart", &target])
            } else {
                command("launchctl", &["bootstrap", &domain, &file_text])
            }
        }
        "stop" => command("launchctl", &["bootout", &target]),
        "status" => command("launchctl", &["print", &target]),
        "uninstall" => {
            if loaded(role)? {
                command("launchctl", &["bootout", &target])?;
            }
            std::fs::remove_file(&file).map_err(|e| format!("Remove {}: {e}", file.display()))
        }
        _ => Err("Unsupported service action".into()),
    }
}

#[cfg(any(target_os = "linux", test))]
fn unit_quote(value: &str, exec: bool) -> String {
    let value = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!(
        "\"{}\"",
        if exec {
            value.replace('$', "$$")
        } else {
            value
        }
    )
}
#[cfg(any(target_os = "linux", test))]
fn unit_working_directory(value: &str) -> String {
    // WorkingDirectory is a scalar path: unlike ExecStart/Environment, systemd
    // neither removes quotes nor unescapes backslashes. Escape specifiers only.
    // The final /. preserves trailing whitespace and prevents a final backslash
    // from being interpreted as an INI line continuation.
    format!("{}/.", value.replace('%', "%%"))
}
#[cfg(any(target_os = "linux", test))]
fn unit(spec: &Spec) -> String {
    let args = std::iter::once(&spec.exe)
        .chain(spec.args.iter())
        .map(|arg| unit_quote(arg, true))
        .collect::<Vec<_>>()
        .join(" ");
    format!("[Unit]\nDescription=Sharebox {} (trusted LAN)\n\n[Service]\nType=simple\nWorkingDirectory={}\nEnvironment={}\nExecStart={args}\nRestart=on-failure\nRestartSec=10\nTimeoutStopSec=15\nKillMode=control-group\nStandardOutput=null\nStandardError=journal\n\n[Install]\nWantedBy=default.target\n", spec.role, unit_working_directory(&spec.cwd), unit_quote(&format!("PATH={}", spec.path), false))
}
#[cfg(target_os = "linux")]
fn platform(action: &str, role: &str, spec: Option<&Spec>) -> Result<()> {
    let dir = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(path) if Path::new(&path).is_absolute() => PathBuf::from(path),
        _ => home()?.join(".config"),
    };
    let name = format!("sharebox-{role}.service");
    let file = dir.join("systemd/user").join(&name);
    match action {
        "install" => {
            write_new(&file, &unit(spec.ok_or("Missing service specification")?))?;
            command("systemctl", &["--user", "daemon-reload"])?;
            command("systemctl", &["--user", "enable", "--now", &name])
        }
        "start" | "stop" | "status" => {
            command("systemctl", &["--user", "--no-pager", action, &name])
        }
        "uninstall" => {
            command("systemctl", &["--user", "disable", "--now", &name])?;
            std::fs::remove_file(&file).map_err(|e| format!("Remove {}: {e}", file.display()))?;
            command("systemctl", &["--user", "daemon-reload"])
        }
        _ => Err("Unsupported service action".into()),
    }
}

#[cfg(any(windows, test))]
fn ps(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
#[cfg(any(windows, test))]
fn windows_arg(value: &str) -> String {
    // CommandLineToArgvW/CRT quoting, including trailing backslashes before quote.
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in value.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
        } else {
            out.push_str(&"\\".repeat(slashes));
        }
        slashes = 0;
        out.push(c);
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}
#[cfg(any(windows, test))]
fn windows_script(action: &str, role: &str, spec: Option<&Spec>) -> Result<String> {
    let task_role = role.replace('.', "-");
    let mut script = format!("$ErrorActionPreference = 'Stop'; $identity = [Security.Principal.WindowsIdentity]::GetCurrent(); $taskName = 'Sharebox-{task_role}-' + $identity.User.Value; ");
    match action {
        "install" => {
            let spec = spec.ok_or("Missing service specification")?;
            // Hidden PowerShell host keeps the console executable out of the desktop.
            // No persistent output files or secrets in the scheduled task definition.
            let invocation = format!("$env:PATH = {}; & {} {} *> $null; exit $LASTEXITCODE", ps(&spec.path), ps(&spec.exe), spec.args.iter().map(|a| ps(a)).collect::<Vec<_>>().join(" "));
            let host_args = format!("-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -Command {}", windows_arg(&invocation));
            script.push_str(&format!("$action = New-ScheduledTaskAction -Execute (Join-Path $PSHOME 'powershell.exe') -Argument {} -WorkingDirectory {}; $trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity.Name; $principal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType Interactive -RunLevel Limited; $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries; $registered = $false; try {{ Register-ScheduledTask -TaskName $taskName -Description {} -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null; $registered = $true; Start-ScheduledTask -TaskName $taskName; }} catch {{ if ($registered) {{ Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue; }} throw; }}", ps(&host_args), ps(&spec.cwd), ps(&format!("Sharebox {}: starts at user login", spec.role))));
        }
        "start" => script.push_str("Start-ScheduledTask -TaskName $taskName;"),
        "stop" => script.push_str("Stop-ScheduledTask -TaskName $taskName;"),
        "status" => script.push_str("Get-ScheduledTask -TaskName $taskName | Select-Object TaskName, State; Get-ScheduledTaskInfo -TaskName $taskName | Select-Object LastRunTime, LastTaskResult;"),
        "uninstall" => script.push_str("Stop-ScheduledTask -TaskName $taskName; Unregister-ScheduledTask -TaskName $taskName -Confirm:$false;"),
        _ => return Err("Unsupported service action".into()),
    }
    Ok(script)
}
#[cfg(windows)]
fn platform(action: &str, role: &str, spec: Option<&Spec>) -> Result<()> {
    command(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &windows_script(action, role, spec)?,
        ],
    )
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform(_: &str, _: &str, _: Option<&Spec>) -> Result<()> {
    Err("Service mode supports Windows, macOS, and Linux only".into())
}

#[cfg(target_os = "macos")]
fn update_registration_matches(role: &str, target: &Path) -> Result<bool> {
    let file = home()?
        .join("Library/LaunchAgents")
        .join(format!("org.ossmalaysia.sharebox.{role}.plist"));
    if !file.is_file() {
        return Ok(false);
    }
    let output = bounded_output(
        "/usr/bin/plutil",
        &["-convert", "json", "-o", "-", &path_text(&file)?],
    )?;
    if !output.status.success() {
        return Err("Cannot inspect registered executable".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "Invalid service registration")?;
    let exe = value
        .get("ProgramArguments")
        .and_then(|a| a.get(0))
        .and_then(|x| x.as_str())
        .ok_or("Service registration has no executable")?;
    Ok(Path::new(exe).canonicalize().map_err(|e| e.to_string())? == target)
}
#[cfg(windows)]
fn update_registration_matches(role: &str, target: &Path) -> Result<bool> {
    let needle = format!("& {} ", ps(&path_text(target)?));
    // Registrations produced by this executable use exactly one PowerShell action.
    // Canonical Windows paths may carry a verbatim prefix absent from older tasks.
    let plain = target
        .to_string_lossy()
        .trim_start_matches("\\\\?\\")
        .to_owned();
    let other = format!("& {} ", ps(&plain));
    let script=format!("$ErrorActionPreference='Stop'; $name={}+[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $t=Get-ScheduledTask -TaskName $name; $a=@($t.Actions); if ($a.Count -eq 1 -and ($a[0].Arguments.Contains({}) -or $a[0].Arguments.Contains({}))) {{ 'yes' }} else {{ 'no' }}",ps(&format!("Sharebox-{}-",role.replace('.',"-"))),ps(&needle),ps(&other));
    let output = bounded_output(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ],
    )?;
    if !output.status.success() {
        return Err("Cannot inspect registered executable".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "yes")
}

/// The updater leaves registrations untouched and restores only previously loaded roles.
#[cfg(any(windows, target_os = "macos"))]
pub fn update_service_states(target: &Path) -> Result<Vec<String>> {
    let mut roles = Vec::new();
    for role in ["master", "worker", "tray.master", "tray.worker"] {
        if running_native(role)? && update_registration_matches(role, target)? {
            roles.push(role.into());
        }
    }
    Ok(roles)
}
#[cfg(any(windows, target_os = "macos"))]
pub fn stop_for_update(roles: &[String]) -> Result<()> {
    for role in roles.iter().rev() {
        if loaded(role)? {
            platform("stop", role, None)?;
        }
    }
    for _ in 0..8 {
        let mut any = false;
        for role in roles {
            any |= running_native(role)?;
        }
        if !any {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err("Timed out waiting for Sharebox services to stop".into())
}
#[cfg(any(windows, target_os = "macos"))]
pub fn restore_after_update(roles: &[String]) -> Result<()> {
    for role in roles {
        if !running_native(role)? {
            platform("start", role, None)?;
        }
    }
    let mut consecutive = 0;
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(750));
        let mut all = true;
        for role in roles {
            all &= running_native(role)?;
        }
        consecutive = if all { consecutive + 1 } else { 0 };
        if consecutive >= 3 {
            return Ok(());
        }
    }
    Err("Updated services did not stay running after restart".into())
}
#[cfg(target_os = "macos")]
pub fn launch_update_helper(helper: &Path, plan: &Path) -> Result<()> {
    let label = format!("org.ossmalaysia.sharebox.update.{}", unsafe {
        libc::geteuid()
    });
    let existing = bounded_output(
        "launchctl",
        &[
            "print",
            &format!("gui/{}/{}", unsafe { libc::geteuid() }, label),
        ],
    )?;
    if existing.status.success() {
        if String::from_utf8_lossy(&existing.stdout)
            .lines()
            .any(|l| l.trim() == "state = running")
        {
            return Err("An updater is already running".into());
        }
        command("launchctl", &["remove", &label])?;
    }
    command(
        "launchctl",
        &[
            "submit",
            "-l",
            &label,
            "--",
            &path_text(helper)?,
            "update",
            "--apply",
            &path_text(plan)?,
        ],
    )
}
#[cfg(target_os = "macos")]
pub fn cleanup_update_helper() -> Result<()> {
    // launchctl's transient submit job disappears when removed; removing a live job
    // also kills it, so leave its inert registration until the next update launch.
    Ok(())
}
#[cfg(windows)]
pub fn launch_update_helper(helper: &Path, plan: &Path) -> Result<()> {
    let invocation = format!(
        "& {} update --apply {}",
        ps(&path_text(helper)?),
        ps(&path_text(plan)?)
    );
    let args = format!(
        "-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -Command {}",
        windows_arg(&invocation)
    );
    let script=format!("$ErrorActionPreference='Stop'; $id=[Security.Principal.WindowsIdentity]::GetCurrent(); $name='Sharebox-update-'+$id.User.Value; if (Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue) {{ throw 'An update task already exists' }}; $action=New-ScheduledTaskAction -Execute {} -Argument {}; $principal=New-ScheduledTaskPrincipal -UserId $id.Name -LogonType Interactive -RunLevel Limited; $settings=New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 10) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries; Register-ScheduledTask -TaskName $name -Action $action -Principal $principal -Settings $settings | Out-Null; try {{ Start-ScheduledTask -TaskName $name }} catch {{ Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction SilentlyContinue; throw }};",ps("powershell.exe"),ps(&args));
    command(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ],
    )
}
#[cfg(windows)]
pub fn cleanup_update_helper() -> Result<()> {
    let helper = std::env::current_exe().map_err(|e| e.to_string())?;
    let needle = format!("& {} ", ps(&path_text(&helper)?));
    let plain = helper
        .to_string_lossy()
        .trim_start_matches("\\\\?\\")
        .to_owned();
    let other = format!("& {} ", ps(&plain));
    let script=format!("$ErrorActionPreference='Stop'; $name='Sharebox-update-'+[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $t=Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue; if ($null -ne $t) {{ $a=@($t.Actions); if ($a.Count -eq 1 -and ($a[0].Arguments.Contains({}) -or $a[0].Arguments.Contains({}))) {{ Unregister-ScheduledTask -TaskName $name -Confirm:$false }} }}",ps(&needle),ps(&other));
    command(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ],
    )
}

#[cfg(all(test, any(windows, target_os = "macos")))]
pub fn update_launcher_test_available() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}", unsafe { libc::geteuid() });
        if !bounded_output("launchctl", &["print", &domain])?
            .status
            .success()
        {
            return Ok(false);
        }
        let label = format!("{domain}/org.ossmalaysia.sharebox.update.{}", unsafe {
            libc::geteuid()
        });
        Ok(!bounded_output("launchctl", &["print", &label])?
            .status
            .success())
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").map_err(|_| "USERNAME is missing")?;
        if !bounded_output("quser.exe", &[&user])?.status.success() {
            return Ok(false);
        }
        Ok(!registered("update")?)
    }
}
#[cfg(all(test, any(windows, target_os = "macos")))]
pub fn cleanup_launcher_test(helper: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let label = format!("org.ossmalaysia.sharebox.update.{}", unsafe {
            libc::geteuid()
        });
        let target = format!("gui/{}/{}", unsafe { libc::geteuid() }, label);
        let out = bounded_output("launchctl", &["print", &target])?;
        if out.status.success()
            && String::from_utf8_lossy(&out.stdout).contains(&path_text(helper)?)
        {
            command("launchctl", &["remove", &label])?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let needle = format!("& {} ", ps(&path_text(helper)?));
        let script=format!("$ErrorActionPreference='Stop'; $name='Sharebox-update-'+[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $t=Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue; if ($null -ne $t -and @($t.Actions).Count -eq 1 -and $t.Actions[0].Arguments.Contains({})) {{ Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue; Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction SilentlyContinue }}; $remaining=Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue; if ($null -ne $remaining -and @($remaining.Actions).Count -eq 1 -and $remaining.Actions[0].Arguments.Contains({})) {{ throw 'Updater test registration was not removed' }}; exit 0",ps(&needle),ps(&needle));
        command(
            "powershell.exe",
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ],
        )
    }
}

/// Read the configuration path from the owner's existing registration. Never
/// guess a new path when a registration exists but cannot be understood.
pub fn worker_registration_config() -> Result<Option<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        let file = home()?.join("Library/LaunchAgents/org.ossmalaysia.sharebox.worker.plist");
        if !file.exists() {
            return Ok(None);
        }
        let output = bounded_output(
            "/usr/bin/plutil",
            &["-convert", "json", "-o", "-", &path_text(&file)?],
        )?;
        if !output.status.success() {
            return Err("Cannot read worker service registration".into());
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| "Invalid worker service registration")?;
        let args = value["ProgramArguments"]
            .as_array()
            .ok_or("Worker service arguments missing")?;
        let index = args
            .iter()
            .position(|a| a == "--config")
            .ok_or("Worker service configuration path missing")?;
        let path = args
            .get(index + 1)
            .and_then(|a| a.as_str())
            .ok_or("Worker service configuration path missing")?;
        checked_worker_config(path).map(Some)
    }
    #[cfg(windows)]
    {
        if !registered("worker")? {
            return Ok(None);
        }
        let script = "$ErrorActionPreference='Stop'; $name='Sharebox-worker-'+[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $t=Get-ScheduledTask -TaskName $name; $a=@($t.Actions); if ($a.Count -ne 1) { throw 'Unexpected worker service actions' }; ConvertTo-Json -Compress -InputObject $a[0].Arguments";
        let output = bounded_output(
            "powershell.exe",
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ],
        )?;
        if !output.status.success() {
            return Err("Cannot read worker service registration".into());
        }
        let args: String = serde_json::from_slice(&output.stdout)
            .map_err(|_| "Invalid worker service arguments")?;
        config_from_powershell(&args)
            .and_then(|p| checked_worker_config(&p))
            .map(Some)
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let directory = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or(home()?.join(".config"));
        let file = directory.join("systemd/user/sharebox-worker.service");
        if !file.exists() {
            return Ok(None);
        }
        let mut text = String::new();
        std::fs::File::open(file)
            .map_err(|e| e.to_string())?
            .take(16385)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        if text.len() > 16384 {
            return Err("Worker service registration too large".into());
        }
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix("ExecStart="))
            .ok_or("Worker service arguments missing")?;
        config_from_systemd(line)
            .and_then(|p| checked_worker_config(&p))
            .map(Some)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    Err("Worker services are unsupported on this platform".into())
}
fn checked_worker_config(value: &str) -> Result<PathBuf> {
    safe(value)?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("Registered worker configuration path must be absolute".into());
    }
    Ok(path)
}
#[cfg(any(windows, test))]
fn config_from_powershell(args: &str) -> Result<String> {
    let tail = args
        .rsplit_once("'--config'")
        .ok_or("Worker service configuration path missing")?
        .1
        .trim_start();
    let mut chars = tail
        .strip_prefix('\'')
        .ok_or("Unsupported worker service configuration quoting")?
        .chars()
        .peekable();
    let mut value = String::new();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                value.push('\'');
            } else {
                return Ok(value);
            }
        } else {
            value.push(c);
        }
    }
    Err("Unterminated worker service configuration path".into())
}
#[cfg(any(target_os = "linux", test))]
fn config_from_systemd(args: &str) -> Result<String> {
    let mut chars = args.chars().peekable();
    let mut tokens = Vec::new();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        if chars.next() != Some('"') {
            return Err("Unsupported worker service arguments".into());
        }
        let mut token = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    closed = true;
                    break;
                }
                '\\' => {
                    let next = chars.next().ok_or("Invalid service escape")?;
                    if !matches!(next, '\\' | '"') {
                        return Err("Unsupported service escape".into());
                    }
                    token.push(next);
                }
                _ => token.push(c),
            }
        }
        if !closed {
            return Err("Unterminated worker service argument".into());
        }
        tokens.push(token.replace("%%", "%").replace("$$", "$"));
    }
    let i = tokens
        .iter()
        .position(|v| v == "--config")
        .ok_or("Worker service configuration path missing")?;
    tokens
        .get(i + 1)
        .cloned()
        .ok_or("Worker service configuration path missing".into())
}
pub fn worker_registration_uses_current_executable() -> Result<bool> {
    let target = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    #[cfg(any(windows, target_os = "macos"))]
    {
        update_registration_matches("worker", &target)
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let directory = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or(home()?.join(".config"));
        let mut text = String::new();
        std::fs::File::open(directory.join("systemd/user/sharebox-worker.service"))
            .map_err(|e| e.to_string())?
            .take(16385)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        if text.len() > 16384 {
            return Err("Worker service registration too large".into());
        }
        let prefix = format!("ExecStart={} ", unit_quote(&path_text(&target)?, true));
        Ok(text.lines().any(|line| line.starts_with(&prefix)))
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = target;
        Err("Worker services are unsupported on this platform".into())
    }
}

pub fn worker_service_running() -> Result<bool> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        is_running("worker")
    }
    #[cfg(target_os = "linux")]
    {
        let status = Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "sharebox-worker.service"])
            .status()
            .map_err(|e| e.to_string())?;
        match status.code() {
            Some(0) => Ok(true),
            Some(3 | 4) => Ok(false),
            _ => Err("Cannot inspect worker service status".into()),
        }
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Err("Worker services are unsupported on this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn setup_reads_existing_config_paths_with_spaces_and_escapes() {
        let source = sample();
        let args = format!(
            "& {} 'worker' '--config' {}",
            ps(&source.exe),
            ps(&source.args[2])
        );
        assert_eq!(config_from_powershell(&args).unwrap(), source.args[2]);
        assert!(config_from_powershell("'--config' 'unfinished").is_err());
        let unit = unit(&source);
        let line = unit
            .lines()
            .find_map(|l| l.strip_prefix("ExecStart="))
            .unwrap();
        assert_eq!(config_from_systemd(line).unwrap(), source.args[2]);
        assert!(config_from_systemd("unquoted --config /tmp/x").is_err());
    }
    fn sample() -> Spec {
        Spec {
            role: "worker".into(),
            exe: "/a space/sharebox".into(),
            args: vec![
                "worker".into(),
                "--config".into(),
                "/a'&%$/worker.json".into(),
            ],
            cwd: "/a space".into(),
            path: "/bin:/node path".into(),
            dashboard: "http://127.0.0.1:7331".into(),
        }
    }
    #[test]
    fn reject_remote_credentials_and_invalid_actions() {
        for args in [
            vec!["install", "worker", "--code", "secret"],
            vec!["stop", "master", "--data", "x"],
            vec!["install", "other"],
            vec!["install", "master", "--bind", "a\nb"],
            vec!["install", "master", "--data", "a", "--data", "b"],
        ] {
            assert!(parse(args.into_iter().map(str::to_owned).collect()).is_err());
        }
    }
    #[test]
    fn mac_service_escapes_paths_and_does_not_write_logs() {
        let text = plist(&sample());
        assert!(text.contains("/a&apos;&amp;%$/worker.json"));
        assert!(text.contains("<key>PATH</key><string>/bin:/node path</string>"));
        assert!(text.contains("/dev/null"));
    }
    #[test]
    fn companions_start_the_same_role_without_worker_credentials() {
        for role in ["master", "worker"] {
            let mut source = sample();
            source.role = role.into();
            let tray = companion(&source);
            assert_eq!(tray.role, format!("tray.{role}"));
            assert_eq!(
                tray.args,
                [
                    "tray",
                    "--role",
                    role,
                    "--dashboard",
                    "http://127.0.0.1:7331"
                ]
            );
            assert!(!tray.args.iter().any(|arg| arg.contains("worker.json")));
            let mac = plist(&tray);
            assert!(mac.contains("<key>KeepAlive</key><false/>"));
            assert!(mac.contains(&format!("org.ossmalaysia.sharebox.tray.{role}")));
            let windows = windows_script("install", &tray.role, Some(&tray)).unwrap();
            assert!(windows.contains(&format!("'Sharebox-tray-{role}-'")));
            assert!(windows.contains("$registered = $true"));
            assert!(windows.contains("if ($registered)"));
        }
    }
    #[test]
    fn systemd_disables_specifier_and_variable_expansion() {
        let text = unit(&sample());
        assert!(text.contains("\"/a'&%%$$/worker.json\""));
        assert!(text.contains("Environment=\"PATH=/bin:/node path\""));
        assert!(text.contains("WorkingDirectory=/a space/.\n"));
        assert_eq!(unit_working_directory("/a\\b\"c% "), "/a\\b\"c%% /.");
        assert_eq!(unit_quote("a\\b\"c", true), "\"a\\\\b\\\"c\"");
    }
    #[test]
    fn windows_quotes_shell_literals_and_command_line() {
        assert_eq!(ps("a'$(evil)"), "'a''$(evil)'");
        assert_eq!(windows_arg("C:\\a b\\"), "\"C:\\a b\\\\\"");
        assert_eq!(windows_arg("a\"b"), "\"a\\\"b\"");
        let text = windows_script("install", "worker", Some(&sample())).unwrap();
        assert!(text.contains("-LogonType Interactive -RunLevel Limited"));
        assert!(text.contains("-AtLogOn"));
        assert!(text.contains("-WindowStyle Hidden"));
        assert!(!text.contains("-Force"));
        assert!(!text.contains("--code"));
        assert!(text.contains("/a''''&%$/worker.json"));
    }
}
