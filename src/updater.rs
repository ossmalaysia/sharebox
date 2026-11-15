//! Owner-triggered updates from the official GitHub release channel.
// Linux can check releases; installation is exposed only by native desktop trays.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;
const REPO: &str = "https://github.com/ossmalaysia/sharebox/releases/download";
const MAX_ARCHIVE: u64 = 64 * 1024 * 1024;
pub const HELP: &str = "sharebox update --check\n\nCheck the official stable GitHub release. Install updates from the Windows/macOS tray.\nThe tray asks before stopping services, verifies SHA-256, and preserves existing service registrations.";
#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
}
#[derive(Deserialize)]
struct Metadata {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    target: PathBuf,
    tag: String,
    sha256: String,
    previous_sha256: String,
}

fn version(value: &str) -> Result<(u64, u64, u64)> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty()
                || p.len() > 8
                || !p.bytes().all(|c| c.is_ascii_digit())
                || (p.len() > 1 && p.starts_with('0'))
        })
    {
        return Err("Unsupported stable release version".into());
    }
    Ok((
        parts[0].parse().map_err(|_| "Invalid version")?,
        parts[1].parse().map_err(|_| "Invalid version")?,
        parts[2].parse().map_err(|_| "Invalid version")?,
    ))
}
fn release_metadata(bytes: &[u8], current: &str) -> Result<Option<Release>> {
    let meta: Metadata =
        serde_json::from_slice(bytes).map_err(|_| "Invalid GitHub release metadata")?;
    if meta.draft || meta.prerelease {
        return Ok(None);
    }
    if !meta.tag_name.starts_with('v') {
        return Err("Release tag must start with v".into());
    }
    if version(&meta.tag_name)? > version(current)? {
        Ok(Some(Release { tag: meta.tag_name }))
    } else {
        Ok(None)
    }
}
fn process_to_file(mut command: Command, dest: &Path, limit: u64, seconds: u64) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(dest).map_err(|e| e.to_string())?;
    command
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
        .map_err(|e| format!("Start update utility: {e}"))?;
    let stdout = child.stdout.take().ok_or("Missing updater stdout")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = (|| {
            let copied =
                std::io::copy(&mut stdout.take(limit + 1), &mut file).map_err(|e| e.to_string())?;
            if copied > limit {
                return Err("Update output exceeds size limit".to_owned());
            }
            file.sync_all().map_err(|e| e.to_string())
        })();
        let _ = tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut copied = None;
    loop {
        if copied.is_none() {
            match rx.try_recv() {
                Ok(r) => copied = Some(r),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    copied = Some(Err("Update reader stopped".into()))
                }
                _ => {}
            }
        }
        if copied.as_ref().is_some_and(Result::is_err) {
            let _ = child.kill();
            let _ = child.wait();
            return copied.unwrap();
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            if !status.success() {
                return Err(format!("Update utility failed ({status})"));
            }
            return copied.unwrap_or_else(|| {
                rx.recv_timeout(Duration::from_secs(2))
                    .unwrap_or_else(|_| Err("Update reader timed out".into()))
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Update utility timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn download(url: &str, dest: &Path, limit: u64) -> Result<()> {
    // curl's config is disabled and every redirect remains HTTPS. URLs are constructed
    // from a strict stable tag, never taken from remote asset URLs.
    let mut cmd = Command::new(if cfg!(windows) {
        "curl.exe"
    } else {
        "/usr/bin/curl"
    });
    cmd.args([
        "--disable",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "15",
        "--max-time",
        "180",
        "--max-filesize",
        &limit.to_string(),
        "--user-agent",
        "Sharebox-Updater",
    ])
    .arg(url);
    process_to_file(cmd, dest, limit, 190)?;
    if fs::metadata(dest).map_err(|e| e.to_string())?.len() > limit {
        return Err("Update download exceeds its size limit".into());
    }
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("Create private update directory {}: {e}", path.display()))
}
fn nonce() -> Result<String> {
    let mut b = [0; 16];
    getrandom::getrandom(&mut b).map_err(|e| e.to_string())?;
    Ok(b.iter().map(|v| format!("{v:02x}")).collect())
}
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub fn check_latest() -> Result<Option<Release>> {
    let tmp = Temp(std::env::temp_dir().join(format!("sharebox-check-{}", nonce()?)));
    private_directory(&tmp.0)?;
    let file = tmp.0.join("release.json");
    download(
        "https://api.github.com/repos/ossmalaysia/sharebox/releases/latest",
        &file,
        256 * 1024,
    )?;
    release_metadata(
        &fs::read(file).map_err(|e| e.to_string())?,
        env!("CARGO_PKG_VERSION"),
    )
}
fn asset() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("sharebox-aarch64-apple-darwin.tar.gz"),
        ("macos", "x86_64") => Ok("sharebox-x86_64-apple-darwin.tar.gz"),
        ("windows", "x86_64") => Ok("sharebox-x86_64-pc-windows-msvc.zip"),
        _ => Err("Tray updates support Windows x86_64 and macOS only".into()),
    }
}
fn checksum(manifest: &str, asset: &str) -> Result<String> {
    let mut matches = manifest.lines().filter_map(|line| {
        let words = line.split_whitespace().collect::<Vec<_>>();
        if words.len() == 2 && words[1] == asset {
            Some(words[0])
        } else {
            None
        }
    });
    let hash = matches.next().ok_or("Release checksum missing")?;
    if matches.next().is_some() || hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("Release checksum invalid or duplicated".into());
    }
    Ok(hash.to_ascii_lowercase())
}
fn regular(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !m.is_file() {
        return Err("Update input must be a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.nlink() != 1 {
            return Err("Hard-linked update input is forbidden".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return Err("Reparse update input is forbidden".into());
        }
    }
    Ok(())
}
fn hash(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut b = [0; 32768];
    let mut total = 0;
    loop {
        let n = file.read(&mut b).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_ARCHIVE {
            return Err("Update binary exceeds size limit".into());
        }
        hash.update(&b[..n]);
    }
    Ok(hash.finalize().iter().map(|v| format!("{v:02x}")).collect())
}
fn write(path: &Path, data: &[u8]) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| e.to_string())?;
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .map_err(|e| e.to_string())
}
fn extract(archive: &Path, dir: &Path) -> Result<PathBuf> {
    let binary = dir.join(if cfg!(windows) {
        "sharebox.exe"
    } else {
        "sharebox"
    });
    #[cfg(windows)]
    {
        // Extract only the exact expected entry; never unpack archive paths.
        let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let script=format!("$ErrorActionPreference='Stop'; Add-Type -AssemblyName System.IO.Compression.FileSystem; $z=[IO.Compression.ZipFile]::OpenRead({}); try {{ $entries=@($z.Entries | Where-Object {{ $_.FullName -ceq 'sharebox.exe' }}); if ($entries.Count -ne 1 -or $entries[0].Length -gt 67108864) {{ throw 'Invalid executable entry' }}; $input=$entries[0].Open(); try {{ $input.CopyTo([Console]::OpenStandardOutput()) }} finally {{ $input.Dispose() }} }} finally {{ $z.Dispose() }}",quote(&archive.to_string_lossy()));
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ]);
        process_to_file(cmd, &binary, MAX_ARCHIVE, 30)?;
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("/usr/bin/tar");
        cmd.arg("-xOzf").arg(archive).arg("sharebox");
        process_to_file(cmd, &binary, MAX_ARCHIVE, 30)?;
    }
    let meta = fs::symlink_metadata(&binary).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_ARCHIVE {
        return Err("Invalid extracted executable".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    Ok(binary)
}
fn stage_dir(target: &Path) -> Result<PathBuf> {
    Ok(target
        .parent()
        .ok_or("Executable has no parent")?
        .join(".sharebox-update"))
}

struct UpdateLock {
    _file: File,
}
#[cfg(unix)]
impl Drop for UpdateLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // Release explicitly: a concurrently spawned child can briefly inherit
        // the open file description before CLOEXEC closes its copy.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl UpdateLock {
    fn acquire(target: &Path) -> Result<Self> {
        let path = target
            .parent()
            .ok_or("Missing executable parent")?
            .join(".sharebox-update.lock");
        let mut options = OpenOptions::new();
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
        let file = options
            .open(&path)
            .map_err(|e| format!("Another update operation may be active: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err("Another update operation is active".into());
            }
        }
        Ok(Self { _file: file })
    }
}

pub fn install(release: &Release) -> Result<()> {
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = release;
        Err("Tray updates support Windows and macOS only".into())
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        version(&release.tag)?;
        if !release.tag.starts_with('v')
            || version(&release.tag)? <= version(env!("CARGO_PKG_VERSION"))?
        {
            return Err("Update must be a newer stable release".into());
        }
        let target = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let _lock = UpdateLock::acquire(&target)?;
        cleanup_completed_inner(&target)?;
        let dir = stage_dir(&target)?;
        private_directory(&dir).map_err(|e| format!("Another update may be in progress. {e}"))?;
        let prepared = (|| {
            let asset = asset()?;
            let archive = dir.join(asset);
            let sums = dir.join("SHA256SUMS");
            download(&format!("{REPO}/{}/SHA256SUMS", release.tag), &sums, 16384)?;
            let expected = checksum(
                &fs::read_to_string(&sums).map_err(|e| e.to_string())?,
                asset,
            )?;
            download(
                &format!("{REPO}/{}/{asset}", release.tag),
                &archive,
                MAX_ARCHIVE,
            )?;
            if hash(&archive)? != expected {
                return Err("Update checksum mismatch; nothing was changed".into());
            }
            let binary = extract(&archive, &dir)?;
            let plan = Plan {
                target: target.clone(),
                tag: release.tag.clone(),
                sha256: hash(&binary)?,
                previous_sha256: hash(&target)?,
            };
            write(
                &dir.join("plan.json"),
                &serde_json::to_vec(&plan).map_err(|e| e.to_string())?,
            )?;
            let helper = dir.join(if cfg!(windows) {
                "helper.exe"
            } else {
                "helper"
            });
            fs::copy(&target, &helper).map_err(|e| e.to_string())?;
            let result_file = target
                .parent()
                .ok_or("Executable has no parent")?
                .join(".sharebox-update-result");
            if result_file.exists() {
                fs::remove_file(result_file).map_err(|e| e.to_string())?;
            }
            crate::service::launch_update_helper(&helper, &dir.join("plan.json"))
        })();
        if prepared.is_err() {
            let _ = fs::remove_dir_all(&dir);
        }
        prepared
    }
}

pub fn run(args: Vec<String>) -> Result<()> {
    if args == ["--check"] {
        match check_latest()? {
            Some(r) => println!("Update available: {}", r.tag),
            None => println!("Sharebox {} is up to date", env!("CARGO_PKG_VERSION")),
        };
        return Ok(());
    }
    if args.len() == 2 && args[0] == "--apply" {
        return apply(Path::new(&args[1]));
    }
    if args.is_empty() || args == ["--help"] {
        println!("{HELP}");
        return Ok(());
    }
    Err(HELP.into())
}
fn apply(plan_path: &Path) -> Result<()> {
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = plan_path;
        Err("Update helper unsupported on this platform".into())
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let helper = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let dir = helper.parent().ok_or("Helper has no directory")?;
        if dir.file_name() != Some(std::ffi::OsStr::new(".sharebox-update"))
            || plan_path.canonicalize().map_err(|e| e.to_string())? != dir.join("plan.json")
        {
            return Err("Update helper path mismatch".into());
        }
        let meta = fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err("Invalid update staging directory".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
                return Err("Update directory must be private to its owner".into());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err("Reparse update directory forbidden".into());
            }
        }
        let result = (|| {
            regular(plan_path)?;
            let mut bytes = Vec::new();
            File::open(plan_path)
                .map_err(|e| e.to_string())?
                .take(8193)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() > 8192 {
                return Err("Update plan too large".into());
            }
            let plan: Plan = serde_json::from_slice(&bytes).map_err(|_| "Invalid update plan")?;
            if !plan.tag.starts_with('v')
                || version(&plan.tag)? <= version(env!("CARGO_PKG_VERSION"))?
            {
                return Err("Update must be a newer stable release".into());
            }
            if !plan.target.is_absolute()
                || plan
                    .target
                    .parent()
                    .and_then(|p| p.canonicalize().ok())
                    .as_deref()
                    != dir.parent()
            {
                return Err("Update helper path mismatch".into());
            }
            regular(&plan.target)?;
            let plan = Plan {
                target: plan.target.canonicalize().map_err(|e| e.to_string())?,
                ..plan
            };
            let binary = dir.join(if cfg!(windows) {
                "sharebox.exe"
            } else {
                "sharebox"
            });
            regular(&binary)?;
            if hash(&plan.target)? != plan.previous_sha256 {
                return Err("Installed binary changed since staging".into());
            }
            if hash(&binary)? != plan.sha256 {
                return Err("Staged binary changed".into());
            }
            if hash(&helper)? != plan.previous_sha256 {
                return Err("Update helper does not match installed binary".into());
            }
            apply_inner(&plan, &binary, dir)?;
            Ok(plan.tag)
        })();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let message = match &result {
            Ok(tag) => format!("Updated to {tag} ({stamp})"),
            Err(e) => format!("Update failed: {e} ({stamp})"),
        };
        let result_path = dir
            .parent()
            .ok_or("Missing update parent")?
            .join(".sharebox-update-result");
        let text = message.chars().take(1800).collect::<String>();
        let staged = dir.join("result");
        if write(&staged, text.as_bytes()).is_ok() {
            let _ = fs::remove_file(&result_path);
            let _ = fs::rename(&staged, &result_path);
        }
        // Never discard a backup after a failed restoration. Leave the private
        // staging directory for explicit recovery rather than risking the old bytes.
        if result.is_ok() || !dir.join("previous").exists() {
            let _ = write(&dir.join("complete"), b"done");
        }
        let _ = crate::service::cleanup_update_helper();
        result.map(|_| ())
    }
}
#[cfg(any(windows, target_os = "macos"))]
fn apply_inner(plan: &Plan, binary: &Path, dir: &Path) -> Result<()> {
    let states = crate::service::update_service_states(&plan.target)?;
    if states.is_empty() {
        return Err("No running Sharebox service or tray found".into());
    }
    write(
        &dir.join("services.json"),
        &serde_json::to_vec(&states).map_err(|e| e.to_string())?,
    )?;
    let stopped = crate::service::stop_for_update(&states);
    if let Err(e) = stopped {
        let _ = crate::service::restore_after_update(&states);
        return Err(e);
    }
    let backup = dir.join("previous");
    let replaced = (|| {
        regular(binary)?;
        regular(&plan.target)?;
        if hash(binary)? != plan.sha256 || hash(&plan.target)? != plan.previous_sha256 {
            return Err("Update input changed before replacement".into());
        }
        replace(&plan.target, binary, &backup)
    })();
    if let Err(e) = replaced {
        let _ = crate::service::restore_after_update(&states);
        return Err(e);
    }
    if let Err(e) = crate::service::restore_after_update(&states) {
        let _ = crate::service::stop_for_update(&states);
        let rollback = rollback(&plan.target, &backup);
        let restart = crate::service::restore_after_update(&states);
        return Err(format!(
            "Restart failed: {e}; rollback: {}; previous service restart: {}",
            rollback.err().unwrap_or_else(|| "restored".into()),
            restart.err().unwrap_or_else(|| "requested".into())
        ));
    }
    Ok(())
}
fn replace(target: &Path, binary: &Path, backup: &Path) -> Result<()> {
    if backup.exists() {
        return Err("An update backup already exists; recovery is required".into());
    }
    // Rename keeps the old bytes intact and avoids partially overwriting an EXE.
    fs::rename(target, backup).map_err(|e| {
        format!(
            "Cannot move running executable; close foreground Sharebox processes and retry: {e}"
        )
    })?;
    if let Err(e) = fs::rename(binary, target) {
        let restore = fs::rename(backup, target);
        return Err(format!(
            "Replace executable: {e}; restore: {}",
            restore
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "completed".into())
        ));
    }
    Ok(())
}
fn rollback(target: &Path, backup: &Path) -> Result<()> {
    fs::remove_file(target).map_err(|e| e.to_string())?;
    fs::rename(backup, target).map_err(|e| e.to_string())
}
/// Called during tray startup. Only completed update staging is removed.
pub fn cleanup_completed() -> Result<()> {
    let target = std::env::current_exe().map_err(|e| e.to_string())?;
    let _lock = UpdateLock::acquire(&target)?;
    cleanup_completed_inner(&target)
}
fn cleanup_completed_inner(target: &Path) -> Result<()> {
    let dir = stage_dir(target)?;
    if dir.join("complete").is_file() {
        if fs::symlink_metadata(&dir)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("Update staging directory must not be a symlink".into());
        }
        for _ in 0..20 {
            if fs::remove_dir_all(&dir).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        return Err("Completed updater is still exiting; retry shortly".into());
    }
    Ok(())
}
pub fn last_result() -> Option<String> {
    let p = std::env::current_exe()
        .ok()?
        .parent()?
        .join(".sharebox-update-result");
    let mut b = Vec::new();
    File::open(p).ok()?.take(2048).read_to_end(&mut b).ok()?;
    String::from_utf8(b).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utility_stdout_cannot_exceed_limit() {
        let t =
            Temp(std::env::temp_dir().join(format!("sharebox-output-test-{}", nonce().unwrap())));
        private_directory(&t.0).unwrap();
        let output = t.0.join("out");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.arg("--help");
        assert!(process_to_file(command, &output, 1, 5).is_err());
        assert!(fs::metadata(output).unwrap().len() <= 2);
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    #[ignore = "requires a built application and interactive service manager"]
    fn native_helper_launcher() {
        let Some(binary) = std::env::var_os("SHAREBOX_TEST_UPDATE_BINARY") else {
            eprintln!("SKIP: SHAREBOX_TEST_UPDATE_BINARY is unset");
            return;
        };
        match crate::service::update_launcher_test_available() {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("SKIP: no interactive service manager or an updater already exists");
                return;
            }
            Err(e) => {
                eprintln!("SKIP: cannot inspect service manager: {e}");
                return;
            }
        }
        let t =
            Temp(std::env::temp_dir().join(format!("sharebox-launcher-test-{}", nonce().unwrap())));
        private_directory(&t.0).unwrap();
        let root = t.0.canonicalize().unwrap();
        let stage = root.join(".sharebox-update");
        private_directory(&stage).unwrap();
        let helper = stage.join(if cfg!(windows) {
            "helper.exe"
        } else {
            "helper"
        });
        fs::copy(PathBuf::from(binary).canonicalize().unwrap(), &helper).unwrap();
        let plan = stage.join("plan.json");
        write(&plan, b"{").unwrap();
        let launch = crate::service::launch_update_helper(&helper, &plan);
        if let Err(e) = launch {
            let _ = crate::service::cleanup_launcher_test(&helper);
            panic!("Launch independent helper: {e}");
        }
        let outcome = root.join(".sharebox-update-result");
        let deadline = Instant::now() + Duration::from_secs(45);
        while !stage.join("complete").is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        // The invalid plan is rejected before any service state is read or changed.
        let result = fs::read_to_string(&outcome);
        std::thread::sleep(Duration::from_secs(1));
        crate::service::cleanup_launcher_test(&helper).unwrap();
        let message = result.expect("Independent helper did not persist a result");
        assert!(message.contains("Invalid update plan"), "{message}");
        assert!(stage.join("complete").is_file());
        for _ in 0..20 {
            if fs::remove_dir_all(&stage).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            !stage.exists(),
            "Launcher test left a locked staging directory"
        );
    }
    #[test]
    fn update_lock_serializes_cleanup_and_new_install() {
        let t = Temp(std::env::temp_dir().join(format!("sharebox-lock-test-{}", nonce().unwrap())));
        private_directory(&t.0).unwrap();
        let target = t.0.join("sharebox");
        let held = UpdateLock::acquire(&target).unwrap();
        assert!(UpdateLock::acquire(&target).is_err());
        drop(held);
        assert!(UpdateLock::acquire(&target).is_ok());
    }
    #[test]
    fn stable_versions_only() {
        assert!(version("v0.10.0").unwrap() > version("0.9.9").unwrap());
        for bad in [
            "v1.2",
            "v1.2.3-beta",
            "v01.2.3",
            "v1.2.3/../../x",
            "v1.2.999999999",
        ] {
            assert!(version(bad).is_err());
        }
    }
    #[test]
    fn metadata_rejects_drafts_and_old_releases() {
        assert!(release_metadata(
            br#"{"tag_name":"v0.4.0","draft":false,"prerelease":false}"#,
            "0.4.0"
        )
        .unwrap()
        .is_none());
        assert!(release_metadata(
            br#"{"tag_name":"v0.5.0","draft":true,"prerelease":false}"#,
            "0.4.0"
        )
        .unwrap()
        .is_none());
        assert_eq!(
            release_metadata(
                br#"{"tag_name":"v0.5.0","draft":false,"prerelease":false}"#,
                "0.4.0"
            )
            .unwrap()
            .unwrap()
            .tag,
            "v0.5.0"
        );
    }
    #[test]
    fn checksum_requires_unique_matching_asset() {
        let h = "a".repeat(64);
        assert_eq!(checksum(&format!("{h}  app.zip\n"), "app.zip").unwrap(), h);
        assert!(checksum(&format!("{h} app.zip\n{h} app.zip"), "app.zip").is_err());
        assert!(checksum("bad app.zip", "app.zip").is_err());
        assert!(checksum(&format!("{h} other.zip"), "app.zip").is_err());
    }
    #[test]
    fn replacement_and_rollback_preserve_previous_bytes() {
        let t =
            Temp(std::env::temp_dir().join(format!("sharebox-update-test-{}", nonce().unwrap())));
        private_directory(&t.0).unwrap();
        let target = t.0.join("target");
        let new = t.0.join("new");
        let old = t.0.join("old");
        fs::write(&target, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        replace(&target, &new, &old).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        rollback(&target, &old).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(replace(&target, &new, &old).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }
}
