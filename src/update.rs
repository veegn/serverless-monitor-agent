use crate::cli::legacy::read_install_config;
use crate::net::dns::use_public_dns_resolver;
use crate::net::http::{download_to_file, get};
use crate::platform::{
    apply_windows_scheduled_update, default_paths, is_synology, native_arch,
    require_install_permission,
};
use crate::types::*;
use crate::util::command::{command_exists, command_output, quote_shell, run_command_quiet};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const AUTO_UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
pub const AUTO_UPDATE_LOCK_TTL: Duration = Duration::from_secs(30 * 60);
pub const AUTO_UPDATE_DELAY: Duration = Duration::from_secs(60);
pub const DEFAULT_UPDATE_REPO: &str = "veegn/serverless-monitor-agent";
pub const GITHUB_API_BASE_URL: &str = "https://api.github.com";
pub const SNAPSHOT_VERSION_PREFIX: &str = "Snapshot-";
pub const UPDATE_CHECKSUMS_ASSET_NAME: &str = "checksums.txt";

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GithubRelease {
    #[serde(default)]
    pub tag_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GithubReleaseAsset {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCandidate {
    pub tag_name: String,
    pub asset_name: String,
    pub snapshot: bool,
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateOptions {
    pub check_only: bool,
    pub force: bool,
    pub proxy: Option<String>,
    pub help: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateVersion {
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
    pub pre: Vec<String>,
}

pub fn darwin_update_supported() -> (bool, String) {
    if !cfg!(target_os = "macos") {
        return (true, String::new());
    }
    match darwin_product_version() {
        Ok(ver) => (darwin_major_version(&ver) >= 13, ver),
        Err(_) => (false, String::new()),
    }
}

pub fn darwin_product_version() -> Result<String, String> {
    let out = command_output("sw_vers", &["-productVersion"]);
    let trimmed = out.trim().to_string();
    if trimmed.is_empty() {
        Err("empty sw_vers output".to_string())
    } else {
        Ok(trimmed)
    }
}

pub fn darwin_major_version(ver: &str) -> i32 {
    let first = ver.split('.').next().unwrap_or("").trim();
    first.parse::<i32>().unwrap_or(0)
}

pub fn check_latest_update(
    current_version: &str,
    use_public_dns: bool,
) -> Result<Option<UpdateCandidate>, String> {
    let (owner, name) = split_update_repo_slug(DEFAULT_UPDATE_REPO)?;
    let os_name = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else {
        "linux"
    };
    let asset_name = expected_update_asset_name(os_name, &native_arch());
    if asset_name.is_empty() {
        return Err("cannot determine a compatible update asset for this build".to_string());
    }

    let releases = list_github_releases(&owner, &name, use_public_dns)?;

    if current_version.starts_with(SNAPSHOT_VERSION_PREFIX) {
        if let Some(candidate) = select_latest_snapshot_release(&releases, &asset_name) {
            if candidate.tag_name != current_version {
                return Ok(Some(candidate));
            }
        }
        return Ok(None);
    }

    let current = parse_update_version(current_version)
        .map_err(|e| format!("parse current version {:?}: {}", current_version, e))?;

    Ok(select_latest_stable_release(&releases, &asset_name, &current))
}

pub fn check_latest_release(
    use_public_dns: bool,
    snapshot: bool,
) -> Result<Option<UpdateCandidate>, String> {
    let (owner, name) = split_update_repo_slug(DEFAULT_UPDATE_REPO)?;
    let os_name = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else {
        "linux"
    };
    let asset_name = expected_update_asset_name(os_name, &native_arch());
    if asset_name.is_empty() {
        return Err("cannot determine a compatible update asset for this build".to_string());
    }

    let releases = list_github_releases(&owner, &name, use_public_dns)?;
    if snapshot {
        return Ok(select_latest_snapshot_release(&releases, &asset_name));
    }
    let zero_version = UpdateVersion {
        major: 0,
        minor: 0,
        patch: 0,
        pre: Vec::new(),
    };
    Ok(select_latest_stable_release(&releases, &asset_name, &zero_version))
}

pub fn list_github_releases(
    owner: &str,
    repo: &str,
    use_public_dns: bool,
) -> Result<Vec<GithubRelease>, String> {
    let mut all_releases = Vec::new();
    for page in 1..=10 {
        let endpoint = format!(
            "{}/repos/{}/{}/releases?per_page=100&page={}",
            GITHUB_API_BASE_URL, owner, repo, page
        );
        let resp = get(
            &endpoint,
            &[
                ("Accept", "application/vnd.github+json"),
                ("User-Agent", "cfsm-agent"),
            ],
            Duration::from_secs(30),
            use_public_dns,
        )?;
        if resp.status < 200 || resp.status >= 300 {
            return Err(format!(
                "GitHub releases API returned http {}: {}",
                resp.status,
                resp.text().chars().take(200).collect::<String>()
            ));
        }
        let page_releases: Vec<GithubRelease> = serde_json::from_slice(&resp.body)
            .map_err(|e| format!("decode github releases json: {}", e))?;
        let count = page_releases.len();
        all_releases.extend(page_releases);
        if count < 100 {
            break;
        }
    }
    Ok(all_releases)
}

pub fn select_latest_stable_release(
    releases: &[GithubRelease],
    asset_name: &str,
    current: &UpdateVersion,
) -> Option<UpdateCandidate> {
    let mut latest: Option<UpdateCandidate> = None;
    let mut latest_version: Option<UpdateVersion> = None;

    for release in releases {
        if release.draft || release.prerelease {
            continue;
        }
        if find_release_asset(release, asset_name).is_none() {
            continue;
        }
        let version = match parse_update_version(&release.tag_name) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if compare_update_version(&version, current) != Ordering::Greater {
            continue;
        }
        let should_replace = match (&latest, &latest_version) {
            (None, _) => true,
            (Some(cand), Some(lv)) => {
                let ord = compare_update_version(&version, lv);
                ord == Ordering::Greater || (ord == Ordering::Equal && release.published_at > cand.published_at)
            }
            _ => true,
        };
        if should_replace {
            latest = Some(UpdateCandidate {
                tag_name: release.tag_name.clone(),
                asset_name: asset_name.to_string(),
                snapshot: false,
                published_at: release.published_at,
            });
            latest_version = Some(version);
        }
    }
    latest
}

pub fn select_latest_snapshot_release(
    releases: &[GithubRelease],
    asset_name: &str,
) -> Option<UpdateCandidate> {
    let mut latest: Option<UpdateCandidate> = None;

    for release in releases {
        if release.draft || !release.prerelease || !release.tag_name.starts_with(SNAPSHOT_VERSION_PREFIX) {
            continue;
        }
        if find_release_asset(release, asset_name).is_none() {
            continue;
        }
        let should_replace = match &latest {
            None => true,
            Some(cand) => {
                if release.published_at > cand.published_at {
                    true
                } else if release.published_at == cand.published_at {
                    release.tag_name > cand.tag_name
                } else {
                    false
                }
            }
        };
        if should_replace {
            latest = Some(UpdateCandidate {
                tag_name: release.tag_name.clone(),
                asset_name: asset_name.to_string(),
                snapshot: true,
                published_at: release.published_at,
            });
        }
    }
    latest
}

pub fn find_release_asset<'a>(
    release: &'a GithubRelease,
    asset_name: &str,
) -> Option<&'a GithubReleaseAsset> {
    release.assets.iter().find(|a| a.name == asset_name)
}

pub fn expected_update_asset_name(goos: &str, goarch: &str) -> String {
    if goos == "linux" && (goarch == "arm" || goarch == "armv7") {
        return "sma-linux-armv7".to_string();
    }
    let goarm = "";
    update_asset_name_for_arm(goos, goarch, goarm)
}

pub fn update_asset_name_for_arm(goos: &str, mut goarch: &str, goarm: &str) -> String {
    if goos == "linux" && goarch == "arm" {
        match goarm {
            "5" | "5,softfloat" | "5,hardfloat" => goarch = "armv5",
            "6" | "6,hardfloat" => goarch = "armv6",
            "7" | "7,hardfloat" => goarch = "armv7",
            _ => return String::new(),
        }
    }
    let mut name = format!("sma-{}-{}", goos, goarch);
    if goos == "windows" {
        name.push_str(".exe");
    }
    name
}

pub fn fetch_update_binary(
    config_dir: &str,
    candidate: &UpdateCandidate,
    proxy: &str,
    use_public_dns: bool,
) -> Result<String, String> {
    let raw_url = update_asset_download_url(&candidate.tag_name, &candidate.asset_name, proxy)?;
    let _ = fs::create_dir_all(config_dir);

    let dest = if cfg!(target_os = "windows") {
        PathBuf::from(config_dir).join("sma-update.exe")
    } else {
        PathBuf::from(config_dir).join("sma-update.bin")
    };
    let dest_str = dest.to_string_lossy().to_string();

    let checksums = download_update_checksums(&candidate.tag_name, proxy, use_public_dns)?;
    let expected = checksum_for_asset(&checksums, &candidate.asset_name)
        .ok_or_else(|| format!("checksum missing for {}", candidate.asset_name))?;

    download_to_file(
        &raw_url,
        &[("User-Agent", "cfsm-agent")],
        &dest,
        512 * 1024 * 1024,
        Duration::from_secs(300),
        use_public_dns,
    )?;

    if let Err(e) = verify_file_sha256(&dest_str, &expected) {
        let _ = fs::remove_file(&dest);
        return Err(e);
    }

    Ok(dest_str)
}

pub fn download_update_checksums(
    tag: &str,
    proxy: &str,
    use_public_dns: bool,
) -> Result<String, String> {
    let raw_url = update_asset_download_url(tag, UPDATE_CHECKSUMS_ASSET_NAME, proxy)?;
    let resp = get(
        &raw_url,
        &[("User-Agent", "cfsm-agent")],
        Duration::from_secs(30),
        use_public_dns,
    )?;
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!("download {} returned http {}", raw_url, resp.status));
    }
    Ok(resp.text())
}

pub fn checksum_for_asset(checksums: &str, asset_name: &str) -> Option<String> {
    for line in checksums.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            continue;
        }
        let sum = fields[0].to_ascii_lowercase();
        let raw_name = fields[fields.len() - 1].trim_start_matches('*');
        let name = Path::new(raw_name)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        if name == asset_name && valid_sha256_hex(&sum) {
            return Some(sum);
        }
    }
    None
}

pub fn valid_sha256_hex(value: &str) -> bool {
    if value.len() != 64 {
        return false;
    }
    value.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn verify_file_sha256(path: &str, expected: &str) -> Result<(), String> {
    let expected = expected.trim().to_ascii_lowercase();
    if !valid_sha256_hex(&expected) {
        return Err(format!(
            "invalid sha256 checksum for {}",
            Path::new(path).file_name().and_then(|f| f.to_str()).unwrap_or(path)
        ));
    }
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hex::encode(hasher.finalize()).to_ascii_lowercase();
    if actual != expected {
        return Err(format!(
            "sha256 mismatch for {}",
            Path::new(path).file_name().and_then(|f| f.to_str()).unwrap_or(path)
        ));
    }
    Ok(())
}

pub fn schedule_update_install(paths: &Paths, bin_path: &str, now: i64) -> Result<String, String> {
    if cfg!(target_os = "windows") {
        schedule_windows_update_install(paths, bin_path)
    } else if paths.user_mode {
        schedule_user_mode_update_install(paths, bin_path)
    } else {
        schedule_unix_update_install(&paths.service_name, &paths.log_file, bin_path, now)
    }
}

pub fn schedule_unix_update_install(
    service_name: &str,
    log_file: &str,
    bin_path: &str,
    now: i64,
) -> Result<String, String> {
    let cmd_line = format!(
        "sleep {}; {} install; rm -f {}",
        AUTO_UPDATE_DELAY.as_secs(),
        quote_shell(bin_path),
        quote_shell(bin_path)
    );
    if cfg!(target_os = "linux") && !is_synology() && Path::new("/run/systemd/system").exists() {
        let unit = format!("{}-auto-update-{}", service_name, now);
        if command_exists("systemd-run") {
            let status = Command::new("systemd-run")
                .args(&[format!("--unit={}", unit), "/bin/sh".to_string(), "-c".to_string(), cmd_line.clone()])
                .output();
            if let Ok(out) = status {
                if out.status.success() {
                    return Ok(format!("systemd-run:{}", unit));
                }
            }
            if !command_exists("systemctl") {
                return Err("systemd-run failed".to_string());
            }
        }
        return schedule_systemd_unit(&unit, &cmd_line);
    }

    let mut nohup_cmd = format!("nohup /bin/sh -c {} >/dev/null 2>&1 &", quote_shell(&cmd_line));
    if !log_file.is_empty() {
        nohup_cmd = format!(
            "nohup /bin/sh -c {} >>{} 2>&1 &",
            quote_shell(&cmd_line),
            quote_shell(log_file)
        );
    }
    let res = Command::new("sh").args(["-c", &nohup_cmd]).status().map_err(|e| e.to_string())?;
    if res.success() {
        Ok("nohup".to_string())
    } else {
        Err("nohup command failed".to_string())
    }
}

pub fn schedule_windows_update_install(paths: &Paths, bin_path: &str) -> Result<String, String> {
    let script = format!(
        "$ErrorActionPreference = 'Continue'; Start-Sleep -Seconds {}; & {} upgrade-apply >> {} 2>&1; if ($LASTEXITCODE -eq 0) {{ Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue }}",
        AUTO_UPDATE_DELAY.as_secs(),
        crate::platform::quote_powershell_literal(bin_path),
        crate::platform::quote_powershell_literal(&paths.log_file),
        crate::platform::quote_powershell_literal(bin_path)
    );
    let child = Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .spawn()
        .map_err(|e| e.to_string())?;
    drop(child);
    Ok("powershell".to_string())
}

pub fn schedule_user_mode_update_install(paths: &Paths, bin_path: &str) -> Result<String, String> {
    if paths.binary_file.is_empty() {
        return Err("installed binary path is empty".to_string());
    }
    if let Some(parent) = Path::new(&paths.binary_file).parent() {
        let _ = fs::create_dir_all(parent);
    }
    crate::platform::chmod_executable(bin_path)?;

    let staged = format!("{}.update", paths.binary_file);
    let _ = fs::remove_file(&staged);
    fs::rename(bin_path, &staged).map_err(|e| e.to_string())?;
    if let Err(e) = fs::rename(&staged, &paths.binary_file) {
        let _ = fs::remove_file(&staged);
        return Err(e.to_string());
    }

    std::thread::spawn(|| {
        std::thread::sleep(AUTO_UPDATE_DELAY);
        std::process::exit(42);
    });
    Ok("self-replace".to_string())
}

pub fn apply_scheduled_update(build_version: &str) -> Result<(), String> {
    if !cfg!(target_os = "windows") {
        return Err("scheduled upgrade apply is only supported on Windows".to_string());
    }
    let paths = default_paths();
    println!(
        "[INFO] applying scheduled update version={} target={}",
        build_version, paths.binary_file
    );
    apply_windows_scheduled_update(&paths)
}

pub fn run_manual_update(opts: UpdateOptions, build_version: &str) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let (ok, ver) = darwin_update_supported();
        if !ok {
            return Err(format!(
                "update not supported: current builds require macOS 13 Ventura or later (detected macOS {})",
                ver
            ));
        }
    }

    let paths = default_paths();
    let (cfg, _cfg_path, using_existing_config) = match read_install_config(&paths) {
        Ok((cfg, path)) => (cfg, path, true),
        Err((cfg, path, _)) => (cfg, path, false),
    };

    if !opts.check_only {
        require_install_permission(&paths)?;
        if !using_existing_config {
            return Err(format!(
                "no existing agent configuration found at {}. Please run 'sma install' first to install and configure the agent",
                paths.config_file
            ));
        }
    }

    let proxy = opts.proxy.clone().unwrap_or_else(|| {
        if !cfg.update_proxy.is_empty() {
            cfg.update_proxy.clone()
        } else {
            std::env::var("UPDATE_PROXY").unwrap_or_default()
        }
    });
    let use_dns = use_public_dns_resolver(&cfg);

    println!("[INFO] checking for updates (current version: {})...", build_version);

    let is_snapshot = build_version.starts_with(SNAPSHOT_VERSION_PREFIX);
    let candidate_opt = if opts.force {
        check_latest_release(use_dns, is_snapshot)?
    } else {
        check_latest_update(build_version, use_dns)?
    };

    let candidate = match candidate_opt {
        Some(c) => c,
        None => {
            if opts.force {
                return Err("no update candidate available on GitHub for this platform".to_string());
            }
            println!("[INFO] current version {} is up to date", build_version);
            return Ok(());
        }
    };

    if opts.check_only {
        println!(
            "[INFO] update available: {} (current: {})",
            candidate.tag_name, build_version
        );
        return Ok(());
    }

    if opts.force {
        println!(
            "[INFO] force reinstalling version {} (asset: {})...",
            candidate.tag_name, candidate.asset_name
        );
    } else {
        println!(
            "[INFO] found new version {} (current: {})",
            candidate.tag_name, build_version
        );
    }
    println!("[INFO] downloading {}...", candidate.asset_name);
    let bin_path = fetch_update_binary(&paths.config_dir, &candidate, &proxy, use_dns)?;
    println!("[INFO] download complete, checksum verified");

    #[cfg(target_os = "windows")]
    {
        println!("[INFO] applying update to {}...", candidate.tag_name);
        let current_pid = std::process::id();
        let script = format!(
            "$ErrorActionPreference = 'Continue'; Wait-Process -Id {} -Timeout 15 -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 500; & {} upgrade-apply; if ($LASTEXITCODE -eq 0) {{ Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue }}",
            current_pid,
            crate::platform::quote_powershell_literal(&bin_path),
            crate::platform::quote_powershell_literal(&bin_path)
        );
        Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
            .spawn()
            .map_err(|e| format!("failed to launch Windows updater: {}", e))?;
        println!(
            "[INFO] update to {} scheduled; will apply immediately upon process exit",
            candidate.tag_name
        );
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    {
        crate::platform::chmod_executable(&bin_path)?;
        println!("[INFO] applying update to {}...", candidate.tag_name);
        let mut cmd = Command::new(&bin_path);
        cmd.arg("install");
        if let Some(ref p) = opts.proxy {
            cmd.arg(format!("-install-ghproxy={}", p));
        }
        let status = cmd.status().map_err(|e| {
            let _ = fs::remove_file(&bin_path);
            format!("failed to execute update installer: {}", e)
        })?;
        let _ = fs::remove_file(&bin_path);
        if !status.success() {
            return Err(format!(
                "update installation failed with exit code: {:?}",
                status.code()
            ));
        }
        let lock_file = Path::new(&paths.config_dir).join("auto_update.lock");
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let _ = fs::write(&lock_file, now_unix.to_string().as_bytes());
        println!("[INFO] update to {} completed successfully", candidate.tag_name);
        Ok(())
    }
}

pub fn update_asset_download_url(
    tag: &str,
    asset_name: &str,
    proxy: &str,
) -> Result<String, String> {
    let (owner, repo) = split_update_repo_slug(DEFAULT_UPDATE_REPO)?;
    let raw = format!(
        "https://github.com/{}/{}/releases/download/{}/{}",
        owner, repo, tag, asset_name
    );
    Ok(apply_url_proxy(&raw, proxy))
}

pub fn split_update_repo_slug(slug: &str) -> Result<(String, String), String> {
    let parts: Vec<&str> = slug.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!("invalid repo slug {:?}, expected owner/name", slug));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

pub fn apply_url_proxy(raw: &str, proxy: &str) -> String {
    let proxy = proxy.trim();
    if proxy.is_empty() {
        return raw.to_string();
    }
    format!("{}/{}", proxy.trim_end_matches('/'), raw)
}

pub fn schedule_systemd_unit(unit: &str, cmd_line: &str) -> Result<String, String> {
    if !command_exists("systemctl") {
        return Err("systemctl unavailable under systemd".to_string());
    }
    let service_file = format!("/run/systemd/system/{}.service", unit);
    let content = format!(
        r#"[Unit]
Description=CF Probe auto update

[Service]
Type=oneshot
ExecStart=/bin/sh -c {}

[Install]
WantedBy=multi-user.target
"#,
        quote_systemd_exec_arg(cmd_line)
    );
    fs::write(&service_file, content.as_bytes()).map_err(|e| e.to_string())?;
    let _ = run_command_quiet("systemctl", &["daemon-reload"]);
    let status = Command::new("systemctl")
        .args(["start", &format!("{}.service", unit)])
        .output()
        .map_err(|e| e.to_string())?;
    if status.status.success() {
        Ok(format!("systemd-unit:{}", unit))
    } else {
        Err(format!(
            "systemctl start failed: {}",
            String::from_utf8_lossy(&status.stderr)
        ))
    }
}

pub fn quote_systemd_exec_arg(s: &str) -> String {
    let replaced = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('\n', " ");
    format!("\"{}\"", replaced)
}

pub fn parse_update_version(raw: &str) -> Result<UpdateVersion, String> {
    let mut raw = raw.trim();
    raw = raw.strip_prefix('v').unwrap_or(raw);
    raw = raw.strip_prefix('V').unwrap_or(raw);
    if raw.is_empty() {
        return Err("empty version".to_string());
    }
    if let Some(idx) = raw.find('+') {
        raw = &raw[..idx];
    }
    let mut core = raw;
    let mut pre = Vec::new();
    if let Some(idx) = core.find('-') {
        let pre_raw = &core[idx + 1..];
        core = &core[..idx];
        if pre_raw.is_empty() {
            return Err("empty prerelease".to_string());
        }
        for part in pre_raw.split('.') {
            if part.is_empty() {
                return Err("empty prerelease identifier".to_string());
            }
            pre.push(part.to_string());
        }
    }
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err(format!("invalid semver core {:?}", core));
    }
    let mut nums = [0i32; 3];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err(format!("empty semver component in {:?}", core));
        }
        match part.parse::<i32>() {
            Ok(n) if n >= 0 => nums[i] = n,
            _ => return Err(format!("invalid semver component {:?}", part)),
        }
    }
    Ok(UpdateVersion {
        major: nums[0],
        minor: nums[1],
        patch: nums[2],
        pre,
    })
}

pub fn compare_update_version(a: &UpdateVersion, b: &UpdateVersion) -> Ordering {
    match a.major.cmp(&b.major) {
        Ordering::Equal => match a.minor.cmp(&b.minor) {
            Ordering::Equal => match a.patch.cmp(&b.patch) {
                Ordering::Equal => compare_prerelease(&a.pre, &b.pre),
                other => other,
            },
            other => other,
        },
        other => other,
    }
}

pub fn compare_prerelease(a: &[String], b: &[String]) -> Ordering {
    if a.is_empty() && b.is_empty() {
        return Ordering::Equal;
    }
    if a.is_empty() {
        return Ordering::Greater;
    }
    if b.is_empty() {
        return Ordering::Less;
    }
    for (pa, pb) in a.iter().zip(b.iter()) {
        let a_num = numeric_identifier(pa);
        let b_num = numeric_identifier(pb);
        match (a_num, b_num) {
            (Some(an), Some(bn)) => {
                if an != bn {
                    return an.cmp(&bn);
                }
            }
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            (None, None) => {
                if pa != pb {
                    return pa.cmp(pb);
                }
            }
        }
    }
    a.len().cmp(&b.len())
}

pub fn numeric_identifier(raw: &str) -> Option<i32> {
    if raw.chars().all(|c| c.is_ascii_digit()) {
        raw.parse::<i32>().ok()
    } else {
        None
    }
}
