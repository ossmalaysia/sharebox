mod http;
mod master;
mod network;
mod service;
mod tray;
mod worker;

fn main() {
    if let Err(e) = run() {
        eprintln!("sharebox: {e}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    if matches!(command.as_str(), "" | "help" | "--help" | "-h") {
        println!("Sharebox {} — trusted LAN task runner\n\nsharebox master [--bind 0.0.0.0:7331] [--data .sharebox]\nsharebox init [--config worker.json]\nsharebox worker [--server http://MASTER_IP:7331] [--code PAIR_CODE] [--config worker.json]\nsharebox service --help\nsharebox tray --help\n\nNative tasks run with the worker account's permissions. HTTP is for trusted private LANs only.",env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if command == "--version" {
        println!("sharebox {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if command == "service" {
        return service::run(args.collect());
    }
    if command == "tray" {
        return tray::run(args.collect());
    }
    let mut opts = std::collections::HashMap::new();
    while let Some(k) = args.next() {
        if !["--bind", "--data", "--server", "--code", "--config"].contains(&k.as_str()) {
            return Err(format!("Unknown option {k}"));
        }
        let v = args.next().ok_or(format!("Missing value for {k}"))?;
        opts.insert(k, v);
    }
    let config = std::path::PathBuf::from(
        opts.get("--config")
            .map(String::as_str)
            .unwrap_or("worker.json"),
    );
    match command.as_str() {
        "master" => master::run(
            opts.get("--bind")
                .map(String::as_str)
                .unwrap_or("0.0.0.0:7331"),
            std::path::PathBuf::from(
                opts.get("--data")
                    .map(String::as_str)
                    .unwrap_or(".sharebox"),
            ),
        ),
        "init" => worker::init(config),
        "worker" => worker::run(opts.remove("--server"), opts.remove("--code"), config),
        _ => Err(format!("Unknown command {command}; use --help")),
    }
}
