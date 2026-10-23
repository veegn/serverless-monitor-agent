use crate::platform::{launchd_domain, launchd_plist, service_system, synology_service_file};
use crate::types::*;

const LABEL_REALTIME_LOG: &str = "Live Logs";
const LABEL_STATUS: &str = "Service Status";
const LABEL_STOP: &str = "Stop Service";
const LABEL_COMMANDS: &str = "Management Commands";
const LABEL_CT_NODE: &str = "CT Node";
const LABEL_CU_NODE: &str = "CU Node";
const LABEL_CM_NODE: &str = "CM Node";
const LABEL_BD_NODE: &str = "BD Node";
const LABEL_NODE_1: &str = "Node 1";
const LABEL_NODE_2: &str = "Node 2";
const LABEL_NODE_3: &str = "Node 3";
const LABEL_NODE_4: &str = "Node 4";
const BULLET: &str = "●";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementCommand {
    pub label: String,
    pub command: String,
}

pub fn print_probe_nodes(opts: &InstallOptions) {
    print_probe_node(LABEL_CT_NODE, &opts.ct_node);
    print_probe_node(LABEL_CU_NODE, &opts.cu_node);
    print_probe_node(LABEL_CM_NODE, &opts.cm_node);
    print_probe_node(LABEL_BD_NODE, &opts.bd_node);
    print_probe_node(LABEL_NODE_1, &opts.node_1);
    print_probe_node(LABEL_NODE_2, &opts.node_2);
    print_probe_node(LABEL_NODE_3, &opts.node_3);
    print_probe_node(LABEL_NODE_4, &opts.node_4);
}

pub fn print_probe_node(label: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    println!("    {} {:<10} : {}", BULLET, label, value);
}

pub fn print_management_commands(paths: &Paths) {
    let cmds = management_commands(paths, service_system(paths));
    if cmds.is_empty() {
        return;
    }
    println!("  {} :", LABEL_COMMANDS);
    for cmd in cmds {
        print!("{}", format_management_command(&cmd));
    }
}

pub fn format_management_command(cmd: &ManagementCommand) -> String {
    format!("    {} {} :\n      {}\n", BULLET, cmd.label, cmd.command)
}

pub fn quote_shell(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\\\""))
}

pub fn quote_powershell_display_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn management_commands(paths: &Paths, system: &str) -> Vec<ManagementCommand> {
    match system {
        "systemd" => vec![
            ManagementCommand {
                label: LABEL_REALTIME_LOG.to_string(),
                command: format!("journalctl -u {} -f", paths.service_name),
            },
            ManagementCommand {
                label: LABEL_STATUS.to_string(),
                command: format!("systemctl status {}", paths.service_name),
            },
            ManagementCommand {
                label: LABEL_STOP.to_string(),
                command: format!("systemctl stop {}", paths.service_name),
            },
        ],
        "systemd-user" => vec![
            ManagementCommand {
                label: LABEL_REALTIME_LOG.to_string(),
                command: format!("journalctl --user -u {} -f", paths.service_name),
            },
            ManagementCommand {
                label: LABEL_STATUS.to_string(),
                command: format!("systemctl --user status {}", paths.service_name),
            },
            ManagementCommand {
                label: LABEL_STOP.to_string(),
                command: format!("systemctl --user stop {}", paths.service_name),
            },
        ],
        "openrc" => vec![
            ManagementCommand {
                label: LABEL_REALTIME_LOG.to_string(),
                command: format!("tail -f {}", quote_shell(&paths.log_file)),
            },
            ManagementCommand {
                label: LABEL_STATUS.to_string(),
                command: format!("rc-service {} status", paths.service_name),
            },
            ManagementCommand {
                label: LABEL_STOP.to_string(),
                command: format!("rc-service {} stop", paths.service_name),
            },
        ],
        "procd" => {
            let init_script = format!("/etc/init.d/{}", paths.service_name);
            vec![
                ManagementCommand {
                    label: LABEL_REALTIME_LOG.to_string(),
                    command: format!("logread -f -e {}", paths.service_name),
                },
                ManagementCommand {
                    label: LABEL_STATUS.to_string(),
                    command: format!("{} status", init_script),
                },
                ManagementCommand {
                    label: LABEL_STOP.to_string(),
                    command: format!("{} stop", init_script),
                },
            ]
        }
        "launchd" => {
            let domain = launchd_domain(paths);
            let launchd_target = format!("{}/{}", domain, paths.launchd_label);
            let mut stop_command = format!("launchctl bootout {} {}", domain, quote_shell(&launchd_plist(paths)));
            if !paths.user_mode {
                stop_command = format!("sudo {}", stop_command);
            }
            vec![
                ManagementCommand {
                    label: LABEL_REALTIME_LOG.to_string(),
                    command: format!("tail -f {}", quote_shell(&paths.log_file)),
                },
                ManagementCommand {
                    label: LABEL_STATUS.to_string(),
                    command: format!("launchctl print {}", launchd_target),
                },
                ManagementCommand {
                    label: LABEL_STOP.to_string(),
                    command: stop_command,
                },
            ]
        }
        "upstart" => {
            let upstart_log = format!("/var/log/upstart/{}.log", paths.service_name);
            vec![
                ManagementCommand {
                    label: LABEL_REALTIME_LOG.to_string(),
                    command: format!("tail -f {}", quote_shell(&upstart_log)),
                },
                ManagementCommand {
                    label: LABEL_STATUS.to_string(),
                    command: format!("initctl status {}", paths.service_name),
                },
                ManagementCommand {
                    label: LABEL_STOP.to_string(),
                    command: format!("initctl stop {}", paths.service_name),
                },
            ]
        }
        "synology-rc" => {
            let rc_file = synology_service_file(paths);
            vec![
                ManagementCommand {
                    label: LABEL_REALTIME_LOG.to_string(),
                    command: format!("tail -f {}", quote_shell(&paths.log_file)),
                },
                ManagementCommand {
                    label: LABEL_STATUS.to_string(),
                    command: "ps | grep '[c]f-probe'".to_string(),
                },
                ManagementCommand {
                    label: LABEL_STOP.to_string(),
                    command: format!("{} stop", rc_file),
                },
            ]
        }
        "windows" => vec![
            ManagementCommand {
                label: LABEL_REALTIME_LOG.to_string(),
                command: format!(
                    "powershell -NoProfile -Command \"Get-Content -Path {} -Wait\"",
                    quote_powershell_display_literal(&paths.log_file)
                ),
            },
            ManagementCommand {
                label: LABEL_STATUS.to_string(),
                command: format!("schtasks /Query /TN {}", paths.service_name),
            },
            ManagementCommand {
                label: LABEL_STOP.to_string(),
                command: format!("schtasks /End /TN {}", paths.service_name),
            },
        ],
        _ => vec![
            ManagementCommand {
                label: LABEL_REALTIME_LOG.to_string(),
                command: format!("tail -f {}", quote_shell(&paths.log_file)),
            },
            ManagementCommand {
                label: LABEL_STATUS.to_string(),
                command: format!("kill -0 $(cat {}) && echo running || echo stopped", quote_shell(&paths.pid_file)),
            },
            ManagementCommand {
                label: LABEL_STOP.to_string(),
                command: format!("kill $(cat {})", quote_shell(&paths.pid_file)),
            },
        ],
    }
}
