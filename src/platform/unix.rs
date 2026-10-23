use crate::util::command::{command_exists, command_output, run_command, run_command_quiet};
use crate::types::*;
use std::fs;
use std::path::Path;
use std::process::Command;

pub fn default_paths() -> Paths {
    if !is_root_user() {
        current_user_paths()
    } else {
        system_default_paths()
    }
}

pub fn is_root_user() -> bool {
    #[cfg(unix)]
    unsafe {
        libc::geteuid() == 0
    }
    #[cfg(not(unix))]
    false
}

pub fn current_uid() -> i32 {
    #[cfg(unix)]
    unsafe {
        libc::geteuid() as i32
    }
    #[cfg(not(unix))]
    0
}

pub fn system_default_paths() -> Paths {
    let service_name = SERVICE_NAME_DEFAULT.to_string();
    let install_dir = if is_openwrt() {
        "/usr/bin".to_string()
    } else {
        "/usr/local/bin".to_string()
    };
    let mut config_dir = format!("/etc/config/{}", service_name);
    let pid_file = format!("/run/{}.pid", service_name);
    let log_file = format!("/var/log/{}.log", service_name);
    let mut launchd_user_file = format!("{}/Library/LaunchAgents/com.serverless.{}.plist", user_home_dir(), service_name);
    if cfg!(target_os = "macos") {
        config_dir = format!("/usr/local/etc/{}", service_name);
        launchd_user_file = String::new();
    }
    Paths {
        service_name: service_name.clone(),
        binary_file: format!("{}/{}", install_dir, service_name),
        config_dir: config_dir.clone(),
        config_file: format!("{}/config.conf", config_dir),
        traffic_file: format!("{}/traffic.dat", config_dir),
        old_traffic_file: format!("/var/lib/{}/traffic.dat", service_name),
        cache_file: format!("{}/metrics_cache.dat", config_dir),
        pid_file,
        log_file,
        service_file: format!("/etc/systemd/system/{}.service", service_name),
        debug_env_file: format!("/run/{}-debug.env", service_name),
        launchd_label: format!("com.serverless.{}", service_name),
        launchd_user_file,
        launchd_root_file: format!("/Library/LaunchDaemons/com.serverless.{}.plist", service_name),
        user_mode: false,
        run_user: "root".to_string(),
        run_uid: 0,
        home_dir: user_home_dir(),
    }
}

pub fn current_user_paths() -> Paths {
    let (name, home, uid) = current_account();
    user_paths(&name, uid, &home)
}

pub fn user_paths(name: &str, uid: i32, home: &str) -> Paths {
    let service_name = SERVICE_NAME_DEFAULT.to_string();
    let home = if home.is_empty() { user_home_dir() } else { home.to_string() };
    let config_dir = format!("{}/.{}", home, service_name);
    Paths {
        service_name: service_name.clone(),
        binary_file: format!("{}/bin/{}", config_dir, service_name),
        config_dir: config_dir.clone(),
        config_file: format!("{}/config.conf", config_dir),
        traffic_file: format!("{}/traffic.dat", config_dir),
        old_traffic_file: String::new(),
        cache_file: format!("{}/metrics_cache.dat", config_dir),
        pid_file: format!("{}/{}.pid", config_dir, service_name),
        log_file: format!("{}/{}.log", config_dir, service_name),
        service_file: format!("{}/.config/systemd/user/{}.service", home, service_name),
        debug_env_file: format!("{}/debug.env", config_dir),
        launchd_label: format!("com.cfsm.{}", service_name),
        launchd_user_file: format!("{}/Library/LaunchAgents/com.cfsm.{}.plist", home, service_name),
        launchd_root_file: format!("/Library/LaunchDaemons/com.cfsm.{}.plist", service_name),
        user_mode: true,
        run_user: name.to_string(),
        run_uid: uid,
        home_dir: home,
    }
}

pub fn darwin_user_paths(home: &str) -> Paths {
    let home = if home.is_empty() { user_home_dir() } else { home.to_string() };
    let uid = sudo_user_uid(&home);
    user_paths(&username_for_uid(uid), uid, &home)
}

pub fn user_home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
}

pub fn current_account() -> (String, String, i32) {
    let uid = current_uid();
    let user = std::env::var("USER").unwrap_or_else(|_| username_for_uid(uid));
    let home = user_home_dir();
    (user, home, uid)
}

pub fn username_for_uid(uid: i32) -> String {
    #[cfg(unix)]
    unsafe {
        let pw = libc::getpwuid(uid as libc::uid_t);
        if !pw.is_null() {
            let name_c = std::ffi::CStr::from_ptr((*pw).pw_name);
            return name_c.to_string_lossy().to_string();
        }
    }
    uid.to_string()
}

pub fn sudo_user_uid(home: &str) -> i32 {
    if let Ok(raw) = std::env::var("SUDO_UID") {
        if let Ok(uid) = raw.parse::<i32>() {
            if uid > 0 {
                return uid;
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = fs::metadata(home) {
            return meta.uid() as i32;
        }
    }
    -1
}

pub fn sudo_user_home_dir() -> String {
    if !cfg!(target_os = "macos") {
        return String::new();
    }
    if let Ok(user) = std::env::var("SUDO_USER") {
        if !user.is_empty() && user != "root" {
            let candidate = format!("/Users/{}", user);
            if Path::new(&candidate).is_dir() {
                return candidate;
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() || home == "/var/root" || home == "/" {
        return String::new();
    }
    home
}

pub fn is_openwrt() -> bool {
    Path::new("/etc/openwrt_release").exists()
}

pub fn is_synology() -> bool {
    Path::new("/etc/synoinfo.conf").exists() || Path::new("/etc.defaults/synoinfo.conf").exists()
}

pub fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "freebsd") {
        "FreeBSD"
    } else if is_synology() {
        "Synology DSM"
    } else if is_openwrt() {
        "OpenWrt"
    } else if Path::new("/etc/alpine-release").exists() {
        "Alpine Linux"
    } else {
        "Linux"
    }
}

pub fn init_system() -> &'static str {
    if cfg!(target_os = "macos") && command_exists("launchctl") {
        return "launchd";
    }
    if is_openwrt() {
        return "procd";
    }
    if is_synology() {
        return "synology-rc";
    }
    if Path::new("/etc/alpine-release").exists() && (command_exists("rc-service") || Path::new("/sbin/openrc-run").exists()) {
        return "openrc";
    }
    if command_exists("systemctl") && (Path::new("/run/systemd/system").exists() || command_output("ps", &["-p", "1", "-o", "comm="]) == "systemd") {
        return "systemd";
    }
    if command_exists("rc-service") && Path::new("/etc/init.d").exists() {
        return "openrc";
    }
    if command_exists("initctl") && Path::new("/etc/init").exists() {
        return "upstart";
    }
    "background"
}

pub fn service_system(paths: &Paths) -> &'static str {
    if paths.user_mode {
        if cfg!(target_os = "macos") {
            return "launchd";
        }
        if cfg!(target_os = "linux") && init_system() == "background" {
            return "background";
        }
        return "systemd-user";
    }
    init_system()
}

pub fn require_install_permission(paths: &Paths) -> Result<(), String> {
    if cfg!(target_os = "macos") && is_root_user() {
        return Err("macOS installation only supports non-root regular users; do not use sudo/root. If an older root version is installed, please run sudo /usr/local/bin/sma uninstall first, then install as regular user".to_string());
    }
    if paths.user_mode {
        if cfg!(target_os = "macos") {
            if !command_exists("launchctl") {
                return Err("launchctl not detected on macOS, cannot register LaunchAgent".to_string());
            }
            return Ok(());
        }
        if init_system() == "background" {
            return Ok(());
        }
        if !systemd_user_supported() {
            return Err("current system does not support non-root execution (no systemd user service capability), please install with root".to_string());
        }
        if !systemd_user_available() {
            return Err(format!(
                "cannot connect to systemd --user service\nPossible causes:\n1. Session switched via su/sudo; please log in directly as user {}\n2. Missing XDG_RUNTIME_DIR or user bus\n3. systemd user manager not ready for this user\nRecommendation: run loginctl enable-linger {}, then re-login and install",
                paths.run_user, paths.run_user
            ));
        }
        if !systemd_user_linger_enabled(&paths.run_user) {
            println!("[INFO] User {} linger is not enabled, attempting to enable automatically...", paths.run_user);
            if let Err(e) = enable_systemd_user_linger(&paths.run_user) {
                return Err(format!(
                    "User {} linger is disabled and auto-enable failed. The service will stop after logout.\nPlease execute with root:\n  loginctl enable-linger {}\nError: {}",
                    paths.run_user, paths.run_user, e
                ));
            }
            println!("[INFO] Linger enabled successfully, service will keep running after logout");
        }
        return Ok(());
    }
    if !is_root_user() {
        return Err("please run install command with root privileges".to_string());
    }
    Ok(())
}

pub fn require_uninstall_permission(paths: &Paths) -> Result<(), String> {
    if paths.user_mode {
        return Ok(());
    }
    if !is_root_user() {
        return Err("please run uninstall command with root privileges".to_string());
    }
    Ok(())
}

fn systemd_user_supported() -> bool {
    cfg!(target_os = "linux")
        && command_exists("systemctl")
        && command_exists("loginctl")
        && (Path::new("/run/systemd/system").exists() || command_output("ps", &["-p", "1", "-o", "comm="]) == "systemd")
}

fn systemd_user_available() -> bool {
    systemd_user_supported() && run_command_quiet("systemctl", &["--user", "show-environment"]).is_ok()
}

fn systemd_user_linger_enabled(name: &str) -> bool {
    if !cfg!(target_os = "linux") || name.is_empty() || !command_exists("loginctl") {
        return false;
    }
    command_output("loginctl", &["show-user", name, "-p", "Linger", "--value"]).trim().eq_ignore_ascii_case("yes")
}

fn enable_systemd_user_linger(name: &str) -> Result<(), String> {
    if !cfg!(target_os = "linux") || name.is_empty() || !command_exists("loginctl") {
        return Err("loginctl is not available".to_string());
    }
    run_command("loginctl", &["--no-ask-password", "enable-linger", name])
}

pub fn check_install_conflicts(paths: &Paths) -> Result<(), String> {
    if paths.user_mode {
        let conflicts = system_install_conflicts();
        if !conflicts.is_empty() {
            if cfg!(target_os = "macos") {
                return Err(format!(
                    "Detected macOS root/system sma installation: {}; please run 'sudo /usr/local/bin/sma uninstall' to clean up the legacy version first, then re-install as normal user without sudo",
                    conflicts.join(", ")
                ));
            }
            return Err(format!(
                "Detected system/root sma installation: {}; migration is not supported, please clean up the old version as root before installing as current user",
                conflicts.join(", ")
            ));
        }
    }
    Ok(())
}

fn system_install_conflicts() -> Vec<String> {
    let paths = system_default_paths();
    let checks = vec![
        paths.binary_file,
        "/usr/local/bin/cf-probe".to_string(),
        "/usr/bin/cf-probe".to_string(),
        paths.config_file,
        paths.traffic_file,
        paths.service_file,
    ];
    let mut out = Vec::new();
    for p in checks {
        if Path::new(&p).exists() {
            out.push(p);
        }
    }
    out
}

pub struct InstanceLock {
    #[cfg(unix)]
    fd: libc::c_int,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
            libc::close(self.fd);
        }
    }
}

pub fn acquire_instance_lock(paths: &Paths) -> Result<InstanceLock, String> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let lock_path = format!("{}/{}.lock", std::env::temp_dir().display(), paths.service_name);
        let c_path = CString::new(lock_path.as_str()).map_err(|e| e.to_string())?;
        unsafe {
            let fd = libc::open(
                c_path.as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o666,
            );
            if fd < 0 {
                return Err(format!("failed to check running instance {}: cannot open file", lock_path));
            }
            let res = libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB);
            if res != 0 {
                libc::close(fd);
                return Err("sma is already running".to_string());
            }
            // write PID into lock file
            let pid_str = format!("pid={}\nuid={}\nuser={}\n", std::process::id(), current_uid(), paths.run_user);
            libc::ftruncate(fd, 0);
            libc::write(fd, pid_str.as_ptr() as *const libc::c_void, pid_str.len());
            Ok(InstanceLock { fd })
        }
    }
    #[cfg(not(unix))]
    {
        Ok(InstanceLock {})
    }
}

pub fn stop_current_user_probe_instances() {}

pub fn copy_self_to(dst: &str) -> Result<(), String> {
    let src = std::env::current_exe().map_err(|e| e.to_string())?;
    let dst_path = Path::new(dst);
    if let Ok(canonical_src) = fs::canonicalize(&src) {
        if let Ok(canonical_dst) = fs::canonicalize(dst_path) {
            if canonical_src == canonical_dst {
                return chmod_executable(dst);
            }
        }
    }
    if let Some(parent) = dst_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = format!("{}.tmp", dst);
    fs::copy(&src, &tmp).map_err(|e| e.to_string())?;
    chmod_executable(&tmp)?;
    fs::rename(&tmp, dst_path).map_err(|e| e.to_string())?;
    chmod_executable(dst)?;
    Ok(())
}

pub fn chmod_executable(path: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn write_service(paths: &Paths, debug: bool) -> Result<(), String> {
    match service_system(paths) {
        "systemd" => write_systemd_service(paths, debug),
        "systemd-user" => write_systemd_user_service(paths, debug),
        "openrc" => write_openrc_service(paths, debug),
        "procd" => write_procd_service(paths, debug),
        "launchd" => write_launchd_service(paths, debug),
        "synology-rc" => write_synology_rc_service(paths, debug),
        _ => Ok(()),
    }
}

pub fn start_service(paths: &Paths, debug: bool) -> Result<(), String> {
    match service_system(paths) {
        "systemd" => {
            run_command("systemctl", &["daemon-reload"])?;
            let _ = run_command("systemctl", &["enable", &format!("{}.service", paths.service_name)]);
            run_command("systemctl", &["restart", &format!("{}.service", paths.service_name)])
        }
        "systemd-user" => {
            run_command("systemctl", &["--user", "daemon-reload"])?;
            let _ = run_command("systemctl", &["--user", "enable", &format!("{}.service", paths.service_name)]);
            run_command("systemctl", &["--user", "restart", &format!("{}.service", paths.service_name)])
        }
        "openrc" => {
            let _ = run_command("rc-update", &["add", &paths.service_name, "default"]);
            run_command("rc-service", &[&paths.service_name, "restart"])
        }
        "procd" => {
            let init_script = format!("/etc/init.d/{}", paths.service_name);
            let _ = run_command(&init_script, &["enable"]);
            run_command(&init_script, &["restart"])
        }
        "launchd" => {
            let plist = launchd_plist(paths);
            let domain = launchd_domain(paths);
            let _ = run_command_quiet("launchctl", &["bootout", &domain, &plist]);
            let _ = run_command_quiet("launchctl", &["bootout", &format!("{}/{}", domain, paths.launchd_label)]);
            run_command("launchctl", &["bootstrap", &domain, &plist])
        }
        "synology-rc" => {
            let rc_file = synology_service_file(paths);
            run_command(&rc_file, &["restart"])
        }
        _ => {
            start_detached(&paths.binary_file, &["run", if debug { "-debug=1" } else { "-debug=0" }], &paths.log_file, &paths.pid_file)
        }
    }
}

pub fn stop_service(paths: &Paths) {
    match service_system(paths) {
        "systemd" => {
            let _ = run_command_quiet("systemctl", &["stop", &format!("{}.service", paths.service_name)]);
            let _ = run_command_quiet("systemctl", &["disable", &format!("{}.service", paths.service_name)]);
        }
        "systemd-user" => {
            let _ = run_command_quiet("systemctl", &["--user", "stop", &format!("{}.service", paths.service_name)]);
            let _ = run_command_quiet("systemctl", &["--user", "disable", &format!("{}.service", paths.service_name)]);
        }
        "openrc" => {
            let _ = run_command("rc-service", &[&paths.service_name, "stop"]);
            let _ = run_command("rc-update", &["del", &paths.service_name, "default"]);
        }
        "procd" => {
            let init_script = format!("/etc/init.d/{}", paths.service_name);
            let _ = run_command(&init_script, &["stop"]);
            let _ = run_command(&init_script, &["disable"]);
        }
        "launchd" => {
            let plist = launchd_plist(paths);
            let domain = launchd_domain(paths);
            let _ = run_command_quiet("launchctl", &["bootout", &domain, &plist]);
            let _ = run_command_quiet("launchctl", &["bootout", &format!("{}/{}", domain, paths.launchd_label)]);
        }
        "synology-rc" => {
            let _ = run_command(&synology_service_file(paths), &["stop"]);
        }
        _ => {
            stop_detached(&paths.pid_file);
        }
    }
}

pub fn remove_service(paths: &Paths) {
    match service_system(paths) {
        "systemd" => {
            let _ = fs::remove_file(&paths.service_file);
            let _ = run_command("systemctl", &["daemon-reload"]);
            let _ = run_command_quiet("systemctl", &["reset-failed", &paths.service_name]);
        }
        "systemd-user" => {
            let _ = fs::remove_file(&paths.service_file);
            let _ = run_command_quiet("systemctl", &["--user", "daemon-reload"]);
            let _ = run_command_quiet("systemctl", &["--user", "reset-failed", &paths.service_name]);
        }
        "openrc" | "procd" => {
            let _ = fs::remove_file(format!("/etc/init.d/{}", paths.service_name));
        }
        "launchd" => {
            let _ = fs::remove_file(launchd_plist(paths));
        }
        "synology-rc" => {
            let _ = fs::remove_file(synology_service_file(paths));
            let _ = fs::remove_file(&paths.service_file);
        }
        _ => {}
    }
}

pub fn ensure_platform_log_file(path: &str) -> Result<(), String> {
    ensure_log_file(path)
}

pub fn ensure_log_file(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn apply_windows_scheduled_update(_paths: &Paths) -> Result<(), String> {
    Err("scheduled upgrade apply is only supported on Windows".to_string())
}

pub fn native_arch() -> String {
    crate::metrics::common::native_arch().to_string()
}

pub fn deep_uninstall(paths: &Paths) -> Vec<String> {
    stop_service(paths);
    remove_service(paths);
    remove_installed_files(paths);
    existing_paths(&[
        &paths.binary_file,
        &paths.config_dir,
        &paths.pid_file,
        &paths.log_file,
    ])
}

fn remove_installed_files(paths: &Paths) {
    stop_detached(&paths.pid_file);
    let _ = fs::remove_file(&paths.binary_file);
    let _ = fs::remove_dir_all(&paths.config_dir);
    let _ = fs::remove_file(&paths.pid_file);
    let _ = fs::remove_file(&paths.log_file);
    let _ = fs::remove_file(&paths.debug_env_file);
}

fn existing_paths(paths: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for &p in paths {
        if Path::new(p).exists() {
            out.push(p.to_string());
        }
    }
    out
}

pub fn launchd_plist(paths: &Paths) -> String {
    if paths.user_mode {
        paths.launchd_user_file.clone()
    } else {
        paths.launchd_root_file.clone()
    }
}

pub fn launchd_domain(paths: &Paths) -> String {
    if paths.user_mode {
        let mut uid = paths.run_uid;
        if uid <= 0 {
            uid = current_uid();
        }
        format!("gui/{}", uid)
    } else {
        "system".to_string()
    }
}

pub fn synology_service_file(paths: &Paths) -> String {
    format!("/usr/local/etc/rc.d/{}.sh", paths.service_name)
}

fn write_systemd_service(paths: &Paths, debug: bool) -> Result<(), String> {
    let debug_arg = if debug { "1" } else { "0" };
    let content = format!(
        r#"[Unit]
Description=Serverless Monitor Agent
After=network.target network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={} run -debug={}
Restart=on-failure
RestartSec=5
User=root
Group=root
Nice=19
CPUSchedulingPolicy=other
IOSchedulingClass=idle
IOSchedulingPriority=7
StandardOutput=journal
StandardError=journal
SyslogIdentifier={}

[Install]
WantedBy=multi-user.target
"#,
        paths.binary_file, debug_arg, paths.service_name
    );
    write_file_executable(&paths.service_file, &content, 0o644)
}

fn write_systemd_user_service(paths: &Paths, debug: bool) -> Result<(), String> {
    let debug_arg = if debug { "1" } else { "0" };
    let content = format!(
        r#"[Unit]
Description=Serverless Monitor Agent
After=default.target

[Service]
Type=simple
ExecStart="{}" run -config="{}" -debug={}
Restart=on-failure
RestartSec=5
StandardOutput=journal
StandardError=journal
SyslogIdentifier={}

[Install]
WantedBy=default.target
"#,
        paths.binary_file, paths.config_file, debug_arg, paths.service_name
    );
    write_file_executable(&paths.service_file, &content, 0o644)
}

fn write_openrc_service(paths: &Paths, debug: bool) -> Result<(), String> {
    let debug_arg = if debug { "1" } else { "0" };
    let content = format!(
        r#"#!/sbin/openrc-run

name="Serverless Monitor Agent"
description="Serverless Monitor Agent"
command="{}"
command_args="run -debug={}"
pidfile="/run/{}.pid"
retry="SIGTERM/30"
supervisor=supervise-daemon
output_log="{}"
error_log="{}"

depend() {{
    need net
    after network
}}
"#,
        paths.binary_file, debug_arg, paths.service_name, paths.log_file, paths.log_file
    );
    write_file_executable(&format!("/etc/init.d/{}", paths.service_name), &content, 0o755)
}

fn write_procd_service(paths: &Paths, debug: bool) -> Result<(), String> {
    let debug_arg = if debug { "1" } else { "0" };
    let bin_name = Path::new(&paths.binary_file).file_name().and_then(|s| s.to_str()).unwrap_or("sma");
    let content = format!(
        r#"#!/bin/sh /etc/rc.common

START=99
STOP=10
USE_PROCD=1

start_service() {{
    procd_open_instance
    procd_set_param command {} run -debug={}
    procd_set_param respawn
    procd_set_param stdout 1
    procd_set_param stderr 1
    procd_close_instance
}}

stop_service() {{
    killall {} 2>/dev/null
}}
"#,
        paths.binary_file, debug_arg, bin_name
    );
    write_file_executable(&format!("/etc/init.d/{}", paths.service_name), &content, 0o755)
}

fn write_launchd_service(paths: &Paths, debug: bool) -> Result<(), String> {
    let plist = launchd_plist(paths);
    let debug_arg = if debug { "1" } else { "0" };
    let mut config_arg = String::new();
    if paths.user_mode {
        config_arg = format!("        <string>-config={}</string>\n", paths.config_file);
    }
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>run</string>
{}        <string>-debug={}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
</dict>
</plist>
"#,
        paths.launchd_label, paths.binary_file, config_arg, debug_arg, paths.log_file, paths.log_file
    );
    write_file_executable(&plist, &content, 0o644)
}

fn write_synology_rc_service(paths: &Paths, debug: bool) -> Result<(), String> {
    let file = synology_service_file(paths);
    let debug_arg = if debug { "1" } else { "0" };
    let content = format!(
        r#"#!/bin/sh

BIN={}
PID={}
LOG={}
ARGS="run -debug={}"

start() {{
    if [ -f "$PID" ] && kill -0 "$(cat "$PID")" 2>/dev/null; then
        echo "{} already running"
        exit 0
    fi
    nohup "$BIN" $ARGS >> "$LOG" 2>&1 &
    echo $! > "$PID"
}}

stop() {{
    if [ -f "$PID" ]; then
        kill "$(cat "$PID")" 2>/dev/null || true
        rm -f "$PID"
    fi
}}

case "$1" in
    start) start ;;
    stop) stop ;;
    restart) stop; sleep 1; start ;;
    *) echo "usage: $0 {{start|stop|restart}}"; exit 1 ;;
esac
"#,
        paths.binary_file, paths.pid_file, paths.log_file, debug_arg, paths.service_name
    );
    write_file_executable(&file, &content, 0o755)
}

fn write_file_executable(path: &str, content: &str, mode: u32) -> Result<(), String> {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, content.as_bytes()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    Ok(())
}

fn start_detached(binary: &str, args: &[&str], log_file: &str, pid_file: &str) -> Result<(), String> {
    let log_path = Path::new(log_file);
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .map_err(|e| e.to_string())?;

    let child = Command::new(binary)
        .args(args)
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| e.to_string())?;

    let pid = child.id();
    if let Some(parent) = Path::new(pid_file).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(pid_file, format!("{}\n", pid));
    Ok(())
}

fn stop_detached(pid_file: &str) {
    if let Ok(content) = fs::read_to_string(pid_file) {
        if let Ok(pid) = content.trim().parse::<i32>() {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
    let _ = fs::remove_file(pid_file);
}
