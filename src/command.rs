use std::process::Command;
use std::time::Duration;

pub const COMMAND_OUTPUT_TIMEOUT: Duration = Duration::from_secs(30);

pub fn quote_shell(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\\\""))
}

pub fn command_output(name: &str, args: &[&str]) -> String {
    command_output_checked(name, args).unwrap_or_default()
}

pub fn command_output_checked(name: &str, args: &[&str]) -> Result<String, String> {
    command_output_with_timeout(COMMAND_OUTPUT_TIMEOUT, name, args)
}

pub fn command_output_with_timeout(timeout: Duration, name: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(name);
    cmd.args(args);

    let output = match wait_timeout(&mut cmd, timeout) {
        Ok(out) => out,
        Err(e) => return Err(format!("{} failed: {}", name, e)),
    };

    if !output.status.success() {
        return Err(format!("{} exited with status: {}", name, output.status));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn command_exists(name: &str) -> bool {
    #[cfg(unix)]
    {
        if name.contains('/') {
            return std::path::Path::new(name).is_file();
        }
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in path_var.split(':') {
                let p = std::path::Path::new(dir).join(name);
                if p.is_file() {
                    return true;
                }
            }
        }
        false
    }
    #[cfg(windows)]
    {
        if name.contains('\\') || name.contains('/') {
            return std::path::Path::new(name).is_file();
        }
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in path_var.split(';') {
                let p = std::path::Path::new(dir).join(name);
                if p.is_file() {
                    return true;
                }
                let exe = std::path::Path::new(dir).join(format!("{}.exe", name));
                if exe.is_file() {
                    return true;
                }
            }
        }
        false
    }
}

pub fn command_line_count(name: &str, args: &[&str]) -> usize {
    let out = command_output(name, args);
    out.lines().count()
}

pub fn run_command(name: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(name)
        .args(args)
        .status()
        .map_err(|e| format!("failed to execute {}: {}", name, e))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with status: {}", name, status))
    }
}

pub fn run_command_quiet(name: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(name)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("failed to execute {}: {}", name, e))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with status: {}", name, status))
    }
}

fn wait_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<std::process::Output> {
    use std::sync::mpsc;
    use std::thread;

    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let res = child.wait_with_output();
        let _ = tx.send(res);
    });

    match rx.recv_timeout(timeout) {
        Ok(res) => res,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "command timed out",
        )),
    }
}

pub fn get_system_hostname() -> String {
    #[cfg(unix)]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        if let Ok(content) = std::fs::read_to_string("/etc/hostname") {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let mut buf = vec![0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            if let Ok(c_str) = std::ffi::CStr::from_bytes_until_nul(&buf) {
                let name = c_str.to_string_lossy().trim().to_string();
                if !name.is_empty() {
                    return name;
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    let out = command_output("hostname", &[]);
    if !out.trim().is_empty() {
        return out.trim().to_string();
    }
    if let Ok(h) = std::env::var("HOSTNAME") {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "localhost".to_string()
}
