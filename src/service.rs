//! Per-user background startup. Never installs elevated/system services.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, String>;

pub const HELP: &str = "sharebox service install master [--bind 0.0.0.0:7331] [--data .sharebox]\nsharebox service install worker [--config worker.json]\nsharebox service start|stop|status|uninstall master|worker\n\nInstall starts immediately and enables startup at user login. Keep the executable at its installed path.\nWindows uses a user Scheduled Task, macOS a LaunchAgent, Linux a systemd user unit.\nPair a worker in the foreground first. Stop foreground instances before installing.\nUninstall preserves data and worker credentials. Stop/cancel active jobs before stopping a worker.";

#[derive(Debug)]
struct Spec {
    role: String,
    exe: String,
    args: Vec<String>,
    cwd: String,
    path: String,
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
    if role == "master" {
        let bind = opts
            .get("--bind")
            .map(String::as_str)
            .unwrap_or("0.0.0.0:7331");
        bind.parse::<std::net::SocketAddr>()
            .map_err(|_| "--bind must be an IP address and port, e.g. 0.0.0.0:7331")?;
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
        args.extend(["--config".into(), path_text(&config)?]);
    }
    let path = std::env::var("PATH").map_err(|_| "PATH must be set and valid Unicode")?;
    safe(&path)?;
    Ok(Spec {
        role: role.into(),
        exe: path_text(&exe)?,
        args,
        cwd: path_text(&cwd)?,
        path,
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
    platform(&action, &role, spec.as_ref())?;
    if action == "install" {
        println!("Registered {role} for startup at user login and requested start. Run `sharebox service status {role}` to check its state.");
    }
    Ok(())
}

fn command(program: &str, args: &[&str]) -> Result<()> {
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
    file.write_all(contents.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())
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
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>org.ossmalaysia.sharebox.{}</string>\n<key>ProgramArguments</key><array>{args}</array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n<key>ThrottleInterval</key><integer>10</integer>\n<key>StandardOutPath</key><string>/dev/null</string>\n<key>StandardErrorPath</key><string>/dev/null</string>\n</dict></plist>\n", spec.role, xml(&spec.cwd), xml(&spec.path))
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
            command("launchctl", &["bootstrap", &domain, &file_text])
        }
        "start" => command("launchctl", &["bootstrap", &domain, &file_text]),
        "stop" => command("launchctl", &["bootout", &target]),
        "status" => command("launchctl", &["print", &target]),
        "uninstall" => {
            // A stopped service is already booted out; only boot it out when loaded.
            let loaded = Command::new("launchctl")
                .args(["print", &target])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|e| e.to_string())?;
            if loaded.success() {
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
    let mut script = format!("$ErrorActionPreference = 'Stop'; $identity = [Security.Principal.WindowsIdentity]::GetCurrent(); $taskName = 'Sharebox-{role}-' + $identity.User.Value; ");
    match action {
        "install" => {
            let spec = spec.ok_or("Missing service specification")?;
            // Hidden PowerShell host keeps the console executable out of the desktop.
            // No persistent output files or secrets in the scheduled task definition.
            let invocation = format!("$env:PATH = {}; & {} {} *> $null; exit $LASTEXITCODE", ps(&spec.path), ps(&spec.exe), spec.args.iter().map(|a| ps(a)).collect::<Vec<_>>().join(" "));
            let host_args = format!("-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -Command {}", windows_arg(&invocation));
            script.push_str(&format!("$action = New-ScheduledTaskAction -Execute (Join-Path $PSHOME 'powershell.exe') -Argument {} -WorkingDirectory {}; $trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity.Name; $principal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType Interactive -RunLevel Limited; $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries; Register-ScheduledTask -TaskName $taskName -Description {} -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null; Start-ScheduledTask -TaskName $taskName;", ps(&host_args), ps(&spec.cwd), ps(&format!("Sharebox {}: starts at user login", spec.role))));
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

#[cfg(test)]
mod tests {
    use super::*;
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
