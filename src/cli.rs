use crate::agent::{run, AgentError};
use crate::config::{
    default_config, normalize_binary_value, normalize_config_intervals, normalize_connection_mode,
    normalize_interface_list, normalize_ping_mode,
};
use crate::install::{install, uninstall, LEGACY_AGENT_VERSION};
use crate::types::*;
use crate::update::apply_scheduled_update;
use std::collections::HashSet;
use std::io::Write;

pub fn execute(args: &[String], mut build_version: &str) -> Result<(), AgentError> {
    if build_version.is_empty() || build_version == "dev" {
        build_version = LEGACY_AGENT_VERSION;
    }
    let default_cmd = "install".to_string();
    let cmd = args.first().unwrap_or(&default_cmd);
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };

    match cmd.as_str() {
        "install" => {
            let opts = parse_install_options(rest).map_err(AgentError::Other)?;
            install(opts, build_version).map_err(AgentError::Other)
        }
        "run" | "start-foreground" => {
            let (debug, config_file) = parse_run_options(rest).map_err(AgentError::Other)?;
            run(config_file.as_deref(), debug, build_version)
        }
        "uninstall" | "remove" | "delete" | "purge" => {
            if let Err(e) = parse_uninstall_args(rest) {
                print_usage(&mut std::io::stderr());
                return Err(AgentError::Other(e));
            }
            uninstall(build_version).map_err(AgentError::Other)
        }
        "upgrade-apply" => apply_scheduled_update(build_version).map_err(AgentError::Other),
        "version" | "-v" | "--version" => {
            println!("Serverless Monitor Agent (sma) {}", build_version);
            Ok(())
        }
        "help" | "-h" | "--help" => {
            print_usage(&mut std::io::stdout());
            Ok(())
        }
        other => Err(AgentError::Other(format!(
            "unknown command {:?}, available commands: install | run | uninstall | version",
            other
        ))),
    }
}

pub fn parse_install_options(args: &[String]) -> Result<InstallOptions, String> {
    let mut opts = InstallOptions {
        config: default_config(),
        debug: false,
        rx_correction_gb: String::new(),
        tx_correction_gb: String::new(),
        explicit: HashSet::new(),
    };

    let mut auto_update = String::new();
    let mut debug = String::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if !arg.starts_with('-') {
            return Err(format!("unexpected argument: {}", arg));
        }

        let trimmed = arg.trim_start_matches('-');
        let (key, inline_val) = match trimmed.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (trimmed, None),
        };

        let canon_key = canonical_install_flag(key);
        opts.explicit.insert(canon_key.clone());

        let val = if let Some(v) = inline_val {
            v
        } else if is_boolean_flag(key) {
            "1".to_string()
        } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
            i += 1;
            args[i].clone()
        } else {
            String::new()
        };

        match key {
            "id" => opts.server_id = val,
            "name" => opts.name = val,
            "secret" | "token" => opts.secret = val,
            "url" => opts.worker_url = val,
            "interval" => {
                opts.report_interval = val
                    .parse()
                    .map_err(|_| format!("invalid interval: {}", val))?;
            }
            "collect_interval" | "collect" => {
                opts.collect_interval = val
                    .parse()
                    .map_err(|_| format!("invalid collect_interval: {}", val))?;
            }
            "ct" => opts.ct_node = val,
            "cu" => opts.cu_node = val,
            "cm" => opts.cm_node = val,
            "bd" => opts.bd_node = val,
            "node_1" => opts.node_1 = val,
            "node_2" => opts.node_2 = val,
            "node_3" => opts.node_3 = val,
            "node_4" => opts.node_4 = val,
            "interface" | "interfaces" | "iface" => opts.interface = val,
            "reset_day" => {
                opts.reset_day = val
                    .parse()
                    .map_err(|_| format!("invalid reset_day: {}", val))?;
            }
            "connection_mode" | "connection-mode" => opts.connection_mode = val,
            "ping_mode" | "ping-mode" => opts.ping_mode = val,
            "auto_update" | "auto-update" => auto_update = val,
            "rx_correction" => opts.rx_correction_gb = val,
            "tx_correction" => opts.tx_correction_gb = val,
            "debug" => debug = val,
            "install-ghproxy" => opts.update_proxy = val,
            "install-version" => {
                // ignored
            }
            _ => {
                print_usage(&mut std::io::stderr());
                return Err(format!("unknown flag: -{}", key));
            }
        }
        i += 1;
    }

    opts.auto_update = normalize_binary_value(&auto_update, false)
        .map_err(|e| format!("invalid auto_update value: {}", e))?;
    opts.debug = normalize_binary_value(&debug, false)
        .map_err(|e| format!("invalid debug value: {}", e))?;
    opts.interface = normalize_interface_list(&opts.interface)?;
    opts.connection_mode = normalize_connection_mode(&opts.connection_mode)?;
    opts.ping_mode = normalize_ping_mode(&opts.ping_mode)?;
    normalize_config_intervals(&mut opts.config);

    Ok(opts)
}

fn is_boolean_flag(key: &str) -> bool {
    matches!(key, "debug" | "auto_update" | "auto-update")
}

pub fn canonical_install_flag(name: &str) -> String {
    let replaced = name.replace('-', "_");
    match replaced.as_str() {
        "token" => "secret".to_string(),
        "collect" => "collect_interval".to_string(),
        "interfaces" | "iface" => "interface".to_string(),
        "connection_mode" => "connection_mode".to_string(),
        "ping_mode" => "ping_mode".to_string(),
        "auto_update" => "auto_update".to_string(),
        "install_ghproxy" => "install_ghproxy".to_string(),
        other => other.to_string(),
    }
}

pub fn parse_run_options(args: &[String]) -> Result<(bool, Option<String>), String> {
    let mut debug_raw = "0".to_string();
    let mut config_file = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if !arg.starts_with('-') {
            return Err(format!("unexpected argument: {}", arg));
        }
        let trimmed = arg.trim_start_matches('-');
        let (key, inline_val) = match trimmed.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (trimmed, None),
        };
        let val = if let Some(v) = inline_val {
            v
        } else if key == "debug" {
            "1".to_string()
        } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
            i += 1;
            args[i].clone()
        } else {
            String::new()
        };

        match key {
            "debug" => debug_raw = val,
            "config" => config_file = Some(val),
            _ => return Err(format!("unknown flag: -{}", key)),
        }
        i += 1;
    }

    let debug = normalize_binary_value(&debug_raw, false)
        .map_err(|e| format!("invalid debug value: {}", e))?;
    Ok((debug, config_file))
}

pub fn parse_uninstall_args(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(format!("uninstall does not support arguments: {}", args.join(" ")));
    }
    Ok(())
}

pub fn print_usage<W: Write>(w: &mut W) {
    let _ = writeln!(w, "Usage:");
    let _ = writeln!(
        w,
        "  sma install -id=SERVER_ID -token=TOKEN -url=WORKER_URL [options]"
    );
    let _ = writeln!(
        w,
        "  sma install -token=TOKEN -url=WORKER_URL [-name=NAME]   # Service discovery registration (defaults name to hostname)"
    );
    let _ = writeln!(w, "  sma run [-debug=0|1]");
    let _ = writeln!(w, "  sma uninstall");
    let _ = writeln!(w);
    let _ = writeln!(w, "Install Options:");
    let _ = writeln!(w, "  -id=SERVER_ID        Server ID (optional if auto-discovery enabled)");
    let _ = writeln!(w, "  -name=NAME           Server name (defaults to local hostname)");
    let _ = writeln!(w, "  -token=TOKEN         Server authorization token (alias for -secret)");
    let _ = writeln!(w, "  -secret=SECRET       Server secret / auth token (required on first install)");
    let _ = writeln!(w, "  -url=WORKER_URL      Worker report URL (required on first install)");
    let _ = writeln!(w, "  -interval=N          Report interval in seconds (default 60)");
    let _ = writeln!(w, "  -collect_interval=N  Sample collect interval in seconds (default 0)");
    let _ = writeln!(w, "  -ct/-cu/-cm/-bd=HOST Custom probe target nodes (host or host:port)");
    let _ = writeln!(w, "  -interface=IFACES    Network interfaces to monitor, comma separated");
    let _ = writeln!(w, "  -reset_day=N         Monthly traffic reset day (1-31, 0=never, default 1)");
    let _ = writeln!(w, "  -connection_mode=MODE Connection mode auto|http (default auto)");
    let _ = writeln!(w, "  -ping_mode=MODE      Ping mode tcp|icmp (default tcp)");
    let _ = writeln!(w, "  -auto_update=0|1     Enable auto update check (default 0)");
    let _ = writeln!(w, "  -install-ghproxy=URL Download proxy prefix for GitHub releases");
    let _ = writeln!(w, "  -rx_correction=N     Inbound traffic correction in GB");
    let _ = writeln!(w, "  -tx_correction=N     Outbound traffic correction in GB");
    let _ = writeln!(w, "  -debug=0|1           Verbose debug logging (default 0)");
    let _ = writeln!(w);
    let _ = writeln!(
        w,
        "Re-running install retains existing options and only overrides explicitly passed flags."
    );
}
