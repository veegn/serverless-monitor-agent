use crate::storage::config::{normalize_config_intervals, write_config};
use crate::cli::summary::{print_management_commands, print_probe_nodes};
use crate::cli::legacy::{cleanup_legacy_install, migrate_traffic, read_install_config};
use crate::metrics::read_net_bytes;
use crate::platform::{
    chmod_executable, copy_self_to, default_paths, ensure_platform_log_file,
    native_arch, platform_name, require_install_permission, require_uninstall_permission,
    service_system, start_service, stop_current_user_probe_instances, stop_service,
    write_service, deep_uninstall, check_install_conflicts,
};
use crate::storage::traffic::{apply_traffic_correction, parse_traffic_correction_gb};
use crate::types::*;
use std::path::Path;

pub const LEGACY_AGENT_VERSION: &str = "v1.1.2";

pub fn install(mut opts: InstallOptions, version: &str) -> Result<(), String> {
    let paths = default_paths();
    print_banner(version);
    require_install_permission(&paths)?;
    check_install_conflicts(&paths)?;

    let (existing, existing_path, using_existing_config) = match read_install_config(&paths) {
        Ok((cfg, path)) => (cfg, path, true),
        Err((_, _, _)) => (Config::default(), String::new(), false),
    };

    if using_existing_config {
        println!(
            "[INFO] Found existing config at {}, retaining options not explicitly passed",
            existing_path
        );
        let flag_config = opts.config.clone();
        opts.config = existing;
        merge_explicit_install_config(&mut opts.config, &flag_config, &opts.explicit);
    } else if opts.secret.is_empty() || opts.worker_url.is_empty() {
        crate::cli::print_usage(&mut std::io::stderr());
        return Err("first install requires -token (or -secret) and -url, e.g.: sma install -token=TOKEN -url=WORKER_URL".to_string());
    }

    if opts.name.is_empty() {
        opts.name = crate::util::command::get_system_hostname();
    }

    if !using_existing_config || opts.config_md5.is_empty() {
        opts.config_md5 = "none".to_string();
    }
    normalize_config_intervals(&mut opts.config);

    let correction_net = prepare_install_traffic_correction(&opts)?;

    if !paths.user_mode {
        let (cleaned, residuals) = cleanup_legacy_install(&paths);
        if !cleaned.is_empty() {
            println!(
                "[INFO] Cleaned legacy shell probe artifacts: {}",
                cleaned.join(", ")
            );
        }
        if !residuals.is_empty() {
            for item in &residuals {
                println!("[WARN] Legacy shell probe residual: {}", item);
            }
            return Err(format!(
                "legacy shell probe cleanup incomplete: {}",
                residuals.join(", ")
            ));
        }
    }

    println!(
        "[INFO] Platform: {}/{} ({})",
        std::env::consts::OS,
        native_arch(),
        platform_name()
    );
    if paths.user_mode {
        println!(
            "[INFO] Install mode: user ({})",
            if paths.run_user.is_empty() { "current" } else { &paths.run_user }
        );
    } else {
        println!("[INFO] Install mode: system");
    }
    println!("[INFO] Install binary: {}", paths.binary_file);
    println!("[INFO] Config file: {}", paths.config_file);
    print_windows_auto_update_warning(&paths, &opts);

    stop_service(&paths);
    stop_current_user_probe_instances();

    copy_self_to(&paths.binary_file)
        .map_err(|e| format!("failed to install binary: {}", e))?;
    chmod_executable(&paths.binary_file)
        .map_err(|e| format!("failed to set executable permissions: {}", e))?;

    migrate_traffic(&paths)?;
    write_config(&paths.config_file, opts.config.clone())
        .map_err(|e| format!("failed to write config: {}", e))?;

    if !opts.rx_correction_gb.is_empty() || !opts.tx_correction_gb.is_empty() {
        apply_traffic_correction(
            &paths.traffic_file,
            correction_net,
            &opts.interface,
            &opts.rx_correction_gb,
            &opts.tx_correction_gb,
        )?;
    }

    write_service(&paths, opts.debug)?;
    prepare_management_log_file(&paths)?;
    start_service(&paths, opts.debug)?;

    print_install_summary(&paths, &opts);
    Ok(())
}

pub fn uninstall(version: &str) -> Result<(), String> {
    let paths = default_paths();
    print_banner(version);
    require_uninstall_permission(&paths)?;

    println!("[INFO] Starting uninstallation of {}", paths.service_name);
    let residuals = deep_uninstall(&paths);
    if !residuals.is_empty() {
        for item in &residuals {
            println!("[WARN] Uninstall residual: {}", item);
        }
        return Err(format!("uninstallation incomplete, {} residual items remaining", residuals.len()));
    }
    println!("[INFO] Uninstallation complete");
    Ok(())
}

fn prepare_install_traffic_correction(opts: &InstallOptions) -> Result<NetBytes, String> {
    if opts.rx_correction_gb.is_empty() && opts.tx_correction_gb.is_empty() {
        return Ok(NetBytes::default());
    }
    if !opts.rx_correction_gb.is_empty() {
        parse_traffic_correction_gb(&opts.rx_correction_gb)?;
    }
    if !opts.tx_correction_gb.is_empty() {
        parse_traffic_correction_gb(&opts.tx_correction_gb)?;
    }
    read_net_bytes(&opts.interface)
        .map_err(|e| format!("read network counters before traffic correction: {}", e))
}

fn merge_explicit_install_config(
    dst: &mut Config,
    src: &Config,
    explicit: &std::collections::HashSet<String>,
) {
    for name in explicit {
        match name.as_str() {
            "id" => dst.server_id = src.server_id.clone(),
            "name" => dst.name = src.name.clone(),
            "secret" => dst.secret = src.secret.clone(),
            "url" => dst.worker_url = src.worker_url.clone(),
            "interval" => dst.report_interval = src.report_interval,
            "collect_interval" => dst.collect_interval = src.collect_interval,
            "ct" => dst.ct_node = src.ct_node.clone(),
            "cu" => dst.cu_node = src.cu_node.clone(),
            "cm" => dst.cm_node = src.cm_node.clone(),
            "bd" => dst.bd_node = src.bd_node.clone(),
            "node_1" => dst.node_1 = src.node_1.clone(),
            "node_2" => dst.node_2 = src.node_2.clone(),
            "node_3" => dst.node_3 = src.node_3.clone(),
            "node_4" => dst.node_4 = src.node_4.clone(),
            "interface" => dst.interface = src.interface.clone(),
            "reset_day" => dst.reset_day = src.reset_day,
            "connection_mode" => dst.connection_mode = src.connection_mode.clone(),
            "ping_mode" => dst.ping_mode = src.ping_mode.clone(),
            "auto_update" => dst.auto_update = src.auto_update,
            "install_ghproxy" => dst.update_proxy = src.update_proxy.clone(),
            _ => {}
        }
    }
}

fn print_windows_auto_update_warning(paths: &Paths, opts: &InstallOptions) {
    if !cfg!(target_os = "windows") || !opts.auto_update {
        return;
    }
    println!("[WARN] Windows auto-update executes sma-update.exe which might be blocked by antivirus or security policies.");
    println!(
        "[WARN] If blocked, please whitelist the following paths: {}, {}, {}",
        paths.binary_file,
        Path::new(&paths.config_dir).join("sma-update.exe").display(),
        crate::platform::windows::windows_task_wrapper_file(paths)
    );
}

pub fn print_banner(version: &str) {
    let ver = if version.is_empty() || version == "dev" {
        LEGACY_AGENT_VERSION
    } else {
        version
    };
    println!("===========================================");
    println!("    Serverless Monitor Agent (sma)");
    println!("    Version: {}", ver);
    println!("===========================================");
}

fn prepare_management_log_file(paths: &Paths) -> Result<(), String> {
    let log_file = management_log_file(paths, service_system(paths));
    if log_file.is_empty() {
        return Ok(());
    }
    ensure_platform_log_file(&log_file)
}

fn management_log_file(paths: &Paths, system: &str) -> String {
    match system {
        "openrc" | "launchd" | "synology-rc" | "background" | "windows" => paths.log_file.clone(),
        "upstart" => format!("/var/log/upstart/{}.log", paths.service_name),
        _ => String::new(),
    }
}

pub fn print_install_summary(paths: &Paths, opts: &InstallOptions) {
    println!();
    println!("===========================================");
    println!("  Serverless Monitor Agent Installed Successfully");
    println!("===========================================");
    println!("  Service     : {}", paths.service_name);
    println!("  Binary      : {}", paths.binary_file);
    println!("  Config      : {}", paths.config_file);
    if !opts.server_id.is_empty() {
        println!("  Server ID   : {}", opts.server_id);
    } else {
        println!("  Server ID   : (auto discovery registration)");
    }
    if !opts.name.is_empty() {
        println!("  Server Name : {}", opts.name);
    }
    println!("  Auth Token  : ********");
    println!("  Worker URL  : {}", opts.worker_url);
    println!("  Debug       : {}", opts.debug);
    print_probe_nodes(opts);
    print_management_commands(paths);
    println!("===========================================");
    if opts.server_id.is_empty() {
        println!();
        println!("  No -id was passed; server will automatically register this node on first report:");
        println!("    - Check logs using the live log command above. \"auto discovery created server successfully\" indicates success.");
        println!("    - After registration, the node will appear in the dashboard in ~2 minutes.");
        println!("    - If auto discovery fails or is rejected, check logs and re-run install with valid parameters.");
    }
}
