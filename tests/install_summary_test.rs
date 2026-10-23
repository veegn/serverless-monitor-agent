use serverless_monitor_agent::install_summary::{
    format_management_command, management_commands, ManagementCommand,
};
use serverless_monitor_agent::types::Paths;

#[test]
fn test_management_commands_systemd() {
    let paths = Paths {
        service_name: "sma".to_string(),
        ..Default::default()
    };
    let cmds = management_commands(&paths, "systemd");
    assert_eq!(cmds.len(), 3);
    assert_eq!(cmds[0].command, "journalctl -u sma -f");
    assert_eq!(cmds[1].command, "systemctl status sma");
    assert_eq!(cmds[2].command, "systemctl stop sma");
}

#[test]
fn test_management_commands_systemd_user() {
    let paths = Paths {
        service_name: "sma".to_string(),
        ..Default::default()
    };
    let cmds = management_commands(&paths, "systemd-user");
    assert_eq!(cmds.len(), 3);
    assert_eq!(cmds[0].command, "journalctl --user -u sma -f");
    assert_eq!(cmds[1].command, "systemctl --user status sma");
    assert_eq!(cmds[2].command, "systemctl --user stop sma");
}

#[test]
fn test_management_commands_openrc() {
    let paths = Paths {
        service_name: "sma".to_string(),
        log_file: "/var/log/sma.log".to_string(),
        ..Default::default()
    };
    let cmds = management_commands(&paths, "openrc");
    assert_eq!(cmds.len(), 3);
    assert!(cmds[0].command.starts_with("tail -f"));
    assert_eq!(cmds[1].command, "rc-service sma status");
    assert_eq!(cmds[2].command, "rc-service sma stop");
}

#[test]
fn test_management_commands_procd() {
    let paths = Paths {
        service_name: "sma".to_string(),
        ..Default::default()
    };
    let cmds = management_commands(&paths, "procd");
    assert_eq!(cmds.len(), 3);
    assert_eq!(cmds[0].command, "logread -f -e sma");
    assert_eq!(cmds[1].command, "/etc/init.d/sma status");
    assert_eq!(cmds[2].command, "/etc/init.d/sma stop");
}

#[test]
fn test_management_commands_windows() {
    let paths = Paths {
        service_name: "sma".to_string(),
        log_file: r"C:\ProgramData\sma\sma.log".to_string(),
        ..Default::default()
    };
    let cmds = management_commands(&paths, "windows");
    assert_eq!(cmds.len(), 3);
    assert!(cmds[0].command.contains("powershell"));
    assert_eq!(cmds[1].command, "schtasks /Query /TN sma");
    assert_eq!(cmds[2].command, "schtasks /End /TN sma");
}

#[test]
fn test_format_management_command_uses_copyable_command_line() {
    let cmd = ManagementCommand {
        label: "Live Logs".to_string(),
        command: "powershell -Command ...".to_string(),
    };
    let got = format_management_command(&cmd);
    assert!(!got.contains(" : powershell"));
    assert!(got.contains(" :\n      powershell"));
}
