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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotTask>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotMode {
    Native,
    Docker,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotNetwork {
    #[default]
    None,
    Bridge,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotTask {
    pub mode: SnapshotMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default)]
    pub prepare: Vec<Vec<String>>,
    #[serde(default)]
    pub network: SnapshotNetwork,
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
    #[serde(default)]
    snapshot_id: Option<String>,
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
        if let Some(snapshot) = &task.snapshot {
            validate_snapshot_task(snapshot).map_err(|e| format!("Task {name}: {e}"))?;
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
        .map(|(name, t)| {
            let mut descriptor = json!({"name":name,"description":t.description});
            if let Some(snapshot) = &t.snapshot {
                descriptor["snapshot"] = json!(true);
                descriptor["execution"] = json!(snapshot.mode);
            }
            descriptor
        })
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
            let response = self.post(
                &format!("/api/worker/jobs/{}/events", pending.job_id),
                event,
            )?;
            let rejected_start = response["ignored"].as_bool() == Some(true)
                && event.status.as_deref() == Some("running")
                && !pending.finished;
            pending.events.remove(0);
            pending.save(path)?;
            if rejected_start {
                return Err("HTTP 409: job no longer executable".into());
            }
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
        if task.snapshot.is_some() != job.snapshot_id.is_some() {
            pending.finished = true;
            pending.add(
                Some("Snapshot input must match the owner's explicit task opt-in.".into()),
                Some("failed"),
                Some(-1),
            );
            pending.save(state_path)?;
            return self.deliver(&mut pending, state_path);
        }
        if let Some(snapshot) = &task.snapshot {
            return self.execute_snapshot(&job, task, snapshot, pending, state_path);
        }
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
            return Err(format!(
                "Job start was not acknowledged; nothing was executed and pending state was retained: {e}"
            ));
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
fn validate_snapshot_task(snapshot: &SnapshotTask) -> Result<()> {
    if snapshot.prepare.len() > 16 {
        return Err("Snapshot preparation allows at most 16 steps".into());
    }
    for argv in &snapshot.prepare {
        if argv.is_empty()
            || argv[0].is_empty()
            || argv.len() > 128
            || argv
                .iter()
                .any(|arg| arg.len() > 16384 || arg.contains('\0'))
        {
            return Err("Snapshot prepare must contain bounded nonempty argv lists".into());
        }
    }
    match snapshot.mode {
        SnapshotMode::Native => {
            if snapshot.image.is_some() || snapshot.network != SnapshotNetwork::None {
                return Err("image/network options apply only to Docker; native tasks have the worker account's network access".into());
            }
        }
        SnapshotMode::Docker => {
            let image = snapshot.image.as_deref().unwrap_or("");
            if image.is_empty()
                || image.len() > 256
                || image.starts_with('-')
                || !image
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-/:@".contains(&b))
            {
                return Err(
                    "Docker snapshot tasks require an owner-configured image reference".into(),
                );
            }
        }
    }
    Ok(())
}

struct SnapshotFailure {
    message: String,
    status: &'static str,
    stopped: bool,
    rejected: bool,
}
impl SnapshotFailure {
    fn failed(message: impl std::fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            status: "failed",
            stopped: false,
            rejected: false,
        }
    }
    fn cancelled(message: impl std::fmt::Display, stopped: bool, rejected: bool) -> Self {
        Self {
            message: message.to_string(),
            status: "cancelled",
            stopped,
            rejected,
        }
    }
}
type SnapshotResult<T> = std::result::Result<T, SnapshotFailure>;

/// Tracks one deadline and heartbeat throughout download, preparation, and execution.
struct SnapshotRun<'a> {
    connection: &'a Connection,
    job: &'a Job,
    pending: &'a mut Pending,
    state_path: &'a Path,
    started: Instant,
    timeout: Duration,
    heartbeat: Instant,
    last_contact: Instant,
    logs: Arc<Mutex<Logs>>,
}
impl SnapshotRun<'_> {
    fn check_limits(&self) -> SnapshotResult<()> {
        if STOP.load(Ordering::Relaxed) {
            return Err(SnapshotFailure::cancelled("Worker stopped", true, false));
        }
        if self.started.elapsed() >= self.timeout {
            return Err(SnapshotFailure::failed(
                "Task timeout exceeded (including snapshot download and preparation)",
            ));
        }
        if self.last_contact.elapsed() >= Duration::from_secs(60) {
            return Err(SnapshotFailure::failed(
                "Master unreachable for 60 seconds; stopping task",
            ));
        }
        Ok(())
    }
    fn tick(&mut self, force: bool) -> SnapshotResult<()> {
        self.check_limits()?;
        if force
            || self.heartbeat.elapsed() >= Duration::from_secs(self.connection.config.poll_seconds)
        {
            if let Some(chunk) = self.logs.lock().unwrap().drain() {
                self.pending.add(Some(chunk), None, None);
                self.pending
                    .save(self.state_path)
                    .map_err(SnapshotFailure::failed)?;
            }
            match self.connection.poll(Some(&self.job.id)) {
                Ok(poll) => {
                    self.last_contact = Instant::now();
                    if poll.cancel {
                        return Err(SnapshotFailure::cancelled(
                            "Master cancelled job",
                            false,
                            false,
                        ));
                    }
                }
                Err(e) if permanent(&e) => {
                    return Err(SnapshotFailure::cancelled(
                        format!("Master rejected worker heartbeat: {e}"),
                        false,
                        true,
                    ))
                }
                Err(e) => eprintln!("Heartbeat failed: {e}"),
            }
            self.check_limits()?;
            if let Err(e) = self
                .connection
                .flush_limit(self.pending, self.state_path, 1)
            {
                if permanent(&e) {
                    return Err(SnapshotFailure::cancelled(
                        format!("Master rejected worker events: {e}"),
                        false,
                        true,
                    ));
                }
            }
            self.heartbeat = Instant::now();
        }
        self.check_limits()
    }

    fn get(&mut self, path: String) -> SnapshotResult<Vec<u8>> {
        self.tick(false)?;
        let server = self.connection.config.server.clone();
        let token = self.connection.config.token.clone();
        #[cfg(test)]
        let mock = self.connection.mock.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        // Only the transport thread owns network IO. It cannot write files or
        // launch anything after cancellation; the waiting thread maintains leases.
        std::thread::spawn(move || {
            #[cfg(test)]
            if let Some(mock) = mock {
                let result = mock(&path, &json!({"method":"GET"})).and_then(|value| {
                    if path.ends_with("/snapshot") {
                        serde_json::to_vec(&value).map_err(error)
                    } else {
                        serde_json::from_value::<Vec<u8>>(value).map_err(error)
                    }
                });
                let _ = sender.send(result);
                return;
            }
            let _ = sender.send(crate::http::client_raw(
                &server,
                &path,
                Some(&token),
                "GET",
                "application/octet-stream",
                &[],
            ));
        });
        loop {
            self.tick(false)?;
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => {
                    self.tick(false)?;
                    return result.map_err(|e| {
                        if permanent(&e) {
                            SnapshotFailure::cancelled(
                                format!("Snapshot download rejected: {e}"),
                                false,
                                false,
                            )
                        } else {
                            SnapshotFailure::failed(format!("Snapshot download failed: {e}"))
                        }
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => {
                    return Err(SnapshotFailure::failed(
                        "Snapshot transport stopped unexpectedly",
                    ))
                }
            }
        }
    }

    fn download(&mut self, task: &Task) -> SnapshotResult<SnapshotWorkspace> {
        use sharebox::snapshot::{SnapshotManifest, CHUNK_BYTES, MAX_MANIFEST_BYTES};
        let snapshot_id = self
            .job
            .snapshot_id
            .as_deref()
            .ok_or_else(|| SnapshotFailure::failed("Missing snapshot ID"))?;
        if snapshot_id.len() != 64
            || !snapshot_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(SnapshotFailure::failed("Invalid snapshot ID"));
        }
        let prefix = format!("/api/worker/jobs/{}/snapshot", self.job.id);
        let raw = self.get(prefix.clone())?;
        if raw.len() > MAX_MANIFEST_BYTES {
            return Err(SnapshotFailure::failed(
                "Snapshot manifest exceeds size limit",
            ));
        }
        let manifest: SnapshotManifest =
            serde_json::from_slice(&raw).map_err(SnapshotFailure::failed)?;
        let actual_id =
            sharebox::snapshot::snapshot_id(&manifest).map_err(SnapshotFailure::failed)?;
        if actual_id != snapshot_id {
            return Err(SnapshotFailure::failed(
                "Snapshot manifest ID does not match assigned job",
            ));
        }
        let workspace =
            SnapshotWorkspace::create(&task.cwd, &self.job.id).map_err(SnapshotFailure::failed)?;
        for (index, file) in manifest.files.iter().enumerate() {
            let mut contents = Vec::with_capacity(file.size as usize);
            for chunk in 0..(file.size as usize).div_ceil(CHUNK_BYTES) {
                let bytes = self.get(format!("{prefix}/files/{index}/chunks/{chunk}"))?;
                let expected = CHUNK_BYTES.min(file.size as usize - contents.len());
                if bytes.len() != expected {
                    return Err(SnapshotFailure::failed(format!(
                        "Snapshot file {index} chunk size mismatch"
                    )));
                }
                contents.extend_from_slice(&bytes);
            }
            if contents.len() as u64 != file.size
                || sharebox::snapshot::hash_bytes(&contents) != file.sha256
            {
                return Err(SnapshotFailure::failed(format!(
                    "Snapshot file {index} SHA-256 mismatch"
                )));
            }
            self.tick(false)?;
            workspace
                .write(&file.path, &contents, file.executable)
                .map_err(SnapshotFailure::failed)?;
        }
        self.logs.lock().unwrap().append(
            format!(
                "[sharebox] Verified snapshot {snapshot_id} ({} files) in a fresh workspace.\n",
                manifest.files.len()
            )
            .as_bytes(),
        );
        Ok(workspace)
    }

    fn pipeline(&mut self, task: &Task, snapshot: &SnapshotTask) -> SnapshotResult<i32> {
        self.tick(true)?;
        let mut workspace = self.download(task)?;
        if snapshot.mode == SnapshotMode::Docker {
            workspace
                .prepare_docker_owner()
                .map_err(SnapshotFailure::failed)?;
        }
        let mut docker = if snapshot.mode == SnapshotMode::Docker {
            Some(DockerGuard::new(&self.job.id))
        } else {
            None
        };
        let result = (|| {
            if let Some(container) = docker.as_ref() {
                let command = docker_command(snapshot, &workspace, &container.name)
                    .map_err(SnapshotFailure::failed)?;
                let code = self.run_command(command, &workspace)?;
                if code != 0 {
                    return Ok(code);
                }
                // The readonly input tree was completely validated before this
                // fixed copy. Uploaded source never becomes a host shell string.
                let copy = vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "cp -R /source/. /workspace/".into(),
                ];
                let code = self.step(&copy, &workspace, Some(container))?;
                if code != 0 {
                    return Ok(code);
                }
            }
            for (index, argv) in snapshot.prepare.iter().enumerate() {
                self.logs.lock().unwrap().append(
                    format!(
                        "[sharebox] Preparation step {} of {}\n",
                        index + 1,
                        snapshot.prepare.len()
                    )
                    .as_bytes(),
                );
                let code = self.step(argv, &workspace, docker.as_ref())?;
                if code != 0 {
                    return Ok(code);
                }
            }
            self.logs
                .lock()
                .unwrap()
                .append(b"[sharebox] Running allowed task in verified snapshot workspace.\n");
            self.step(&task.argv, &workspace, docker.as_ref())
        })();
        if let Some(container) = docker.as_mut() {
            if let Err(e) = container.remove() {
                workspace.remove_on_drop = false;
                return Err(SnapshotFailure::failed(format!(
                    "Container cleanup failed; readonly input retained at {} for review: {e}",
                    workspace.path.display()
                )));
            }
        }
        result
    }

    fn step(
        &mut self,
        argv: &[String],
        workspace: &SnapshotWorkspace,
        container: Option<&DockerGuard>,
    ) -> SnapshotResult<i32> {
        let command = if let Some(container) = container {
            docker_exec(&container.name, argv)
        } else {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            command
        };
        self.run_command(command, workspace)
    }

    fn run_command(
        &mut self,
        mut command: Command,
        workspace: &SnapshotWorkspace,
    ) -> SnapshotResult<i32> {
        self.tick(true)?;
        command
            .current_dir(&workspace.path)
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
            command.creation_flags(0x08000200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        }
        self.check_limits()?;
        let mut child = command
            .spawn()
            .map_err(|e| SnapshotFailure::failed(format!("Unable to start snapshot task: {e}")))?;
        let pid = child.id();
        let mut process = ProcessGuard(Some(pid));
        let stdout_stream = child
            .stdout
            .take()
            .ok_or_else(|| SnapshotFailure::failed("Missing process stdout"))?;
        let stderr_stream = child
            .stderr
            .take()
            .ok_or_else(|| SnapshotFailure::failed("Missing process stderr"))?;
        let logs = self.logs.clone();
        let stdout = std::thread::spawn(move || capture(stdout_stream, logs));
        let logs = self.logs.clone();
        let stderr = std::thread::spawn(move || capture(stderr_stream, logs));
        let outcome = loop {
            if let Err(e) = self.tick(false) {
                break Err(e);
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status.code().unwrap_or(-1)),
                Ok(None) => {}
                Err(e) => break Err(SnapshotFailure::failed(format!("Wait failed: {e}"))),
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        kill_tree(pid);
        let _ = child.kill();
        let _ = child.wait();
        process.0 = None;
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
        outcome
    }
}

impl Connection {
    fn execute_snapshot(
        &self,
        job: &Job,
        task: &Task,
        snapshot: &SnapshotTask,
        mut pending: Pending,
        state_path: &Path,
    ) -> Result<()> {
        validate_snapshot_task(snapshot)?;
        if job.id.is_empty()
            || job.id.len() > 128
            || !job
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("Invalid job ID".into());
        }
        let timeout = if job.timeout_seconds == 0 {
            task.timeout_seconds
        } else {
            task.timeout_seconds.min(job.timeout_seconds)
        };
        pending.add(
            Some(format!(
                "[sharebox] Starting owner-configured {:?} snapshot task {}\n",
                snapshot.mode, job.task
            )),
            Some("running"),
            None,
        );
        pending.save(state_path)?;
        if let Err(e) = self.flush(&mut pending, state_path) {
            return Err(format!("Job start was not acknowledged; nothing was executed and pending state was retained: {e}"));
        }
        let logs = Arc::new(Mutex::new(Logs::default()));
        let mut run = SnapshotRun {
            connection: self,
            job,
            pending: &mut pending,
            state_path,
            started: Instant::now(),
            timeout: Duration::from_secs(timeout),
            heartbeat: Instant::now(),
            last_contact: Instant::now(),
            logs: logs.clone(),
        };
        let result = run.pipeline(task, snapshot);
        let (status, code, stopped, rejected) = match result {
            Ok(code) => (
                if code == 0 { "passed" } else { "failed" },
                code,
                false,
                false,
            ),
            Err(failure) => {
                logs.lock()
                    .unwrap()
                    .append(format!("\n[sharebox] {}\n", failure.message).as_bytes());
                (failure.status, -1, failure.stopped, failure.rejected)
            }
        };
        while let Some(chunk) = logs.lock().unwrap().drain() {
            pending.add(Some(chunk), None, None);
        }
        pending.finished = true;
        pending.add(None, Some(status), Some(code));
        pending.save(state_path)?;
        if rejected {
            return Err(
                "Worker authorization/protocol rejected; task stopped and result retained".into(),
            );
        }
        if stopped {
            let _ = self.flush(&mut pending, state_path);
            return Err("worker stopped; any unacknowledged result is saved locally".into());
        }
        self.deliver(&mut pending, state_path)
    }
}

struct SnapshotWorkspace {
    path: PathBuf,
    remove_on_drop: bool,
}
fn real_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Snapshot workspace directories must not be links".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("Snapshot workspace directories must not be reparse points".into());
        }
    }
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(error)
}
impl SnapshotWorkspace {
    fn create(base: &Path, job: &str) -> Result<Self> {
        let base = base.canonicalize().map_err(error)?;
        let parent = base.join(".sharebox-snapshots");
        if !parent.exists() {
            private_directory(&parent)?;
        }
        real_directory(&parent)?;
        // Retained inputs are bounded even if Docker becomes unavailable during
        // cleanup. Never delete unknown or potentially still-mounted directories.
        if std::fs::read_dir(&parent).map_err(error)?.take(8).count() >= 8 {
            return Err("Snapshot workspace retention limit reached (8 entries). Recover Docker and review/remove retained workspaces before accepting more jobs.".into());
        }
        let path = parent.join(format!("{job}-{}", nonce()));
        private_directory(&path)?;
        Ok(Self {
            path,
            remove_on_drop: true,
        })
    }
    fn write(&self, relative: &str, bytes: &[u8], executable: bool) -> Result<()> {
        use std::io::Write;
        sharebox::snapshot::validate_path(relative)?;
        let mut parent = self.path.clone();
        let mut components = relative.split('/').peekable();
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(if executable { 0o700 } else { 0o600 });
                }
                #[cfg(not(unix))]
                let _ = executable;
                let mut file = options.open(parent.join(component)).map_err(error)?;
                file.write_all(bytes)
                    .and_then(|_| file.sync_all())
                    .map_err(error)?;
            } else {
                parent.push(component);
                if !parent.exists() {
                    private_directory(&parent)?;
                }
                real_directory(&parent)?;
            }
        }
        Ok(())
    }
    fn prepare_docker_owner(&self) -> Result<()> {
        #[cfg(unix)]
        if unsafe { libc::geteuid() } == 0 {
            // Root workers still use a non-root container account. Only this
            // freshly created and verified tree is chowned, before any process runs.
            fn change(path: &Path) -> Result<()> {
                use std::os::unix::ffi::OsStrExt;
                let metadata = std::fs::symlink_metadata(path).map_err(error)?;
                if metadata.file_type().is_symlink() {
                    return Err("Unexpected link in fresh snapshot".into());
                }
                if metadata.is_dir() {
                    for entry in std::fs::read_dir(path).map_err(error)? {
                        change(&entry.map_err(error)?.path())?;
                    }
                }
                let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(error)?;
                if unsafe { libc::chown(raw.as_ptr(), 65534, 65534) } != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                Ok(())
            }
            change(&self.path)?;
        }
        Ok(())
    }
}
impl Drop for SnapshotWorkspace {
    fn drop(&mut self) {
        if self.remove_on_drop {
            if let Err(e) = std::fs::remove_dir_all(&self.path) {
                eprintln!(
                    "Snapshot workspace cleanup failed for {}: {e}",
                    self.path.display()
                );
            }
        }
    }
}

fn docker_command(
    snapshot: &SnapshotTask,
    workspace: &SnapshotWorkspace,
    name: &str,
) -> Result<Command> {
    let path = workspace
        .path
        .to_str()
        .ok_or("Docker workspace path must be valid Unicode")?;
    if path.contains([',', '\r', '\n']) {
        return Err("Docker workspace path cannot contain commas or newlines".into());
    }
    #[cfg(unix)]
    let user = {
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        if uid == 0 {
            "65534:65534".to_owned()
        } else {
            format!("{uid}:{gid}")
        }
    };
    #[cfg(not(unix))]
    let user = "1000:1000".to_owned();
    let mut command = Command::new("docker");
    command.args([
        "run",
        "--detach",
        "--init",
        "--log-driver",
        "none",
        "--rm",
        "--pull",
        "never",
        "--name",
        name,
        "--user",
        &user,
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=256m",
        "--tmpfs",
        &format!(
            "/workspace:rw,nosuid,nodev,size=1g,uid={},gid={},mode=0700",
            user.split(':').next().unwrap(),
            user.split(':').nth(1).unwrap()
        ),
        "--memory",
        "2g",
        "--cpus",
        "2",
        "--pids-limit",
        "256",
        "--network",
        if snapshot.network == SnapshotNetwork::Bridge {
            "bridge"
        } else {
            "none"
        },
        "--env",
        "HOME=/tmp",
        "--env",
        "npm_config_cache=/tmp/.npm",
        "--mount",
        &format!("type=bind,source={path},target=/source,readonly"),
        "--workdir",
        "/workspace",
        "--entrypoint",
        "/bin/sh",
        snapshot
            .image
            .as_deref()
            .ok_or("Docker image is required")?,
    ]);
    command.args(["-c", "while :; do sleep 3600; done"]);
    Ok(command)
}
fn docker_exec(name: &str, argv: &[String]) -> Command {
    let mut command = Command::new("docker");
    command.args(["exec", "--workdir", "/workspace", name]);
    command.args(argv);
    command
}
struct DockerGuard {
    name: String,
    armed: bool,
}
impl DockerGuard {
    fn new(job: &str) -> Self {
        Self {
            name: format!("sharebox-{job}-{}", nonce()),
            armed: true,
        }
    }
    fn remove(&mut self) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        let mut command = Command::new("docker");
        command
            .args(["rm", "--force", &self.name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().map_err(error)?;
        let stderr = child.stderr.take().ok_or("Missing Docker cleanup stderr")?;
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.take(8192).read_to_end(&mut bytes);
            bytes
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().map_err(error)? {
                let stderr =
                    String::from_utf8_lossy(&reader.join().unwrap_or_default()).to_lowercase();
                if status.success() || stderr.contains("no such container") {
                    self.armed = false;
                    return Ok(());
                }
                self.armed = false; // caller retains workspace and reports failure
                return Err(format!("docker rm {} failed: {stderr}", self.name));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                self.armed = false;
                return Err(format!(
                    "docker rm {} timed out after 10 seconds",
                    self.name
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for DockerGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.remove();
        }
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
                snapshot: None,
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
        "Worker {} ready: {} allowed tasks, one execution slot (owner-configured local or snapshot tasks).",
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
                        snapshot: None,
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
                    snapshot_id: None,
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
                    snapshot_id: None,
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
                    snapshot_id: None,
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
                    snapshot_id: None,
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
                snapshot_id: None,
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
                    timeout_seconds: 5,
                    snapshot_id: None,
                },
                &d.join("pending.json")
            )
            .is_err());
        assert!(!d.join("executed").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn ignored_or_unacknowledged_start_never_executes() {
        for response in [
            Ok(json!({"ok":true,"ignored":true,"reason":"Job already terminal"})),
            Err("Connection timed out".to_string()),
        ] {
            let server = FakeServer::start(false);
            let d = dir();
            let mut connection = runner(
                &server,
                &d,
                "require('node:fs').writeFileSync('executed','bad')",
                5,
            );
            connection.mock = Some(Arc::new(move |_, _| response.clone()));
            let pending_path = d.join("pending.json");
            let result = connection.execute(
                Job {
                    id: "job-terminal-start".into(),
                    task: "test".into(),
                    timeout_seconds: 5,
                    snapshot_id: None,
                },
                &pending_path,
            );
            assert!(result.unwrap_err().contains("nothing was executed"));
            assert!(!d.join("executed").exists());
            assert!(pending_path.exists());
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn terminal_recovery_drains_ignored_events_without_restarting() {
        let server = FakeServer::start(false);
        let d = dir();
        let mut connection = runner(&server, &d, "process.exit(99)", 5);
        connection.mock = Some(Arc::new(|_, _| Ok(json!({"ok":true,"ignored":true}))));
        let mut pending = Pending {
            job_id: "job-recovery".into(),
            sequence: 0,
            finished: true,
            events: vec![],
        };
        pending.add(None, Some("running"), None);
        pending.add(None, Some("failed"), Some(-1));
        let path = d.join("pending.json");
        pending.save(&path).unwrap();
        connection.deliver(&mut pending, &path).unwrap();
        assert!(!path.exists());
        assert!(pending.events.is_empty());
        std::fs::remove_dir_all(d).unwrap();
    }
    fn snapshot_config() -> SnapshotTask {
        SnapshotTask {
            mode: SnapshotMode::Native,
            image: None,
            prepare: vec![],
            network: SnapshotNetwork::None,
        }
    }
    fn snapshot_job(manifest: &sharebox::snapshot::SnapshotManifest) -> Job {
        Job {
            id: "job-snapshot".into(),
            task: "test".into(),
            timeout_seconds: 5,
            snapshot_id: Some(sharebox::snapshot::snapshot_id(manifest).unwrap()),
        }
    }
    fn manifest(path: &str, bytes: &[u8]) -> sharebox::snapshot::SnapshotManifest {
        sharebox::snapshot::SnapshotManifest {
            version: 1,
            files: vec![sharebox::snapshot::SnapshotFile {
                path: path.into(),
                size: bytes.len() as u64,
                sha256: sharebox::snapshot::hash_bytes(bytes),
                executable: false,
            }],
        }
    }
    fn snapshot_runner(
        server: &FakeServer,
        d: &Path,
        script: &str,
        manifest: sharebox::snapshot::SnapshotManifest,
        bytes: Vec<u8>,
        cancel_after_polls: usize,
    ) -> Connection {
        let mut connection = runner(server, d, script, 5);
        connection.config.tasks.get_mut("test").unwrap().snapshot = Some(snapshot_config());
        let events = server.events.clone();
        let polls = std::sync::atomic::AtomicUsize::new(0);
        connection.mock = Some(Arc::new(move |path, value| {
            if path.ends_with("/events") {
                events.lock().unwrap().push(value.clone());
                Ok(json!({"ok":true}))
            } else if path.ends_with("/snapshot") {
                Ok(json!(manifest))
            } else if path.contains("/chunks/") {
                Ok(json!(bytes))
            } else {
                Ok(
                    json!({"job":null,"cancel":polls.fetch_add(1, Ordering::SeqCst) >= cancel_after_polls}),
                )
            }
        }));
        connection
    }
    fn assert_no_workspaces(d: &Path) {
        let parent = d.join(".sharebox-snapshots");
        assert!(!parent.exists() || std::fs::read_dir(parent).unwrap().next().is_none());
    }
    #[test]
    fn snapshot_executes_uploaded_changes_and_preparation_in_fresh_workspace() {
        let d = dir();
        std::fs::write(d.join("input.txt"), "old checkout").unwrap();
        let bytes = b"new uncommitted source".to_vec();
        let manifest = manifest("input.txt", &bytes);
        let job = snapshot_job(&manifest);
        let server = FakeServer::start(false);
        let mut connection = snapshot_runner(&server, &d, "const fs=require('node:fs');if(fs.readFileSync('input.txt','utf8')!=='new uncommitted source'||fs.readFileSync('prepared','utf8')!=='yes')process.exit(42);console.log('SNAPSHOT_EXACT_OK')", manifest, bytes, usize::MAX);
        connection
            .config
            .tasks
            .get_mut("test")
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .prepare = vec![vec![
            "node".into(),
            "-e".into(),
            "require('node:fs').writeFileSync('prepared','yes')".into(),
        ]];
        connection.execute(job, &d.join("pending.json")).unwrap();
        let events = server.events.lock().unwrap();
        assert_eq!(events.last().unwrap()["status"], "passed");
        assert!(events.iter().any(|e| e["log"]
            .as_str()
            .unwrap_or("")
            .contains("SNAPSHOT_EXACT_OK")));
        assert_eq!(
            std::fs::read_to_string(d.join("input.txt")).unwrap(),
            "old checkout"
        );
        assert!(!d.join("prepared").exists());
        assert_no_workspaces(&d);
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn snapshot_input_requires_owner_opt_in_and_is_mandatory_for_snapshot_tasks() {
        for opt_in in [false, true] {
            let d = dir();
            let server = FakeServer::start(false);
            let mut connection = runner(
                &server,
                &d,
                "require('node:fs').writeFileSync('executed','bad')",
                5,
            );
            if opt_in {
                connection.config.tasks.get_mut("test").unwrap().snapshot = Some(snapshot_config());
            }
            connection
                .execute(
                    Job {
                        id: "job-mismatch".into(),
                        task: "test".into(),
                        timeout_seconds: 5,
                        snapshot_id: if opt_in { None } else { Some("a".repeat(64)) },
                    },
                    &d.join("pending.json"),
                )
                .unwrap();
            let events = server.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["status"], "failed");
            assert!(!d.join("executed").exists());
            assert_no_workspaces(&d);
            std::fs::remove_dir_all(d).unwrap();
        }
    }
    #[test]
    fn corrupt_or_traversing_snapshots_never_execute_owner_command() {
        for traversal in [false, true] {
            let d = dir();
            let server = FakeServer::start(false);
            let mut source = manifest("input.txt", b"correct");
            let job = snapshot_job(&source);
            if traversal {
                source.files[0].path = "../escape".into();
            }
            let marker = serde_json::to_string(&d.join("executed").to_string_lossy()).unwrap();
            let connection = snapshot_runner(
                &server,
                &d,
                &format!("require('node:fs').writeFileSync({marker},'bad')"),
                source,
                b"corrupt".to_vec(),
                usize::MAX,
            );
            connection.execute(job, &d.join("pending.json")).unwrap();
            let events = server.events.lock().unwrap();
            assert_eq!(events.last().unwrap()["status"], "failed");
            assert!(!d.join("executed").exists());
            assert_no_workspaces(&d);
            std::fs::remove_dir_all(d).unwrap();
        }
    }
    #[test]
    fn cancellation_during_snapshot_preparation_prevents_final_task() {
        let d = dir();
        let server = FakeServer::start(false);
        let source = manifest("input.txt", b"ok");
        let job = snapshot_job(&source);
        let marker = serde_json::to_string(&d.join("executed").to_string_lossy()).unwrap();
        let mut connection = snapshot_runner(
            &server,
            &d,
            &format!("require('node:fs').writeFileSync({marker},'bad')"),
            source,
            b"ok".to_vec(),
            2,
        );
        connection
            .config
            .tasks
            .get_mut("test")
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .prepare = vec![vec![
            "node".into(),
            "-e".into(),
            "console.log('PREP_STARTED');setInterval(()=>{},1000)".into(),
        ]];
        let began = Instant::now();
        connection.execute(job, &d.join("pending.json")).unwrap();
        assert!(began.elapsed() < Duration::from_secs(4));
        let events = server.events.lock().unwrap();
        assert_eq!(events.last().unwrap()["status"], "cancelled");
        assert!(events
            .iter()
            .any(|e| e["log"].as_str().unwrap_or("").contains("PREP_STARTED")));
        assert!(!d.join("executed").exists());
        assert_no_workspaces(&d);
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn snapshot_download_is_inside_job_deadline_and_cannot_execute_after_timeout() {
        let d = dir();
        let server = FakeServer::start(false);
        let source = manifest("input.txt", b"ok");
        let mut job = snapshot_job(&source);
        job.timeout_seconds = 1;
        let mut connection = runner(
            &server,
            &d,
            "require('node:fs').writeFileSync('executed','bad')",
            5,
        );
        connection.config.tasks.get_mut("test").unwrap().snapshot = Some(snapshot_config());
        let events = server.events.clone();
        connection.mock = Some(Arc::new(move |path, value| {
            if path.ends_with("/snapshot") {
                std::thread::sleep(Duration::from_secs(4));
                Ok(json!(source))
            } else if path.ends_with("/events") {
                events.lock().unwrap().push(value.clone());
                Ok(json!({"ok":true}))
            } else {
                Ok(json!({"job":null,"cancel":false}))
            }
        }));
        let began = Instant::now();
        connection.execute(job, &d.join("pending.json")).unwrap();
        assert!(began.elapsed() < Duration::from_secs(3));
        assert_eq!(
            server.events.lock().unwrap().last().unwrap()["status"],
            "failed"
        );
        assert!(!d.join("executed").exists());
        assert_no_workspaces(&d);
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn snapshot_start_acknowledgment_precedes_download_and_execution() {
        let d = dir();
        let server = FakeServer::start(false);
        let source = manifest("input.txt", b"ok");
        let job = snapshot_job(&source);
        let mut connection = runner(&server, &d, "process.exit(99)", 5);
        connection.config.tasks.get_mut("test").unwrap().snapshot = Some(snapshot_config());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        connection.mock = Some(Arc::new(move |path, _| {
            observed.lock().unwrap().push(path.to_owned());
            Ok(json!({"ok":true,"ignored":true}))
        }));
        assert!(connection
            .execute(job, &d.join("pending.json"))
            .unwrap_err()
            .contains("nothing was executed"));
        assert_eq!(
            requests.lock().unwrap().as_slice(),
            ["/api/worker/jobs/job-snapshot/events"]
        );
        assert_no_workspaces(&d);
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn delayed_prelaunch_heartbeat_cannot_start_command_after_deadline() {
        let d = dir();
        let server = FakeServer::start(false);
        let source = manifest("input.txt", b"ok");
        let mut job = snapshot_job(&source);
        job.timeout_seconds = 1;
        let marker = serde_json::to_string(&d.join("executed").to_string_lossy()).unwrap();
        let mut connection = snapshot_runner(
            &server,
            &d,
            &format!("require('node:fs').writeFileSync({marker},'bad')"),
            source,
            b"ok".to_vec(),
            usize::MAX,
        );
        let original = connection.mock.take().unwrap();
        let polls = std::sync::atomic::AtomicUsize::new(0);
        connection.mock = Some(Arc::new(move |path, value| {
            if path.ends_with("/poll") && polls.fetch_add(1, Ordering::SeqCst) == 1 {
                std::thread::sleep(Duration::from_millis(1200));
            }
            original(path, value)
        }));
        connection.execute(job, &d.join("pending.json")).unwrap();
        assert_eq!(
            server.events.lock().unwrap().last().unwrap()["status"],
            "failed"
        );
        assert!(!d.join("executed").exists());
        assert_no_workspaces(&d);
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn snapshot_configuration_and_docker_command_keep_owner_boundaries() {
        assert!(serde_json::from_value::<SnapshotTask>(
            json!({"mode":"native","command":"remote"})
        )
        .is_err());
        assert!(serde_json::from_value::<SnapshotTask>(json!({"mode":"unknown"})).is_err());
        let mut snapshot = snapshot_config();
        snapshot.mode = SnapshotMode::Docker;
        assert!(validate_snapshot_task(&snapshot).is_err());
        snapshot.image = Some("node:22-bookworm-slim".into());
        validate_snapshot_task(&snapshot).unwrap();
        let d = dir();
        let workspace = SnapshotWorkspace::create(&d, "job-docker-test").unwrap();
        let command = docker_command(&snapshot, &workspace, "sharebox-test").unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|x| x.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
        assert!(args.windows(2).any(|pair| pair == ["--pull", "never"]));
        assert!(args.windows(2).any(|pair| pair == ["--cap-drop", "ALL"]));
        assert!(args.windows(2).any(|pair| pair == ["--env", "HOME=/tmp"]));
        assert!(!args.windows(2).any(|pair| pair == ["--user", "0:0"]));
        assert!(!args.iter().any(|arg| arg.contains("docker.sock")));
        assert!(args.windows(2).any(|pair| pair == ["--log-driver", "none"]));
        assert!(args
            .iter()
            .any(|arg| arg.ends_with("target=/source,readonly")));
        assert!(args
            .iter()
            .any(|arg| arg.starts_with("/workspace:rw,nosuid,nodev,size=1g,")));
        assert!(!args.iter().any(|arg| arg.contains("target=/workspace")));
        let exec = docker_exec("sharebox-test", &["node".into(), "test.cjs".into()]);
        let exec_args: Vec<_> = exec
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            exec_args,
            [
                "exec",
                "--workdir",
                "/workspace",
                "sharebox-test",
                "node",
                "test.cjs"
            ]
        );

        assert_eq!(
            &args[args.len() - 3..],
            [
                "node:22-bookworm-slim",
                "-c",
                "while :; do sleep 3600; done"
            ]
        );
        drop(workspace);
        std::fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn snapshot_workspace_retention_is_bounded_without_deleting_existing_entries() {
        let d = dir();
        let parent = d.join(".sharebox-snapshots");
        std::fs::create_dir(&parent).unwrap();
        for index in 0..8 {
            std::fs::create_dir(parent.join(format!("retained-{index}"))).unwrap();
        }
        assert!(SnapshotWorkspace::create(&d, "job-new")
            .err()
            .unwrap()
            .contains("retention limit"));
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 8);
        std::fs::remove_dir_all(d).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn snapshot_materialization_refuses_linked_workspace_parent() {
        let d = dir();
        let elsewhere = dir();
        std::os::unix::fs::symlink(&elsewhere, d.join(".sharebox-snapshots")).unwrap();
        assert!(SnapshotWorkspace::create(&d, "job-link").is_err());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
        std::fs::remove_dir_all(d).unwrap();
        std::fs::remove_dir_all(elsewhere).unwrap();
    }
}
