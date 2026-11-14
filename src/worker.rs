//! Native LAN worker. Only commands explicitly named in the local config can run.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, String>;
static STOP: AtomicBool = AtomicBool::new(false);
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn nonce() -> String {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("secure randomness unavailable");
    b.iter().map(|v| format!("{v:02x}")).collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    #[serde(default)]
    pub description: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}
fn default_timeout() -> u64 {
    600
}
fn default_poll() -> u64 {
    3
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub server: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub worker_id: String,
    pub name: String,
    #[serde(default = "default_poll")]
    pub poll_seconds: u64,
    pub tasks: BTreeMap<String, Task>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Job {
    id: String,
    task: String,
    #[serde(default)]
    timeout_seconds: u64,
}
#[derive(Debug, Deserialize)]
struct Poll {
    #[serde(default)]
    job: Option<Job>,
    #[serde(default)]
    cancel: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    job_id: String,
    sequence: u64,
    finished: bool,
    events: Vec<Event>,
}
impl Pending {
    fn add(&mut self, log: Option<String>, status: Option<&str>, exit_code: Option<i32>) {
        self.sequence += 1;
        self.events.push(Event {
            sequence: self.sequence,
            log,
            status: status.map(str::to_owned),
            exit_code,
        });
    }
    fn save(&self, path: &Path) -> Result<()> {
        private_write(path, &serde_json::to_vec(self).map_err(error)?)
    }
}
const MAX_LOG: usize = 256 * 1024;
#[derive(Default)]
struct Logs {
    bytes: Vec<u8>,
    total: usize,
    truncated: bool,
}
impl Logs {
    fn append(&mut self, data: &[u8]) {
        let keep = data.len().min(MAX_LOG.saturating_sub(self.total));
        self.bytes.extend_from_slice(&data[..keep]);
        self.total += keep;
        if keep < data.len() && !self.truncated {
            self.truncated = true;
            self.bytes
                .extend_from_slice(b"\n[sharebox] Output truncated at 256 KiB.\n");
        }
    }
    fn drain(&mut self) -> Option<String> {
        if self.bytes.is_empty() {
            return None;
        }
        let count = self.bytes.len().min(16 * 1024);
        Some(String::from_utf8_lossy(&self.bytes.drain(..count).collect::<Vec<_>>()).into_owned())
    }
}
fn capture<R: Read>(mut stream: R, logs: Arc<Mutex<Logs>>) {
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => logs.lock().unwrap().append(&buffer[..n]),
        }
    }
}
fn private_write(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(error)?;
    let temp = parent.join(format!(".sharebox-{}.tmp", nonce()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp, path)?;
        Ok::<_, std::io::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(error)
}
#[cfg(not(windows))]
pub(crate) fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}
#[cfg(windows)]
pub(crate) fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn validate(config: &mut Config) -> Result<()> {
    if !config.server.starts_with("http://")
        || config.server[7..].is_empty()
        || config.server[7..]
            .trim_end_matches('/')
            .contains(['/', '?', '#', '@'])
    {
        return Err(
            "v1 requires an http://HOST:PORT LAN origin, without path or credentials".into(),
        );
    }
    config.server = config.server.trim_end_matches('/').to_owned();
    if !(1..=30).contains(&config.poll_seconds) {
        return Err("pollSeconds must be 1..30".into());
    }
    if config.tasks.is_empty() || config.tasks.len() > 64 {
        return Err("configure 1 to 64 allowed tasks".into());
    }
    if config.name.len() > 100 {
        return Err("worker name must be at most 100 bytes".into());
    }
    for (name, task) in &config.tasks {
        if name.is_empty()
            || name.len() > 80
            || task.description.len() > 500
            || task.argv.is_empty()
            || task.argv[0].is_empty()
        {
            return Err(format!("invalid task: {name}"));
        }
        if !task.cwd.is_absolute() || !task.cwd.is_dir() {
            return Err(format!(
                "task {name} cwd must be an existing absolute directory"
            ));
        }
        if !(1..=86400).contains(&task.timeout_seconds) {
            return Err(format!("task {name} timeout must be 1..86400 seconds"));
        }
    }
    Ok(())
}
fn descriptors(config: &Config) -> Vec<Value> {
    config
        .tasks
        .iter()
        .map(|(name, t)| json!({"name":name,"description":t.description}))
        .collect()
}
#[cfg(test)]
type MockTransport = Arc<dyn Fn(&str, &Value) -> Result<Value> + Send + Sync>;
struct Connection {
    config: Config,
    #[cfg(test)]
    mock: Option<MockTransport>,
}
impl Connection {
    fn post(&self, path: &str, data: &impl Serialize) -> Result<Value> {
        let value = serde_json::to_value(data).map_err(error)?;
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            return mock(path, &value);
        }
        crate::http::client_request(
            &self.config.server,
            path,
            if self.config.token.is_empty() {
                None
            } else {
                Some(&self.config.token)
            },
            &value,
        )
    }
    fn poll(&self, running: Option<&str>) -> Result<Poll> {
        serde_json::from_value(self.post(
            "/api/worker/poll",
            &json!({"runningJobId":running,"tasks":descriptors(&self.config)}),
        )?)
        .map_err(error)
    }
    fn flush(&self, pending: &mut Pending, path: &Path) -> Result<()> {
        self.flush_limit(pending, path, usize::MAX)
    }
    fn flush_limit(&self, pending: &mut Pending, path: &Path, limit: usize) -> Result<()> {
        let mut sent = 0;
        while let Some(event) = pending.events.first() {
            if sent >= limit {
                break;
            }
            if !pending
                .job_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err("invalid job ID".into());
            }
            self.post(
                &format!("/api/worker/jobs/{}/events", pending.job_id),
                event,
            )?;
            pending.events.remove(0);
            pending.save(path)?;
            sent += 1;
        }
        Ok(())
    }
    fn deliver(&self, pending: &mut Pending, path: &Path) -> Result<()> {
        loop {
            match self.flush(pending, path) {
                Ok(()) => return std::fs::remove_file(path).map_err(error),
                Err(e) if permanent(&e) => {
                    return Err(format!(
                        "Result delivery rejected; pending result retained for review: {e}"
                    ))
                }
                Err(e) => eprintln!("Result delivery pending: {e}"),
            }
            if !sleep_interruptible(Duration::from_secs(3)) {
                return Err("worker stopped; pending results retained".into());
            }
        }
    }
    fn execute(&self, job: Job, state_path: &Path) -> Result<()> {
        let mut pending = Pending {
            job_id: job.id.clone(),
            sequence: 0,
            finished: false,
            events: vec![],
        };
        let Some(task) = self.config.tasks.get(&job.task) else {
            pending.finished = true;
            pending.add(
                Some("Task is not allowed by local worker configuration.".into()),
                Some("failed"),
                Some(-1),
            );
            pending.save(state_path)?;
            return self.deliver(&mut pending, state_path);
        };
        let mut command = Command::new(&task.argv[0]);
        command
            .args(&task.argv[1..])
            .current_dir(&task.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x00000200);
        }
        let timeout = if job.timeout_seconds == 0 {
            task.timeout_seconds
        } else {
            task.timeout_seconds.min(job.timeout_seconds)
        };
        pending.add(
            Some(format!("[sharebox] Starting allowed task {}\n", job.task)),
            Some("running"),
            None,
        );
        pending.save(state_path)?;
        if let Err(e) = self.flush(&mut pending, state_path) {
            if permanent(&e) {
                return Err(format!("Job start rejected; nothing was executed: {e}"));
            }
        }
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                pending.finished = true;
                pending.add(
                    Some(format!("Unable to start task: {e}\n")),
                    Some("failed"),
                    Some(-1),
                );
                pending.save(state_path)?;
                return self.deliver(&mut pending, state_path);
            }
        };
        let pid = child.id();
        let mut guard = ProcessGuard(Some(pid));
        let logs = Arc::new(Mutex::new(Logs::default()));
        let out = child.stdout.take().unwrap();
        let outlogs = logs.clone();
        let stdout = std::thread::spawn(move || capture(out, outlogs));
        let err = child.stderr.take().unwrap();
        let errlogs = logs.clone();
        let stderr = std::thread::spawn(move || capture(err, errlogs));
        let started = Instant::now();
        let mut heartbeat = Instant::now();
        let mut last_contact = Instant::now();
        let mut rejected = None;
        let (status, exit_code, stopped) = loop {
            match child.try_wait() {
                Ok(Some(s)) => {
                    break (
                        if s.success() { "passed" } else { "failed" },
                        s.code().unwrap_or(-1),
                        false,
                    )
                }
                Ok(None) => {}
                Err(e) => {
                    logs.lock()
                        .unwrap()
                        .append(format!("Wait failed: {e}\n").as_bytes());
                    break ("failed", -1, false);
                }
            }
            if STOP.load(Ordering::Relaxed) {
                kill_tree(pid);
                let _ = child.kill();
                let _ = child.wait();
                break ("cancelled", -1, true);
            }
            if started.elapsed() >= Duration::from_secs(timeout) {
                kill_tree(pid);
                let _ = child.kill();
                let _ = child.wait();
                logs.lock()
                    .unwrap()
                    .append(b"\n[sharebox] Task timeout exceeded.\n");
                break ("failed", -1, false);
            }
            if last_contact.elapsed() > Duration::from_secs(60) {
                kill_tree(pid);
                let _ = child.kill();
                let _ = child.wait();
                logs.lock()
                    .unwrap()
                    .append(b"\n[sharebox] Master unreachable for 60 seconds; stopping task.\n");
                break ("failed", -1, false);
            }
            if heartbeat.elapsed() >= Duration::from_secs(self.config.poll_seconds) {
                if let Some(chunk) = logs.lock().unwrap().drain() {
                    pending.add(Some(chunk), None, None);
                    pending.save(state_path)?;
                }
                match self.poll(Some(&job.id)) {
                    Ok(p) => {
                        last_contact = Instant::now();
                        if p.cancel {
                            kill_tree(pid);
                            let _ = child.kill();
                            let _ = child.wait();
                            break ("cancelled", -1, false);
                        }
                    }
                    Err(e) if permanent(&e) => {
                        kill_tree(pid);
                        let _ = child.kill();
                        let _ = child.wait();
                        logs.lock().unwrap().append(
                            format!("\n[sharebox] Master rejected worker heartbeat: {e}\n")
                                .as_bytes(),
                        );
                        rejected = Some(e);
                        break ("cancelled", -1, false);
                    }
                    Err(e) => eprintln!("Heartbeat failed: {e}"),
                }
                if let Err(e) = self.flush_limit(&mut pending, state_path, 1) {
                    if permanent(&e) {
                        kill_tree(pid);
                        let _ = child.kill();
                        let _ = child.wait();
                        rejected = Some(e);
                        break ("cancelled", -1, false);
                    }
                }
                heartbeat = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        kill_tree(pid);
        guard.0 = None;
        // Pipe reader threads finish once all process-tree handles are closed.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !(stdout.is_finished() && stderr.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if stdout.is_finished() {
            let _ = stdout.join();
        }
        if stderr.is_finished() {
            let _ = stderr.join();
        }
        while let Some(chunk) = logs.lock().unwrap().drain() {
            pending.add(Some(chunk), None, None);
        }
        pending.finished = true;
        pending.add(None, Some(status), Some(exit_code));
        pending.save(state_path)?;
        if let Some(e) = rejected {
            return Err(format!(
                "Worker authorization/protocol rejected; task stopped and result retained: {e}"
            ));
        }
        if stopped {
            let _ = self.flush(&mut pending, state_path);
            return Err("worker stopped; any unacknowledged result is saved locally".into());
        }
        self.deliver(&mut pending, state_path)
    }
}
struct ProcessGuard(Option<u32>);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_tree(pid);
        }
    }
}
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
fn permanent(message: &str) -> bool {
    message.starts_with("HTTP 4")
        && !message.starts_with("HTTP 408")
        && !message.starts_with("HTTP 429")
}
fn sleep_interruptible(duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if STOP.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}
fn install_shutdown() {
    STOP.store(false, Ordering::Relaxed);
    #[cfg(unix)]
    {
        extern "C" fn stop(_: libc::c_int) {
            STOP.store(true, Ordering::Relaxed);
        }
        unsafe {
            libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        }
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetConsoleCtrlHandler(
                handler: Option<extern "system" fn(u32) -> i32>,
                add: i32,
            ) -> i32;
        }
        extern "system" fn stop(event: u32) -> i32 {
            if event == 0 || event == 1 {
                STOP.store(true, Ordering::Relaxed);
                1
            } else {
                0
            }
        }
        unsafe {
            SetConsoleCtrlHandler(Some(stop), 1);
        }
    }
}

/// Create a private worker config and a bundled Node smoke test. Refuses to overwrite config.
pub fn init(path: PathBuf) -> Result<()> {
    if path.exists() {
        return Err(format!("config already exists: {}", path.display()));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(error)?;
    let parent = parent.canonicalize().map_err(error)?;
    let demo = parent.join("sharebox-demo");
    std::fs::create_dir_all(&demo).map_err(error)?;
    let smoke = demo.join("smoke.test.cjs");
    if !smoke.exists() {
        std::fs::write(&smoke,"const {test}=require('node:test');const assert=require('node:assert/strict');test('remote Node test',()=>assert.equal(20+22,42));\n").map_err(error)?;
    }
    let config = Config {
        server: "http://127.0.0.1:7331".into(),
        token: String::new(),
        worker_id: String::new(),
        name: std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| format!("{}-worker", std::env::consts::OS)),
        poll_seconds: 3,
        tasks: BTreeMap::from([(
            "node-smoke".into(),
            Task {
                description: "Node.js smoke test".into(),
                argv: vec!["node".into(), "--test".into(), "smoke.test.cjs".into()],
                cwd: demo,
                timeout_seconds: 60,
            },
        )]),
    };
    private_write(&path, &serde_json::to_vec_pretty(&config).map_err(error)?)?;
    println!("Created {} with node-smoke task. Edit allowed tasks before pairing. Native execution is not a sandbox.",path.display());
    Ok(())
}

/// Pair if requested, then execute the owner's task allowlist on the trusted LAN.
pub fn run(server: Option<String>, code: Option<String>, config_path: PathBuf) -> Result<()> {
    install_shutdown();
    if !config_path.exists() {
        init(config_path.clone())?;
    }
    let mut config: Config =
        serde_json::from_slice(&std::fs::read(&config_path).map_err(error)?).map_err(error)?;
    if let Some(server) = server {
        config.server = server;
    }
    validate(&mut config)?;
    let mut connection = Connection {
        config,
        #[cfg(test)]
        mock: None,
    };
    let state_path = PathBuf::from(format!("{}.pending.json", config_path.display()));
    if let Some(code) = code {
        let response=connection.post("/api/worker/pair",&json!({"code":code,"name":connection.config.name,"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"tasks":descriptors(&connection.config)}))?;
        connection.config.token = response
            .get("token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("missing pairing token")?
            .into();
        connection.config.worker_id = response
            .get("workerId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("missing worker ID")?
            .into();
        if state_path.exists() {
            let archive = PathBuf::from(format!(
                "{}.archived-{}.json",
                state_path.display(),
                nonce()
            ));
            std::fs::rename(&state_path, &archive).map_err(error)?;
            eprintln!(
                "Previous worker pending results archived at {} (new pairing has a new identity).",
                archive.display()
            );
        }
        private_write(
            &config_path,
            &serde_json::to_vec_pretty(&connection.config).map_err(error)?,
        )?;
        println!("Paired worker {}.", connection.config.worker_id);
    }
    if connection.config.token.is_empty() || connection.config.worker_id.is_empty() {
        return Err("worker is not paired; provide --server and --code".into());
    }
    if state_path.exists() {
        let mut pending: Pending =
            serde_json::from_slice(&std::fs::read(&state_path).map_err(error)?).map_err(error)?;
        if !pending.finished {
            pending.add(Some("[sharebox] Worker restarted before completion. Task was not automatically retried.\n".into()),Some("failed"),Some(-1));
            pending.finished = true;
            pending.save(&state_path)?;
        }
        connection.deliver(&mut pending, &state_path)?;
    }
    println!(
        "Worker {} ready: {} allowed tasks, one native execution slot.",
        connection.config.name,
        connection.config.tasks.len()
    );
    while !STOP.load(Ordering::Relaxed) {
        match connection.poll(None) {
            Ok(Poll { job: Some(job), .. }) => {
                println!("Running job {} task {}", job.id, job.task);
                connection.execute(job, &state_path)?;
                continue;
            }
            Ok(_) => {}
            Err(e) if permanent(&e) => {
                return Err(format!(
                    "Worker rejected by master; stop and re-pair if revoked: {e}"
                ))
            }
            Err(e) => eprintln!("Poll failed: {e}"),
        }
        if !sleep_interruptible(Duration::from_secs(connection.config.poll_seconds)) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("sharebox-test-{}", nonce()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    #[test]
    fn log_limit_is_total_not_per_chunk() {
        let mut logs = Logs::default();
        logs.append(&vec![b'a'; MAX_LOG]);
        while logs.drain().is_some() {}
        logs.append(b"overflow");
        let text = logs.drain().unwrap();
        assert!(text.contains("truncated"));
        logs.append(b"more");
        assert!(logs.drain().is_none());
    }
    #[test]
    fn init_is_private_and_never_overwrites() {
        let d = dir();
        let path = d.join("config.json");
        init(path.clone()).unwrap();
        assert!(init(path.clone()).is_err());
        let mut c: Config = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        validate(&mut c).unwrap();
        assert!(c.tasks["node-smoke"].cwd.is_absolute());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn invalid_origins_and_workspaces_are_rejected() {
        let d = dir();
        let p = d.join("config.json");
        init(p.clone()).unwrap();
        let mut c: Config = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        c.server = "http://user:password@localhost:7331".into();
        assert!(validate(&mut c).is_err());
        c.server = "http://192.168.1.5:7331".into();
        validate(&mut c).unwrap();
        c.tasks.get_mut("node-smoke").unwrap().cwd = PathBuf::from("relative");
        assert!(validate(&mut c).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn pending_replay_keeps_sequences() {
        let mut p = Pending {
            job_id: "job-1".into(),
            sequence: 0,
            finished: false,
            events: vec![],
        };
        p.add(Some("hello".into()), Some("running"), None);
        p.add(None, Some("passed"), Some(0));
        let restored: Pending = serde_json::from_slice(&serde_json::to_vec(&p).unwrap()).unwrap();
        assert_eq!(restored.events[0].sequence, 1);
        assert_eq!(restored.events[1].sequence, 2);
    }
    struct FakeServer {
        events: Arc<Mutex<Vec<Value>>>,
        cancel: bool,
    }
    impl FakeServer {
        fn start(cancel: bool) -> Self {
            Self {
                events: Arc::new(Mutex::new(vec![])),
                cancel,
            }
        }
    }
    fn runner(server: &FakeServer, d: &Path, script: &str, timeout: u64) -> Connection {
        let events = server.events.clone();
        let cancel = server.cancel;
        Connection {
            mock: Some(Arc::new(move |path, value| {
                if path.ends_with("/events") {
                    events.lock().unwrap().push(value.clone());
                    Ok(json!({"ok":true}))
                } else {
                    Ok(json!({"job":null,"cancel":cancel}))
                }
            })),
            config: Config {
                server: "http://127.0.0.1:7331".into(),
                token: "test-token".into(),
                worker_id: "test-worker".into(),
                name: "Test".into(),
                poll_seconds: 1,
                tasks: BTreeMap::from([(
                    "test".into(),
                    Task {
                        description: "test".into(),
                        argv: vec!["node".into(), "-e".into(), script.into()],
                        cwd: d.to_path_buf(),
                        timeout_seconds: timeout,
                    },
                )]),
            },
        }
    }
    #[test]
    fn executes_node_and_delivers_ordered_results() {
        let server = FakeServer::start(false);
        let d = dir();
        runner(&server, &d, "console.log('hello remote worker')", 5)
            .execute(
                Job {
                    id: "job-test".into(),
                    task: "test".into(),
                    timeout_seconds: 5,
                },
                &d.join("pending.json"),
            )
            .unwrap();
        let events = server.events.lock().unwrap();
        assert_eq!(events.last().unwrap()["status"], "passed");
        assert_eq!(events.last().unwrap()["exitCode"], 0);
        assert!(events.iter().any(|e| e["log"]
            .as_str()
            .unwrap_or("")
            .contains("hello remote worker")));
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e["sequence"], (i + 1) as u64);
        }
        assert!(!d.join("pending.json").exists());
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn refuses_remote_task_not_in_allowlist() {
        let server = FakeServer::start(false);
        let d = dir();
        runner(&server, &d, "console.log('should not execute')", 5)
            .execute(
                Job {
                    id: "job-reject".into(),
                    task: "arbitrary-command".into(),
                    timeout_seconds: 5,
                },
                &d.join("pending.json"),
            )
            .unwrap();
        let events = server.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["status"], "failed");
        assert!(events[0]["log"].as_str().unwrap().contains("not allowed"));
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn timeout_kills_long_running_task() {
        let server = FakeServer::start(false);
        let d = dir();
        let start = Instant::now();
        runner(&server, &d, "setInterval(()=>{},1000)", 1)
            .execute(
                Job {
                    id: "job-timeout".into(),
                    task: "test".into(),
                    timeout_seconds: 1,
                },
                &d.join("pending.json"),
            )
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        let events = server.events.lock().unwrap();
        assert_eq!(events.last().unwrap()["status"], "failed");
        assert!(events
            .iter()
            .any(|e| e["log"].as_str().unwrap_or("").contains("timeout")));
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn remote_cancellation_stops_task() {
        let server = FakeServer::start(true);
        let d = dir();
        runner(&server, &d, "setInterval(()=>{},1000)", 10)
            .execute(
                Job {
                    id: "job-cancel".into(),
                    task: "test".into(),
                    timeout_seconds: 10,
                },
                &d.join("pending.json"),
            )
            .unwrap();
        assert_eq!(
            server.events.lock().unwrap().last().unwrap()["status"],
            "cancelled"
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn revocation_stops_task_and_retains_result() {
        let server = FakeServer::start(false);
        let d = dir();
        let mut connection = runner(&server, &d, "setInterval(()=>{},1000)", 10);
        connection.mock = Some(Arc::new(|path, _| {
            if path.ends_with("/poll") {
                Err("HTTP 401: Valid bearer token required".into())
            } else {
                Ok(json!({"ok":true}))
            }
        }));
        let start = Instant::now();
        let pending = d.join("pending.json");
        let result = connection.execute(
            Job {
                id: "job-revoked".into(),
                task: "test".into(),
                timeout_seconds: 10,
            },
            &pending,
        );
        assert!(result.unwrap_err().contains("rejected"));
        assert!(start.elapsed() < Duration::from_secs(4));
        let saved: Pending = serde_json::from_slice(&std::fs::read(pending).unwrap()).unwrap();
        assert_eq!(
            saved.events.last().unwrap().status.as_deref(),
            Some("cancelled")
        );
        assert!(saved.finished);
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn unauthorized_start_does_not_launch_process() {
        let server = FakeServer::start(false);
        let d = dir();
        let mut connection = runner(
            &server,
            &d,
            "require('node:fs').writeFileSync('executed','bad')",
            5,
        );
        connection.mock = Some(Arc::new(|_, _| Err("HTTP 401: revoked".into())));
        assert!(connection
            .execute(
                Job {
                    id: "job-revoked-start".into(),
                    task: "test".into(),
                    timeout_seconds: 5
                },
                &d.join("pending.json")
            )
            .is_err());
        assert!(!d.join("executed").exists());
        std::fs::remove_dir_all(d).unwrap();
    }
}
