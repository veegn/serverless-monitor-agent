use crate::command::{command_output, run_command, run_command_quiet};
use crate::types::*;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub const WINDOWS_TASK_NAMESPACE: &str = "http://schemas.microsoft.com/windows/2004/02/mit/task";
pub const WINDOWS_TASK_AUTHOR_ID: &str = "Author";
pub const WINDOWS_RESUME_EVENT_SUBSCRIPTION: &str = r#"<QueryList>
  <Query Id="0" Path="System">
    <Select Path="System">*[System[Provider[@Name='Microsoft-Windows-Power-Troubleshooter'] and EventID=1]]</Select>
    <Select Path="System">*[System[Provider[@Name='Microsoft-Windows-Kernel-Power'] and EventID=107]]</Select>
  </Query>
</QueryList>"#;

pub fn default_paths() -> Paths {
    let service_name = SERVICE_NAME_DEFAULT.to_string();
    let program_data = env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
    let program_files = env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
    let mut install_dir = PathBuf::from(&program_files).join(&service_name);
    if install_dir.to_string_lossy().trim() == service_name.as_str() {
        install_dir = PathBuf::from(format!(r"C:\Program Files\{}", service_name));
    }
    let binary = install_dir.join(format!("{}.exe", service_name));
    let config_dir = PathBuf::from(&program_data).join(&service_name);

    Paths {
        service_name: service_name.clone(),
        binary_file: binary.to_string_lossy().to_string(),
        config_dir: config_dir.to_string_lossy().to_string(),
        config_file: config_dir.join("config.conf").to_string_lossy().to_string(),
        traffic_file: config_dir.join("traffic.dat").to_string_lossy().to_string(),
        cache_file: config_dir.join("metrics_cache.dat").to_string_lossy().to_string(),
        old_traffic_file: PathBuf::from(&program_data)
            .join(&service_name)
            .join("traffic.dat")
            .to_string_lossy()
            .to_string(),
        pid_file: PathBuf::from(&program_data)
            .join(&service_name)
            .join(format!("{}.pid", service_name))
            .to_string_lossy()
            .to_string(),
        log_file: PathBuf::from(&program_data)
            .join(&service_name)
            .join(format!("{}.log", service_name))
            .to_string_lossy()
            .to_string(),
        service_file: service_name.clone(),
        debug_env_file: config_dir.join("debug.env").to_string_lossy().to_string(),
        launchd_label: String::new(),
        launchd_user_file: String::new(),
        launchd_root_file: String::new(),
        user_mode: false,
        run_user: "SYSTEM".to_string(),
        run_uid: 0,
        home_dir: String::new(),
    }
}

pub fn require_install_permission(_paths: &Paths) -> Result<(), String> {
    if !is_elevated() {
        return Err("Windows installation requires Administrator privileges. Please re-open PowerShell as Administrator and try again (right-click PowerShell and select 'Run as Administrator')".to_string());
    }
    Ok(())
}

pub fn require_uninstall_permission(_paths: &Paths) -> Result<(), String> {
    if !is_elevated() {
        return Err("Windows uninstallation requires Administrator privileges. Please re-open PowerShell as Administrator and try again (right-click PowerShell and select 'Run as Administrator')".to_string());
    }
    Ok(())
}

pub fn is_elevated() -> bool {
    #[cfg(target_os = "windows")]
    {
        // Try fltmc.exe as a fast elevation check
        let status = Command::new("fltmc.exe").status();
        if let Ok(st) = status {
            return st.success();
        }
        // Fallback to PowerShell
        let out = command_output(
            "powershell.exe",
            &[
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
            ],
        );
        out.trim().eq_ignore_ascii_case("true")
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

pub fn check_install_conflicts(_paths: &Paths) -> Result<(), String> {
    Ok(())
}

pub fn stop_current_user_probe_instances() {}

pub struct InstanceLock;

pub fn acquire_instance_lock(_paths: &Paths) -> Result<InstanceLock, String> {
    Ok(InstanceLock)
}

pub fn copy_self_to(dst: &str) -> Result<(), String> {
    let src = env::current_exe().map_err(|e| e.to_string())?;
    let src_canon = fs::canonicalize(&src).unwrap_or(src);
    let dst_path = PathBuf::from(dst);
    let dst_canon = fs::canonicalize(&dst_path).unwrap_or_else(|_| dst_path.clone());

    if src_canon == dst_canon {
        return Ok(());
    }

    if let Some(parent) = dst_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let tmp = format!("{}.tmp", dst);
    fs::copy(&src_canon, &tmp).map_err(|e| e.to_string())?;

    let backup = format!("{}.previous", dst);
    if Path::new(&backup).exists() {
        let _ = fs::remove_file(&backup);
    }

    if Path::new(dst).exists() {
        let _ = rename_windows_file_with_retry(dst, &backup, Duration::from_secs(5));
    }

    if let Err(e) = rename_windows_file_with_retry(&tmp, dst, Duration::from_secs(5)) {
        if Path::new(&backup).exists() {
            let _ = rename_windows_file_with_retry(&backup, dst, Duration::from_secs(5));
        }
        return Err(e);
    }

    let _ = fs::remove_file(&backup);
    Ok(())
}

pub fn start_detached(binary: &str, args: &[&str], log_file: &str, pid_file: &str) -> Result<(), String> {
    if let Some(parent) = Path::new(log_file).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
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

pub fn stop_detached(pid_file: &str) {
    let _ = fs::remove_file(pid_file);
}

pub fn stop_windows_scheduled_task(paths: &Paths) {
    let _ = run_command_quiet("schtasks", &["/End", "/TN", &paths.service_name]);
    stop_windows_probe_processes(paths);
    wait_windows_probe_processes_stopped(paths, Duration::from_secs(5));
}

pub fn stop_windows_probe_processes(paths: &Paths) {
    let script = windows_probe_process_script(
        paths,
        "ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }",
    );
    let _ = run_command_quiet(
        "powershell.exe",
        &[
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ],
    );
}

pub fn wait_windows_probe_processes_stopped(paths: &Paths, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if !windows_probe_processes_running(paths) || Instant::now() > deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn wait_windows_probe_processes_started(paths: &Paths, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if windows_probe_processes_running(paths) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn windows_probe_processes_running(paths: &Paths) -> bool {
    let script = windows_probe_process_script(paths, "Select-Object -First 1 -ExpandProperty ProcessId");
    let out = command_output(
        "powershell.exe",
        &[
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ],
    );
    !out.trim().is_empty()
}

fn windows_probe_process_script(paths: &Paths, pipeline: &str) -> String {
    format!(
        "$ErrorActionPreference='SilentlyContinue'; $me=$PID; $bin={}; $wrapper={}; $procs=Get-CimInstance Win32_Process | Where-Object {{ $cmd=[string]$_.CommandLine; $_.ProcessId -ne $me -and $cmd -and ((($cmd.IndexOf($bin, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) -and $cmd.ToLowerInvariant().Contains(' run ')) -or ($cmd.IndexOf($wrapper, [System.StringComparison]::OrdinalIgnoreCase) -ge 0)) }}; $procs | {}",
        quote_powershell_literal(&paths.binary_file),
        quote_powershell_literal(&windows_task_wrapper_file(paths)),
        pipeline
    )
}

pub fn deep_uninstall(paths: &Paths) -> Vec<String> {
    let _ = run_command_quiet("schtasks", &["/End", "/TN", &paths.service_name]);
    let _ = run_command_quiet("schtasks", &["/Delete", "/TN", &paths.service_name, "/F"]);
    remove_installed_files(paths);
    let delayed = remove_windows_binary(paths);
    windows_uninstall_residuals(paths, &delayed)
}

pub fn remove_installed_files(paths: &Paths) {
    stop_detached(&paths.pid_file);
    let _ = fs::remove_file(&paths.binary_file);
    if !paths.config_dir.is_empty() {
        let _ = fs::remove_dir_all(&paths.config_dir);
    }
    let _ = fs::remove_file(&paths.pid_file);
    let _ = fs::remove_file(&paths.log_file);
    let _ = fs::remove_file(&paths.debug_env_file);
}

pub fn remove_windows_binary(paths: &Paths) -> Vec<String> {
    let mut delayed = Vec::new();
    if remove_windows_file(&paths.binary_file) {
        delayed.push(paths.binary_file.clone());
    }
    let tmp = format!("{}.tmp", paths.binary_file);
    if remove_windows_file(&tmp) {
        delayed.push(tmp);
    }
    if let Some(parent) = Path::new(&paths.binary_file).parent() {
        let _ = fs::remove_dir(parent);
    }
    delayed
}

fn remove_windows_file(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    if let Ok(()) = fs::remove_file(path) {
        return false;
    }
    if !Path::new(path).exists() {
        return false;
    }
    schedule_windows_file_removal(path)
}

fn schedule_windows_file_removal(path: &str) -> bool {
    let dir = Path::new(path).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let script = format!(
        "Start-Sleep -Seconds 2; Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue; Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue",
        quote_powershell_literal(path),
        quote_powershell_literal(&dir)
    );
    let mut cmd = Command::new("powershell.exe");
    cmd.args(&["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script]);
    cmd.spawn().is_ok()
}

fn windows_uninstall_residuals(paths: &Paths, delayed: &[String]) -> Vec<String> {
    let mut delayed_set = std::collections::HashSet::new();
    for path in delayed {
        delayed_set.insert(path.clone());
    }
    let checks = [
        &paths.binary_file,
        &paths.config_dir,
        &paths.pid_file,
        &paths.log_file,
        &paths.debug_env_file,
    ];
    let mut residuals = Vec::new();
    for path in checks {
        if Path::new(path).exists() && !delayed_set.contains(path.as_str()) && !delayed_path_in_dir(delayed, path) {
            residuals.push(path.to_string());
        }
    }
    if run_command_quiet("schtasks", &["/Query", "/TN", &paths.service_name]).is_ok() {
        residuals.push(format!("scheduled-task:{}", paths.service_name));
    }
    residuals.sort();
    residuals.dedup();
    residuals
}

fn delayed_path_in_dir(delayed: &[String], dir: &str) -> bool {
    if dir.is_empty() {
        return false;
    }
    let clean_dir = Path::new(dir);
    for path in delayed {
        let clean_path = Path::new(path);
        if clean_path.starts_with(clean_dir) {
            return true;
        }
    }
    false
}

pub fn quote_powershell_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn apply_windows_scheduled_update(paths: &Paths) -> Result<(), String> {
    let src = env::current_exe().map_err(|e| e.to_string())?;
    let src_canon = fs::canonicalize(&src).unwrap_or(src);
    let dst_path = PathBuf::from(&paths.binary_file);
    let dst_canon = fs::canonicalize(&dst_path).unwrap_or_else(|_| dst_path.clone());

    if src_canon == dst_canon {
        println!("[INFO] update binary already installed; starting scheduled task");
        return start_service(paths, false);
    }

    if let Some(parent) = dst_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&paths.config_dir).map_err(|e| e.to_string())?;

    let backup = PathBuf::from(&paths.config_dir).join(format!("{}-rollback.exe", paths.service_name));
    let previous = format!("{}.previous", paths.binary_file);
    let staged = format!("{}.new", paths.binary_file);

    let _ = fs::remove_file(&staged);
    let _ = fs::remove_file(&previous);

    if Path::new(&paths.binary_file).exists() {
        let _ = fs::copy(&paths.binary_file, &backup);
    }
    if let Err(e) = fs::copy(&src_canon, &staged) {
        let _ = fs::remove_file(&staged);
        return Err(format!("stage updated binary: {}", e));
    }

    if run_command_quiet("schtasks", &["/Query", "/TN", &paths.service_name]).is_err() {
        let _ = fs::remove_file(&staged);
        return Err(format!("Windows scheduled task {} is missing", paths.service_name));
    }
    if !Path::new(&windows_task_wrapper_file(paths)).exists() {
        let _ = fs::remove_file(&staged);
        return Err(format!("Windows task wrapper is missing: {}", windows_task_wrapper_file(paths)));
    }

    println!("[INFO] stopping Windows scheduled task for binary swap");
    stop_windows_scheduled_task(paths);

    if Path::new(&paths.binary_file).exists() {
        if let Err(e) = rename_windows_file_with_retry(&paths.binary_file, &previous, Duration::from_secs(5)) {
            let _ = fs::remove_file(&staged);
            let _ = start_service(paths, false);
            return Err(format!("move current binary aside: {}", e));
        }
    }

    if let Err(e) = rename_windows_file_with_retry(&staged, &paths.binary_file, Duration::from_secs(5)) {
        let rollback_err = rollback_windows_scheduled_update(paths, &paths.binary_file, &previous, &backup.to_string_lossy());
        return Err(format!("install updated binary: {}; rollback: {:?}", e, rollback_err));
    }

    println!("[INFO] starting Windows scheduled task after update");
    if let Err(e) = start_service(paths, false) {
        let rollback_err = rollback_windows_scheduled_update(paths, &paths.binary_file, &previous, &backup.to_string_lossy());
        return Err(format!("start updated scheduled task: {}; rollback: {:?}", e, rollback_err));
    }

    if !wait_windows_probe_processes_started(paths, Duration::from_secs(10)) {
        let rollback_err = rollback_windows_scheduled_update(paths, &paths.binary_file, &previous, &backup.to_string_lossy());
        return Err(format!("updated probe did not stay running; rollback: {:?}", rollback_err));
    }

    let _ = fs::remove_file(&previous);
    println!("[INFO] Windows scheduled update applied");
    Ok(())
}

fn rollback_windows_scheduled_update(paths: &Paths, dst: &str, previous: &str, backup: &str) -> Result<(), String> {
    println!("[WARN] rolling back Windows scheduled update");
    stop_windows_scheduled_task(paths);
    let _ = fs::remove_file(dst);

    let mut restore_err = None;
    if Path::new(previous).exists() {
        if let Err(e) = rename_windows_file_with_retry(previous, dst, Duration::from_secs(5)) {
            restore_err = Some(e);
        }
    }
    if (restore_err.is_some() || !Path::new(dst).exists()) && Path::new(backup).exists() {
        restore_err = fs::copy(backup, dst).map(|_| ()).map_err(|e| e.to_string()).err();
    }
    if let Some(e) = restore_err {
        return Err(format!("rollback restore failed: {}", e));
    }
    if !Path::new(dst).exists() {
        return Err("rollback restore failed: no backup binary available".to_string());
    }

    start_service(paths, false)?;
    if !wait_windows_probe_processes_started(paths, Duration::from_secs(10)) {
        return Err("rollback start failed: probe did not stay running".to_string());
    }
    println!("[INFO] Windows scheduled update rolled back");
    Ok(())
}

fn rename_windows_file_with_retry(src: &str, dst: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if Instant::now() > deadline {
                    return Err(e.to_string());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn windows_service_args(paths: &Paths, _debug: bool) -> Vec<String> {
    vec![
        "cmd.exe".to_string(),
        "/D".to_string(),
        "/C".to_string(),
        quote_windows_cmd_arg(&windows_task_wrapper_file(paths)),
    ]
}

pub fn windows_scheduled_task_xml(paths: &Paths, debug: bool) -> Result<String, String> {
    let args = windows_service_args(paths, debug);
    let command = args.first().cloned().unwrap_or_default();
    let arguments = args[1..].join(" ");

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="{xmlns}">
  <RegistrationInfo>
    <Description>CF Server Monitor Probe Agent</Description>
  </RegistrationInfo>
  <Triggers>
    <BootTrigger>
      <Enabled>true</Enabled>
    </BootTrigger>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
    <EventTrigger>
      <Enabled>true</Enabled>
      <Subscription>{subscription}</Subscription>
      <Delay>PT10S</Delay>
    </EventTrigger>
  </Triggers>
  <Principals>
    <Principal id="{author_id}">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="{author_id}">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        xmlns = WINDOWS_TASK_NAMESPACE,
        author_id = WINDOWS_TASK_AUTHOR_ID,
        subscription = WINDOWS_RESUME_EVENT_SUBSCRIPTION,
        command = command,
        arguments = arguments
    );
    Ok(xml)
}

pub fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + s.len() * 2);
    out.push(0xff);
    out.push(0xfe);
    for u in s.encode_utf16() {
        out.push(u as u8);
        out.push((u >> 8) as u8);
    }
    out
}

pub fn write_windows_task_wrapper(paths: &Paths, debug: bool) -> Result<(), String> {
    let debug_arg = if debug { "-debug=1" } else { "-debug=0" };
    let content = format!(
        "@echo off\r\nsetlocal\r\nif not exist {config_dir} mkdir {config_dir}\r\necho [%DATE% %TIME%] sma task starting >> {log_file}\r\n{bin} run {debug_arg} >> {log_file} 2>&1\r\nexit /b %ERRORLEVEL%\r\n",
        config_dir = quote_windows_cmd_arg(&paths.config_dir),
        log_file = quote_windows_cmd_arg(&paths.log_file),
        bin = quote_windows_cmd_arg(&paths.binary_file),
        debug_arg = debug_arg,
    );
    let file = windows_task_wrapper_file(paths);
    if let Some(parent) = Path::new(&file).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&file, content.as_bytes()).map_err(|e| e.to_string())
}

pub fn windows_task_wrapper_file(paths: &Paths) -> String {
    PathBuf::from(&paths.config_dir)
        .join(format!("{}.cmd", paths.service_name))
        .to_string_lossy()
        .to_string()
}

pub fn quote_windows_cmd_arg(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

pub fn ensure_platform_log_file(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    match fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(_) => Ok(()),
        Err(e) => {
            if Path::new(path).exists() {
                // file in use
                Ok(())
            } else {
                Err(e.to_string())
            }
        }
    }
}

pub fn chmod_executable(_path: &str) -> Result<(), String> {
    Ok(())
}

pub fn platform_name() -> &'static str {
    "Windows"
}

pub fn is_openwrt() -> bool {
    false
}

pub fn is_synology() -> bool {
    false
}

pub fn service_system(_paths: &Paths) -> &'static str {
    "windows"
}

pub fn write_service(paths: &Paths, debug: bool) -> Result<(), String> {
    write_windows_service(paths, debug)
}

pub fn launchd_domain(_paths: &Paths) -> String {
    String::new()
}

pub fn launchd_plist(_paths: &Paths) -> String {
    String::new()
}

pub fn synology_service_file(_paths: &Paths) -> String {
    String::new()
}

pub fn write_windows_service(paths: &Paths, debug: bool) -> Result<(), String> {
    write_windows_task_wrapper(paths, debug)?;
    let _ = run_command_quiet("schtasks", &["/Delete", "/TN", &paths.service_name, "/F"]);
    let xml_content = windows_scheduled_task_xml(paths, debug)?;
    let xml_bytes = utf16le_with_bom(&xml_content);

    let tmp_path = env::temp_dir().join(format!("{}-task-{}.xml", paths.service_name, std::process::id()));
    fs::write(&tmp_path, xml_bytes).map_err(|e| e.to_string())?;

    let res = run_command(
        "schtasks",
        &[
            "/Create",
            "/TN",
            &paths.service_name,
            "/XML",
            &tmp_path.to_string_lossy(),
            "/RU",
            "SYSTEM",
            "/F",
        ],
    );
    let _ = fs::remove_file(&tmp_path);
    if let Err(e) = res {
        return Err(format!("failed to create Windows scheduled task, please ensure PowerShell is running as Administrator: {}", e));
    }
    Ok(())
}

pub fn start_service(paths: &Paths, _debug: bool) -> Result<(), String> {
    run_command("schtasks", &["/Run", "/TN", &paths.service_name])
}

pub fn stop_service(paths: &Paths) {
    stop_windows_scheduled_task(paths);
}

pub fn remove_service(paths: &Paths) {
    let _ = run_command("schtasks", &["/Delete", "/TN", &paths.service_name, "/F"]);
}

pub fn native_arch() -> String {
    #[cfg(target_os = "windows")]
    {
        if let Ok(arch) = env::var("PROCESSOR_ARCHITEW6432") {
            let upper = arch.to_ascii_uppercase();
            if upper == "ARM64" {
                return "arm64".to_string();
            } else if upper == "AMD64" {
                return "amd64".to_string();
            } else if upper == "X86" {
                return "386".to_string();
            }
        }
        if let Ok(arch) = env::var("PROCESSOR_ARCHITECTURE") {
            let upper = arch.to_ascii_uppercase();
            if upper == "ARM64" {
                return "arm64".to_string();
            } else if upper == "AMD64" {
                return "amd64".to_string();
            } else if upper == "X86" {
                return "386".to_string();
            }
        }
    }
    match std::env::consts::ARCH {
        "x86_64" => "amd64".to_string(),
        "x86" => "386".to_string(),
        "aarch64" => "arm64".to_string(),
        other => other.to_string(),
    }
}
