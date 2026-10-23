use std::env;
use std::process;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn setup_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let payload = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unspecified panic payload".to_string()
        };

        eprintln!(
            "[ERROR] {} unhandled panic crash (at {}): {}",
            timestamp, location, payload
        );
    }));
}

fn main() {
    setup_panic_hook();
    let args: Vec<String> = env::args().skip(1).collect();
    if let Err(err) = serverless_monitor_agent::execute(&args, VERSION) {
        if matches!(err, serverless_monitor_agent::AgentError::AutoDiscoveryFailed) {
            // Auto-discovery failure is an unrecoverable business failure: details have already been logged.
            // Exit with code 0 to cooperate with service managers (Restart=on-failure),
            // completely stopping the process and avoiding repeated auto-discovery loops.
            process::exit(0);
        }
        eprintln!("[ERROR] {}", err);
        process::exit(1);
    }
}
