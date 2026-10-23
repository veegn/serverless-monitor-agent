use crate::util::command::{command_exists, command_output, run_command_quiet};
use crate::storage::config::read_config;
use crate::platform::is_synology;
use crate::types::*;
use std::fs;
use std::path::Path;

pub const LEGACY_SHELL_SCRIPT_NAME: &str = "cf-probe.sh";
pub const LEGACY_UNIX_SCRIPT_FILE: &str = "/usr/local/bin/cf-probe.sh";
pub const LEGACY_WINDOWS_TASK_NAME: &str = "CFProbe";
pub const LEGACY_WINDOWS_SCRIPT_NAME: &str = "cf-server-monitor.ps1";
pub const LEGACY_WINDOWS_LOG_NAME: &str = "cf_probe.log";
pub const LEGACY_DARWIN_LAUNCHD_FILE: &str = "/Library/LaunchDaemons/com.cf.probe.plist";

pub fn same_path(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return a == b;
    }
    let pa = Path::new(a);
    let pb = Path::new(b);
    if cfg!(target_os = "windows") {
        pa.to_string_lossy().eq_ignore_ascii_case(&pb.to_string_lossy())
    } else {
        pa == pb
    }
}

pub fn remove_file_if_exists(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    if fs::symlink_metadata(path).is_ok() {
        let _ = fs::remove_file(path);
        return true;
    }
    false
}

pub fn remove_dir_if_empty(path: &str) {
    if path.is_empty() || path == "." || path == "/" {
        return;
    }
    let _ = fs::remove_dir(path);
}

pub fn base_name_any_sep(path: &str) -> &str {
    let p = path.trim_matches(|c| c == '"' || c == '\'');
    if let Some(idx) = p.rfind(['/', '\\']) {
        &p[idx + 1..]
    } else {
        p
    }
}

pub fn split_command_line_fields(raw: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut quote = ' ';
    let mut escaped = false;

    for r in raw.chars() {
        if escaped {
            current.push(r);
            escaped = false;
            continue;
        }
        if r == '`' {
            escaped = true;
            continue;
        }
        if in_quote {
            if r == quote {
                in_quote = false;
                continue;
            }
            current.push(r);
            continue;
        }
        if r == '"' || r == '\'' {
            in_quote = true;
            quote = r;
            continue;
        }
        if r.is_whitespace() {
            if !current.is_empty() {
                fields.push(current);
                current = String::new();
            }
            continue;
        }
        current.push(r);
    }
    if !current.is_empty() {
        fields.push(current);
    }
    fields
}

pub fn extract_legacy_powershell_script_paths(raw: &str) -> Vec<String> {
    let fields = split_command_line_fields(raw);
    let mut paths = Vec::new();
    for i in 0..fields.len().saturating_sub(1) {
        if fields[i].eq_ignore_ascii_case("-File")
            && base_name_any_sep(&fields[i + 1]).eq_ignore_ascii_case(LEGACY_WINDOWS_SCRIPT_NAME)
        {
            paths.push(fields[i + 1].clone());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

pub fn read_install_config(paths: &Paths) -> Result<(Config, String), (Config, String, String)> {
    match read_config(&paths.config_file) {
        Ok(cfg) => Ok((cfg, paths.config_file.clone())),
        Err(first_err) => {
            for legacy_path in legacy_config_files(paths) {
                if same_path(&legacy_path, &paths.config_file) {
                    continue;
                }
                if let Ok(cfg) = read_config(&legacy_path) {
                    return Ok((cfg, legacy_path));
                }
            }
            Err((Config::default(), paths.config_file.clone(), first_err.to_string()))
        }
    }
}

pub fn migrate_traffic(paths: &Paths) -> Result<(), String> {
    if Path::new(&paths.traffic_file).exists() {
        return Ok(());
    }
    for old_traffic_file in legacy_traffic_files(paths) {
        if old_traffic_file.is_empty() || same_path(&old_traffic_file, &paths.traffic_file) {
            continue;
        }
        if !Path::new(&old_traffic_file).exists() {
            continue;
        }
        if let Some(parent) = Path::new(&paths.traffic_file).parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::rename(&old_traffic_file, &paths.traffic_file).map_err(|e| e.to_string())?;
        if let Some(parent) = Path::new(&old_traffic_file).parent() {
            remove_dir_if_empty(&parent.to_string_lossy());
        }
        return Ok(());
    }
    if let Some(parent) = Path::new(&paths.traffic_file).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _ = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&paths.traffic_file);
    Ok(())
}

pub fn legacy_config_files(paths: &Paths) -> Vec<String> {
    if cfg!(target_os = "windows") {
        return Vec::new();
    }
    let mut candidates = vec![paths.config_file.clone()];
    if cfg!(target_os = "macos") {
        candidates.push("/Library/Application Support/cf-probe/config.conf".to_string());
    }
    if is_synology() {
        candidates.push("/usr/local/etc/cf-probe/config.conf".to_string());
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

pub fn legacy_traffic_files(paths: &Paths) -> Vec<String> {
    let mut candidates = vec![paths.old_traffic_file.clone()];
    if cfg!(target_os = "macos") {
        candidates.push("/Library/Application Support/cf-probe/traffic.dat".to_string());
    }
    if is_synology() {
        candidates.push("/usr/local/etc/cf-probe/traffic.dat".to_string());
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

pub fn cleanup_legacy_install(paths: &Paths) -> (Vec<String>, Vec<String>) {
    if cfg!(target_os = "windows") {
        cleanup_legacy_windows_install(paths)
    } else {
        cleanup_legacy_unix_install(paths)
    }
}

fn cleanup_legacy_unix_install(paths: &Paths) -> (Vec<String>, Vec<String>) {
    if !legacy_unix_install_detected(paths) {
        return (Vec::new(), Vec::new());
    }

    let mut cleaned = Vec::new();
    stop_legacy_unix_autostart(paths);

    if legacy_shell_process_running() {
        stop_legacy_shell_processes();
        cleaned.push(format!("process:{}", LEGACY_SHELL_SCRIPT_NAME));
    }
    for pid_file in legacy_unix_pid_files(paths) {
        if remove_file_if_exists(&pid_file) {
            cleaned.push(pid_file);
        }
    }
    if remove_file_if_exists(LEGACY_UNIX_SCRIPT_FILE) {
        cleaned.push(LEGACY_UNIX_SCRIPT_FILE.to_string());
    }
    let ctl_file = format!("{}.ctl", LEGACY_UNIX_SCRIPT_FILE);
    if remove_file_if_exists(&ctl_file) {
        cleaned.push(ctl_file);
    }
    for service_file in legacy_unix_service_files(paths) {
        if legacy_file_contains(&service_file, LEGACY_SHELL_SCRIPT_NAME) && remove_file_if_exists(&service_file) {
            cleaned.push(service_file);
        }
    }
    if cfg!(target_os = "macos") && remove_file_if_exists(LEGACY_DARWIN_LAUNCHD_FILE) {
        cleaned.push(LEGACY_DARWIN_LAUNCHD_FILE.to_string());
    }
    for path in legacy_runtime_files() {
        if remove_file_if_exists(&path) {
            cleaned.push(path);
        }
    }
    if remove_legacy_var_lib_except_traffic(paths) {
        if let Some(parent) = Path::new(&paths.old_traffic_file).parent() {
            cleaned.push(parent.to_string_lossy().to_string());
        }
    }
    for log_file in legacy_unix_log_files(paths) {
        if remove_file_if_exists(&log_file) {
            cleaned.push(log_file);
        }
    }
    if remove_file_if_exists(&paths.debug_env_file) {
        cleaned.push(paths.debug_env_file.clone());
    }

    if command_exists("systemctl") {
        let _ = run_command_quiet("systemctl", &["daemon-reload"]);
        let _ = run_command_quiet("systemctl", &["reset-failed", &paths.service_name]);
        let _ = run_command_quiet("systemctl", &["reset-failed", &format!("{}.service", paths.service_name)]);
    }

    cleaned.sort();
    cleaned.dedup();
    (cleaned, legacy_unix_residuals(paths))
}

fn legacy_unix_install_detected(paths: &Paths) -> bool {
    if Path::new(LEGACY_UNIX_SCRIPT_FILE).exists() || Path::new(&format!("{}.ctl", LEGACY_UNIX_SCRIPT_FILE)).exists() {
        return true;
    }
    for service_file in legacy_unix_service_files(paths) {
        if legacy_file_contains(&service_file, LEGACY_SHELL_SCRIPT_NAME) {
            return true;
        }
    }
    if cfg!(target_os = "macos") && Path::new(LEGACY_DARWIN_LAUNCHD_FILE).exists() {
        return true;
    }
    if legacy_shell_process_running() {
        return true;
    }
    !legacy_runtime_files().is_empty() || legacy_var_lib_has_removable_files(paths)
}

fn stop_legacy_unix_autostart(paths: &Paths) {
    if cfg!(target_os = "macos") {
        let _ = run_command_quiet("launchctl", &["bootout", "system", LEGACY_DARWIN_LAUNCHD_FILE]);
        let _ = run_command_quiet("launchctl", &["bootout", "system/com.cf.probe"]);
        return;
    }
    if legacy_file_contains(&paths.service_file, LEGACY_SHELL_SCRIPT_NAME) && command_exists("systemctl") {
        let _ = run_command_quiet("systemctl", &["stop", &format!("{}.service", paths.service_name)]);
        let _ = run_command_quiet("systemctl", &["disable", &format!("{}.service", paths.service_name)]);
    }
    let init_script = format!("/etc/init.d/{}", paths.service_name);
    if legacy_file_contains(&init_script, LEGACY_SHELL_SCRIPT_NAME) {
        let _ = run_command_quiet(&init_script, &["stop"]);
        let _ = run_command_quiet(&init_script, &["disable"]);
        if command_exists("rc-service") {
            let _ = run_command_quiet("rc-service", &[&paths.service_name, "stop"]);
        }
        if command_exists("rc-update") {
            let _ = run_command_quiet("rc-update", &["del", &paths.service_name, "default"]);
        }
    }
    let upstart_file = format!("/etc/init/{}.conf", paths.service_name);
    if legacy_file_contains(&upstart_file, LEGACY_SHELL_SCRIPT_NAME) && command_exists("initctl") {
        let _ = run_command_quiet("initctl", &["stop", &paths.service_name]);
    }
    let synology_file = crate::platform::synology_service_file(paths);
    if legacy_file_contains(&synology_file, LEGACY_SHELL_SCRIPT_NAME) {
        let _ = run_command_quiet(&synology_file, &["stop"]);
    }
}

fn legacy_unix_service_files(paths: &Paths) -> Vec<String> {
    let mut files = vec![
        paths.service_file.clone(),
        format!("/etc/init.d/{}", paths.service_name),
        format!("/etc/init/{}.conf", paths.service_name),
        crate::platform::synology_service_file(paths),
    ];
    files.sort();
    files.dedup();
    files
}

fn legacy_unix_pid_files(paths: &Paths) -> Vec<String> {
    let mut files = vec![
        paths.pid_file.clone(),
        format!("/run/{}.pid", paths.service_name),
        format!("/var/run/{}.pid", paths.service_name),
    ];
    files.sort();
    files.dedup();
    files
}

fn legacy_unix_log_files(paths: &Paths) -> Vec<String> {
    let mut files = vec![
        paths.log_file.clone(),
        format!("/var/log/{}.log", paths.service_name),
    ];
    files.sort();
    files.dedup();
    files
}

fn legacy_file_contains(path: &str, needle: &str) -> bool {
    if let Ok(data) = fs::read_to_string(path) {
        data.contains(needle)
    } else {
        false
    }
}

fn legacy_shell_process_running() -> bool {
    !legacy_shell_process_ids().is_empty()
}

fn stop_legacy_shell_processes() {
    for pid in legacy_shell_process_ids() {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    for _ in 0..20 {
        if !legacy_shell_process_running() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn legacy_shell_process_ids() -> Vec<i32> {
    if !command_exists("pgrep") {
        return Vec::new();
    }
    let out = command_output("pgrep", &["-f", LEGACY_UNIX_SCRIPT_FILE]);
    let mut pids = Vec::new();
    let my_pid = std::process::id() as i32;
    for line in out.split_whitespace() {
        if let Ok(pid) = line.parse::<i32>() {
            if pid > 0 && pid != my_pid && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

fn legacy_runtime_files() -> Vec<String> {
    let mut files = Vec::new();
    let patterns = [
        "/dev/shm/.cf_ipv4*",
        "/dev/shm/.cf_ipv6*",
        "/dev/shm/.cf_probe_*",
        "/tmp/.cf_ipv4*",
        "/tmp/.cf_ipv6*",
        "/tmp/.cf_probe_*",
        "/tmp/cf-probe/.cf_probe_*",
    ];
    for pattern in patterns {
        if let Ok(entries) = glob_simple(pattern) {
            files.extend(entries);
        }
    }
    files.sort();
    files.dedup();
    files
}

fn glob_simple(pattern: &str) -> Result<Vec<String>, ()> {
    let path = Path::new(pattern);
    let dir = match path.parent() {
        Some(d) => d,
        None => return Ok(Vec::new()),
    };
    let prefix = match path.file_name().and_then(|f| f.to_str()) {
        Some(name) => name.trim_end_matches('*'),
        None => return Ok(Vec::new()),
    };
    let mut matches = Vec::new();
    if let Ok(read_dir) = fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            if let Some(file_name) = entry.file_name().to_str() {
                if file_name.starts_with(prefix) {
                    matches.push(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }
    Ok(matches)
}

fn legacy_var_lib_has_removable_files(paths: &Paths) -> bool {
    let dir = match Path::new(&paths.old_traffic_file).parent() {
        Some(d) => d,
        None => return false,
    };
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !same_path(&path.to_string_lossy(), &paths.old_traffic_file) {
                return true;
            }
        }
    }
    false
}

fn remove_legacy_var_lib_except_traffic(paths: &Paths) -> bool {
    let dir = match Path::new(&paths.old_traffic_file).parent() {
        Some(d) => d,
        None => return false,
    };
    let mut changed = false;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if same_path(&path.to_string_lossy(), &paths.old_traffic_file) {
                continue;
            }
            let _ = fs::remove_dir_all(&path).or_else(|_| fs::remove_file(&path));
            changed = true;
        }
    }
    if !Path::new(&paths.old_traffic_file).exists() {
        remove_dir_if_empty(&dir.to_string_lossy());
    }
    changed
}

fn legacy_unix_residuals(paths: &Paths) -> Vec<String> {
    let mut residuals = Vec::new();
    if Path::new(LEGACY_UNIX_SCRIPT_FILE).exists() {
        residuals.push(LEGACY_UNIX_SCRIPT_FILE.to_string());
    }
    let ctl = format!("{}.ctl", LEGACY_UNIX_SCRIPT_FILE);
    if Path::new(&ctl).exists() {
        residuals.push(ctl);
    }
    for service_file in legacy_unix_service_files(paths) {
        if legacy_file_contains(&service_file, LEGACY_SHELL_SCRIPT_NAME) {
            residuals.push(service_file);
        }
    }
    if cfg!(target_os = "macos") && Path::new(LEGACY_DARWIN_LAUNCHD_FILE).exists() {
        residuals.push(LEGACY_DARWIN_LAUNCHD_FILE.to_string());
    }
    if legacy_shell_process_running() {
        residuals.push(format!("process:{}", LEGACY_SHELL_SCRIPT_NAME));
    }
    residuals.extend(legacy_runtime_files());
    if legacy_var_lib_has_removable_files(paths) {
        if let Some(parent) = Path::new(&paths.old_traffic_file).parent() {
            residuals.push(parent.to_string_lossy().to_string());
        }
    }
    residuals.sort();
    residuals.dedup();
    residuals
}

fn cleanup_legacy_windows_install(_paths: &Paths) -> (Vec<String>, Vec<String>) {
    let actions = format!(
        "{}\n{}",
        legacy_windows_task_actions().trim(),
        legacy_windows_process_command_lines().trim()
    );
    let scripts = extract_legacy_powershell_script_paths(&actions);
    if !legacy_windows_install_detected(&actions) {
        return (Vec::new(), Vec::new());
    }

    let mut cleaned = Vec::new();
    if run_command_quiet("schtasks", &["/Query", "/TN", LEGACY_WINDOWS_TASK_NAME]).is_ok() {
        let _ = run_command_quiet("schtasks", &["/End", "/TN", LEGACY_WINDOWS_TASK_NAME]);
        let _ = run_command_quiet("schtasks", &["/Delete", "/TN", LEGACY_WINDOWS_TASK_NAME, "/F"]);
        cleaned.push(format!("scheduled-task:{}", LEGACY_WINDOWS_TASK_NAME));
    }
    if legacy_windows_process_running() {
        stop_legacy_windows_processes();
        cleaned.push(format!("process:{}", LEGACY_WINDOWS_SCRIPT_NAME));
    }
    for script in &scripts {
        if remove_file_if_exists(script) {
            cleaned.push(script.clone());
        }
        if let Some(parent) = Path::new(script).parent() {
            let log_file = parent.join(LEGACY_WINDOWS_LOG_NAME).to_string_lossy().to_string();
            if remove_file_if_exists(&log_file) {
                cleaned.push(log_file);
            }
        }
    }
    let temp_ping = std::env::temp_dir().join("cf_probe_ping_results.json").to_string_lossy().to_string();
    if remove_file_if_exists(&temp_ping) {
        cleaned.push(temp_ping);
    }

    cleaned.sort();
    cleaned.dedup();
    (cleaned, legacy_windows_residuals(&scripts))
}

fn legacy_windows_install_detected(task_actions: &str) -> bool {
    !task_actions.is_empty() || legacy_windows_process_running()
}

fn legacy_windows_task_actions() -> String {
    command_output(
        "powershell.exe",
        &[
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!("$t=Get-ScheduledTask -TaskName '{}' -ErrorAction SilentlyContinue; if($t){{$t.Actions | ForEach-Object {{ $_.Execute; $_.Arguments }}}}", LEGACY_WINDOWS_TASK_NAME),
        ],
    )
}

fn legacy_windows_process_running() -> bool {
    let out = command_output(
        "powershell.exe",
        &[
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "$me=$PID; Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $me -and $_.CommandLine -like '*cf-server-monitor*run*' } | Select-Object -First 1 -ExpandProperty ProcessId",
        ],
    );
    !out.trim().is_empty()
}

fn legacy_windows_process_command_lines() -> String {
    command_output(
        "powershell.exe",
        &[
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "$me=$PID; Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $me -and $_.CommandLine -like '*cf-server-monitor*run*' } | Select-Object -ExpandProperty CommandLine",
        ],
    )
}

fn stop_legacy_windows_processes() {
    let _ = run_command_quiet(
        "powershell.exe",
        &[
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "$ErrorActionPreference='SilentlyContinue'; $me=$PID; Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $me -and $_.CommandLine -like '*cf-server-monitor*run*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }",
        ],
    );
}

fn legacy_windows_residuals(scripts: &[String]) -> Vec<String> {
    let mut residuals = Vec::new();
    if run_command_quiet("schtasks", &["/Query", "/TN", LEGACY_WINDOWS_TASK_NAME]).is_ok() {
        residuals.push(format!("scheduled-task:{}", LEGACY_WINDOWS_TASK_NAME));
    }
    if legacy_windows_process_running() {
        residuals.push(format!("process:{}", LEGACY_WINDOWS_SCRIPT_NAME));
    }
    for script in scripts {
        if Path::new(script).exists() {
            residuals.push(script.clone());
        }
    }
    residuals.sort();
    residuals.dedup();
    residuals
}
