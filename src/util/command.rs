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

pub fn command_output_with_timeout(
    timeout: Duration,
    name: &str,
    args: &[&str],
) -> Result<String, String> {
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
    use std::thread;
    use std::time::Instant;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    #[cfg(windows)]
    let process_job = WindowsProcessJob::attach(&child);

    // Drain both pipes while polling the child. Waiting on the process without
    // readers can deadlock when a command fills stdout or stderr.
    let mut stdout = child.stdout.take().ok_or_else(|| {
        std::io::Error::other("failed to capture command stdout")
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        std::io::Error::other("failed to capture command stderr")
    })?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stderr, &mut bytes).map(|_| bytes)
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                terminate_process_tree(&mut child, #[cfg(windows)] process_job.as_ref());
                let _ = child.wait();
                break Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "command timed out",
                ));
            }
            Err(error) => {
                terminate_process_tree(&mut child, #[cfg(windows)] process_job.as_ref());
                let _ = child.wait();
                break Err(error);
            }
        }
    };

    // A command can exit while a descendant still holds one of its pipes open.
    // End those descendants before joining the readers so completion remains
    // bounded even when a tool daemonizes unexpectedly.
    if status.is_ok() {
        terminate_process_tree(&mut child, #[cfg(windows)] process_job.as_ref());
    }

    // The child has exited or was killed, so both pipe readers can now finish.
    let stdout = stdout_reader
        .join()
        .map_err(|_| std::io::Error::other("stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| std::io::Error::other("stderr reader panicked"))??;

    match status {
        Ok(status) => Ok(std::process::Output {
            status,
            stdout,
            stderr,
        }),
        Err(error) => Err(error),
    }
}

fn terminate_process_tree(
    child: &mut std::process::Child,
    #[cfg(windows)] process_job: Option<&WindowsProcessJob>,
) {
    #[cfg(unix)]
    unsafe {
        // Commands such as shells can start descendants that inherit stdout or
        // stderr. Put the command in its own process group so timeout cleanup
        // also closes those pipes and stops the whole command tree.
        let process_group = child.id() as libc::pid_t;
        let _ = libc::kill(-process_group, libc::SIGKILL);
    }
    #[cfg(windows)]
    if let Some(job) = process_job {
        job.terminate();
    }
    let _ = child.kill();
}

#[cfg(windows)]
struct WindowsProcessJob(isize);

#[cfg(windows)]
impl WindowsProcessJob {
    fn attach(child: &std::process::Child) -> Option<Self> {
        use std::mem::{size_of, zeroed};
        use std::os::windows::io::{AsHandle, AsRawHandle};
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JobObjectExtendedLimitInformation,
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job == 0 {
            return None;
        }

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        let process_handle = child.as_handle().as_raw_handle() as isize;
        if configured == 0 || unsafe { AssignProcessToJobObject(job, process_handle) } == 0 {
            unsafe { CloseHandle(job) };
            None
        } else {
            Some(Self(job))
        }
    }

    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        unsafe { TerminateJobObject(self.0, 1) };
    }
}

#[cfg(windows)]
impl Drop for WindowsProcessJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(self.0) };
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
