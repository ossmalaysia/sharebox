//! Native status icon companion. Quitting this process never stops its service.
use std::net::{IpAddr, SocketAddr};

type Result<T> = std::result::Result<T, String>;
pub const HELP: &str = "sharebox tray --role master|worker --dashboard http://HOST:PORT [--check]\n\nWindows notification icon / macOS menu-bar companion for an installed service.\nQuit icon leaves the service running. --check briefly creates a native icon and exits without controlling the service.\nDashboard must use localhost or a private IPv4 address. Linux tray icons are not supported.";

#[derive(Debug)]
struct Options {
    role: String,
    dashboard: String,
    check: bool,
}

pub fn validate_dashboard(url: &str) -> Result<()> {
    let authority = url
        .strip_prefix("http://")
        .ok_or("Dashboard must be an http://localhost:PORT or private IPv4 origin")?
        .strip_suffix('/')
        .unwrap_or_else(|| &url[7..]);
    if authority.contains(['/', '?', '#', '@']) || authority.chars().any(char::is_whitespace) {
        return Err(
            "Dashboard must be an HTTP origin without credentials, path, query, or fragment".into(),
        );
    }
    let allowed = if let Some(port) = authority.strip_prefix("localhost:") {
        port.parse::<u16>().is_ok_and(|port| port != 0)
    } else {
        authority.parse::<SocketAddr>().is_ok_and(|address| {
            address.port() != 0
                && matches!(address.ip(), IpAddr::V4(ip) if ip.is_loopback() || ip.is_private())
        })
    };
    if !allowed {
        return Err("Dashboard must use localhost or a private/loopback IPv4 address with an explicit nonzero port".into());
    }
    Ok(())
}

fn parse(args: Vec<String>) -> Result<Options> {
    let mut args = args.into_iter();
    let mut role = None;
    let mut dashboard = None;
    let mut check = false;
    while let Some(key) = args.next() {
        match key.as_str() {
            "--role" if role.is_none() => {
                role = Some(args.next().ok_or("Missing value for --role")?);
            }
            "--dashboard" if dashboard.is_none() => {
                dashboard = Some(args.next().ok_or("Missing value for --dashboard")?);
            }
            "--check" if !check => check = true,
            _ => {
                return Err(format!(
                    "Unknown or duplicate option {key}; use sharebox tray --help"
                ))
            }
        }
    }
    let role = role.ok_or("Provide --role master or --role worker")?;
    if !matches!(role.as_str(), "master" | "worker") {
        return Err("Tray role must be master or worker".into());
    }
    let dashboard = dashboard.ok_or("Provide --dashboard http://HOST:PORT")?;
    validate_dashboard(&dashboard)?;
    Ok(Options {
        role,
        dashboard,
        check,
    })
}

pub fn run(args: Vec<String>) -> Result<()> {
    if args.is_empty() || args == ["--help"] || args == ["-h"] || args == ["help"] {
        println!("{HELP}");
        return Ok(());
    }
    let options = parse(args)?;
    #[cfg(any(target_os = "macos", windows))]
    return native::run(options);
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (options.role, options.dashboard, options.check);
        Err(
            "Native tray icons are supported on Windows and macOS only; use the dashboard on Linux"
                .into(),
        )
    }
}

#[cfg(any(target_os = "macos", windows))]
mod native {
    use super::*;
    use std::fs::File;
    use std::path::PathBuf;
    #[cfg(target_os = "macos")]
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::time::{Duration, Instant};
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
        Icon, TrayIconBuilder,
    };

    enum Action {
        Start,
        Stop,
        Refresh,
        Open,
        CheckUpdates,
    }
    struct Update {
        running: Option<bool>,
        message: String,
        update_message: String,
        installing: bool,
    }

    fn singleton(role: &str) -> Result<File> {
        #[cfg(windows)]
        let directory =
            PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?)
                .join("Sharebox");
        #[cfg(target_os = "macos")]
        let directory =
            PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(".sharebox-ui");
        if !directory.is_absolute() {
            return Err("Tray state directory must be absolute".into());
        }
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(directory.join(format!("tray-{role}.lock")))
            .map_err(|e| {
                format!("Cannot acquire {role} tray lock (an icon may already be running): {e}")
            })?;
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(format!(
                    "The Sharebox {role} menu-bar icon is already running"
                ));
            }
        }
        Ok(file)
    }

    fn icon() -> Result<Icon> {
        // Two linked boxes, rasterized here to keep the binary self-contained.
        // macOS uses alpha as a template so the menu bar controls light/dark tint.
        let mut rgba = vec![0; 32 * 32 * 4];
        for y in 0usize..32 {
            for x in 0usize..32 {
                let first = (3..18).contains(&x)
                    && (3..18).contains(&y)
                    && (!(6..15).contains(&x) || !(6..15).contains(&y));
                let second = (14..29).contains(&x)
                    && (14..29).contains(&y)
                    && (!(17..26).contains(&x) || !(17..26).contains(&y));
                let bridge = (12..20).contains(&x) && (12..20).contains(&y) && x.abs_diff(y) <= 2;
                if first || second || bridge {
                    let offset = (y * 32 + x) * 4;
                    let color = if cfg!(target_os = "macos") {
                        [0, 0, 0, 255]
                    } else {
                        [20, 184, 166, 255]
                    };
                    rgba[offset..offset + 4].copy_from_slice(&color);
                }
            }
        }
        Icon::from_rgba(rgba, 32, 32).map_err(|e| e.to_string())
    }

    fn monitor(
        role: String,
        dashboard: String,
        requests: Receiver<Action>,
        updates: SyncSender<Update>,
    ) {
        let mut next_action = Some(Action::Refresh);
        let mut next_check = Instant::now();
        let _ = crate::updater::cleanup_completed();
        let mut last_result = crate::updater::last_result();
        let mut update_message = last_result
            .clone()
            .unwrap_or_else(|| format!("Version {}", env!("CARGO_PKG_VERSION")));
        let mut notified = String::new();
        let mut installing = false;
        let mut install_started: Option<Instant> = None;
        loop {
            let result = crate::updater::last_result();
            if result != last_result {
                if let Some(message) = &result {
                    update_message = message.clone();
                    installing = false;
                    install_started = None;
                    let _ = crate::updater::cleanup_completed();
                }
                last_result = result;
            }
            if install_started.is_some_and(|started| started.elapsed() >= Duration::from_secs(600))
            {
                installing = false;
                install_started = None;
                update_message = "Update did not report completion — inspect .sharebox-update-result beside the executable".into();
            }
            let manual_check = matches!(next_action, Some(Action::CheckUpdates));
            if !installing && (manual_check || Instant::now() >= next_check) {
                next_check = Instant::now() + Duration::from_secs(6 * 60 * 60);
                let _ = updates.try_send(Update {
                    running: crate::service::is_running(&role).ok(),
                    message: "Checking for updates…".into(),
                    update_message: update_message.clone(),
                    installing: true,
                });
                match crate::updater::check_latest() {
                    Ok(Some(release)) => {
                        update_message = format!("Update available: {}", release.tag);
                        if manual_check || notified != release.tag {
                            notified = release.tag.clone();
                            match confirm_update(&release.tag) {
                                Ok(true) => match crate::updater::install(&release) {
                                    Ok(()) => {
                                        installing = true;
                                        install_started = Some(Instant::now());
                                        update_message = "Installing update and restarting…".into();
                                    }
                                    Err(error) => {
                                        eprintln!("Sharebox update: {error}");
                                        update_message = format!("Update failed: {error}");
                                    }
                                },
                                Ok(false) => {}
                                Err(error) => {
                                    eprintln!("Sharebox update prompt: {error}");
                                    update_message =
                                        "Update available — click Check for updates".into();
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        update_message = format!("Up to date ({})", env!("CARGO_PKG_VERSION"))
                    }
                    Err(error) => {
                        eprintln!("Sharebox update check: {error}");
                        update_message = "Update check failed — click to retry".into();
                    }
                }
            }
            let action_result = match next_action.take() {
                Some(Action::Start) => crate::service::service_action("start", &role),
                Some(Action::Stop) => crate::service::service_action("stop", &role),
                Some(Action::Open) => open_dashboard(&dashboard),
                _ => Ok(()),
            };
            let status = crate::service::is_running(&role);
            let (running, message) = match (action_result, status) {
                (Err(_), status) => (
                    status.ok(),
                    "Action failed — check service status in a terminal".into(),
                ),
                (Ok(()), Ok(true)) => (Some(true), "Service running".into()),
                (Ok(()), Ok(false)) => (Some(false), "Service stopped or not installed".into()),
                (Ok(()), Err(_)) => (None, "Service status unavailable".into()),
            };
            if let Err(mpsc::TrySendError::Disconnected(_)) = updates.try_send(Update {
                running,
                message,
                update_message: update_message.clone(),
                installing,
            }) {
                break;
            }
            match requests.recv_timeout(Duration::from_secs(if cfg!(windows) { 15 } else { 5 })) {
                Ok(action) => next_action = Some(action),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn confirm_update(tag: &str) -> Result<bool> {
        let message = update_prompt(tag);
        let output = Command::new("/usr/bin/osascript")
            .args(["-e", "on run argv", "-e", "display dialog (item 1 of argv) with title \"Sharebox update\" buttons {\"Later\", \"Update and restart\"} default button \"Later\" giving up after 120", "-e", "end run", "--", &message])
            .stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
        Ok(output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("button returned:Update and restart"))
    }

    #[cfg(windows)]
    fn confirm_update(tag: &str) -> Result<bool> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_YESNO,
        };
        let message: Vec<u16> = update_prompt(tag).encode_utf16().chain(Some(0)).collect();
        let title: Vec<u16> = "Sharebox update".encode_utf16().chain(Some(0)).collect();
        let result = unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_YESNO | MB_ICONINFORMATION | MB_DEFBUTTON2,
            )
        };
        if result == 0 {
            return Err("Could not display update prompt".into());
        }
        Ok(result == IDYES)
    }

    fn update_prompt(tag: &str) -> String {
        format!("Sharebox {tag} is available. Download, verify, and restart with your existing configuration and pairing?\n\nFinish or cancel active tests first. This restarts running master and worker services using this installation. Other machines must be updated separately.")
    }

    #[cfg(target_os = "macos")]
    fn open_dashboard(url: &str) -> Result<()> {
        validate_dashboard(url)?;
        let mut child = Command::new("/usr/bin/open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return if status.success() {
                    Ok(())
                } else {
                    Err("Could not open the dashboard browser".into())
                };
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Opening the dashboard browser timed out".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    #[cfg(windows)]
    fn open_dashboard(url: &str) -> Result<()> {
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
        validate_dashboard(url)?;
        let operation: Vec<u16> = "open\0".encode_utf16().collect();
        let url: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                url.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if result as isize <= 32 {
            Err("Could not open the dashboard browser".into())
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    struct EventLoop(objc2::rc::Retained<objc2_app_kit::NSApplication>);
    #[cfg(target_os = "macos")]
    impl EventLoop {
        fn new() -> Result<Self> {
            use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
            let main = objc2::MainThreadMarker::new()
                .ok_or("The macOS tray must run on the main thread")?;
            let app = NSApplication::sharedApplication(main);
            if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
                return Err("Could not enable the menu-bar application".into());
            }
            app.finishLaunching();
            Ok(Self(app))
        }
        fn pump(&self) -> bool {
            use objc2_app_kit::NSEventMask;
            use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
            objc2::rc::autoreleasepool(|_| {
                let until = NSDate::dateWithTimeIntervalSinceNow(0.1);
                if let Some(event) = self.0.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&until),
                    unsafe { NSDefaultRunLoopMode },
                    true,
                ) {
                    self.0.sendEvent(&event);
                }
            });
            true
        }
    }
    #[cfg(windows)]
    struct EventLoop;
    #[cfg(windows)]
    impl EventLoop {
        fn new() -> Result<Self> {
            // Scheduled Tasks already launch a hidden host. Stay attached so
            // stopping that task also terminates this companion and releases its lock.
            Ok(Self)
        }
        fn pump(&self) -> bool {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
            };
            let mut message: MSG = unsafe { std::mem::zeroed() };
            for _ in 0..64 {
                if unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } == 0
                {
                    break;
                }
                if message.message == WM_QUIT {
                    return false;
                }
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
            true
        }
    }

    pub(super) fn run(options: Options) -> Result<()> {
        let _lock = if options.check {
            None
        } else {
            Some(singleton(&options.role)?)
        };
        let event_loop = EventLoop::new()?;
        let menu = Menu::new();
        let heading = MenuItem::new(format!("Sharebox {}", options.role), false, None);
        let status = MenuItem::new(
            if options.check {
                "Native icon check"
            } else {
                "Checking service status…"
            },
            false,
            None,
        );
        let open = MenuItem::new("Open dashboard", !options.check, None);
        let start = MenuItem::new("Start service", false, None);
        let stop = MenuItem::new("Stop service (cancel jobs first)", false, None);
        let refresh = MenuItem::new("Refresh status", !options.check, None);
        let update_status = MenuItem::new(
            format!("Version {}", env!("CARGO_PKG_VERSION")),
            false,
            None,
        );
        let check_updates = MenuItem::new("Check for updates…", !options.check, None);
        let quit = MenuItem::new("Quit icon (keep service running)", true, None);
        menu.append_items(&[
            &heading,
            &status,
            &PredefinedMenuItem::separator(),
            &open,
            &start,
            &stop,
            &refresh,
            &PredefinedMenuItem::separator(),
            &update_status,
            &check_updates,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| e.to_string())?;
        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(format!("Sharebox {}", options.role));
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon()?);
        #[cfg(windows)]
        let builder = builder.with_icon(icon()?);
        let icon = builder.build().map_err(|e| e.to_string())?;
        let (actions, requests) = mpsc::sync_channel(1);
        let (updates, responses) = mpsc::sync_channel(1);
        if !options.check {
            let role = options.role.clone();
            std::thread::spawn(move || monitor(role, options.dashboard, requests, updates));
        }
        let started = Instant::now();
        loop {
            if !event_loop.pump() {
                break;
            }
            if options.check && started.elapsed() >= Duration::from_secs(1) {
                break;
            }
            while let Ok(update) = responses.try_recv() {
                status.set_text(&update.message);
                start.set_enabled(!update.installing && update.running == Some(false));
                stop.set_enabled(!update.installing && update.running == Some(true));
                check_updates.set_enabled(!update.installing);
                update_status.set_text(update.update_message.chars().take(150).collect::<String>());
                let _ = icon.set_tooltip(Some(format!(
                    "Sharebox {} — {}",
                    options.role, update.message
                )));
            }
            for _ in 0..64 {
                let Ok(event) = MenuEvent::receiver().try_recv() else {
                    break;
                };
                if event.id == *quit.id() {
                    return Ok(());
                }
                if options.check {
                    continue;
                }
                let action = if event.id == *start.id() {
                    Action::Start
                } else if event.id == *stop.id() {
                    Action::Stop
                } else if event.id == *open.id() {
                    Action::Open
                } else if event.id == *check_updates.id() {
                    Action::CheckUpdates
                } else if event.id == *refresh.id() {
                    Action::Refresh
                } else {
                    continue;
                };
                if actions.try_send(action).is_ok() {
                    status.set_text("Working…");
                    start.set_enabled(false);
                    stop.set_enabled(false);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dashboard_only_accepts_trusted_http_origins() {
        for valid in [
            "http://localhost:7331",
            "http://127.0.0.1:7331/",
            "http://192.168.1.3:8080",
            "http://10.0.0.1:7331",
            "http://172.31.1.2:7331",
        ] {
            assert!(validate_dashboard(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "https://localhost:7331",
            "http://localhost:0",
            "http://localhost",
            "http://localhost:7331/admin",
            "http://localhost:7331//",
            "http://localhost:7331?token=x",
            "http://user@localhost:7331",
            "http://8.8.8.8:7331",
            "file:///etc/passwd",
            "http://localhost:7331\n",
            "http://example.com:7331",
        ] {
            assert!(validate_dashboard(invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn tray_parser_requires_role_and_dashboard_and_rejects_duplicates() {
        let args = [
            "--role",
            "worker",
            "--dashboard",
            "http://192.168.1.3:7331",
            "--check",
        ];
        let options = parse(args.into_iter().map(str::to_owned).collect()).unwrap();
        assert_eq!(options.role, "worker");
        assert!(options.check);
        for args in [
            vec!["--role", "other", "--dashboard", "http://localhost:7331"],
            vec!["--role", "master"],
            vec!["--role", "master", "--role", "worker"],
            vec!["--check", "--check"],
        ] {
            assert!(parse(args.into_iter().map(str::to_owned).collect()).is_err());
        }
    }
}
