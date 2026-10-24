use serverless_monitor_agent::cli::{
    parse_install_options, parse_uninstall_args, parse_update_options,
};
use serverless_monitor_agent::types::*;

#[test]
fn test_parse_uninstall_args_allows_no_args() {
    assert!(parse_uninstall_args(&[]).is_ok());
}

#[test]
fn test_parse_install_options_rejects_custom_path_and_service() {
    let cases = vec![
        vec!["-install_dir=/tmp/sma".to_string()],
        vec!["-install-dir=/tmp/sma".to_string()],
        vec!["-service_name=probe-a".to_string()],
        vec!["-service-name=probe-a".to_string()],
    ];

    for args in cases {
        assert!(
            parse_install_options(&args).is_err(),
            "parse_install_options({:?}) expected error",
            args
        );
    }
}

#[test]
fn test_parse_uninstall_args_rejects_extra_args() {
    let cases = vec![
        vec!["-service_name=probe-a".to_string()],
        vec!["-service-name=probe-a".to_string()],
        vec!["probe-a".to_string()],
    ];

    for args in cases {
        assert!(
            parse_uninstall_args(&args).is_err(),
            "parse_uninstall_args({:?}) expected error",
            args
        );
    }
}

#[test]
fn test_parse_install_options_auto_update_default_off() {
    let got = parse_install_options(&[]).unwrap();
    assert!(!got.auto_update, "AutoUpdate = true, want false");
}

#[test]
fn test_parse_install_options_auto_update_enabled() {
    let got = parse_install_options(&["-auto_update=1".to_string()]).unwrap();
    assert!(got.auto_update, "AutoUpdate = false, want true");
}

#[test]
fn test_parse_install_options_install_proxy() {
    let got = parse_install_options(&["--install-ghproxy=https://gh-proxy.example.com".to_string()]).unwrap();
    assert_eq!(got.update_proxy, "https://gh-proxy.example.com");
}

#[test]
fn test_parse_install_options_connection_mode() {
    let got = parse_install_options(&["-connection_mode=http".to_string()]).unwrap();
    assert_eq!(got.connection_mode, CONNECTION_MODE_HTTP);

    let got = parse_install_options(&["--connection-mode=websocket".to_string()]).unwrap();
    assert_eq!(got.connection_mode, CONNECTION_MODE_AUTO);

    assert!(parse_install_options(&["-connection_mode=bad".to_string()]).is_err());
}

#[test]
fn test_parse_install_options_ping_mode() {
    let got = parse_install_options(&["-ping_mode=icmp".to_string()]).unwrap();
    assert_eq!(got.ping_mode, PING_MODE_ICMP);

    let got = parse_install_options(&["--ping-mode=tcp".to_string()]).unwrap();
    assert_eq!(got.ping_mode, PING_MODE_TCP);

    assert!(parse_install_options(&["-ping_mode=bad".to_string()]).is_err());
}

#[test]
fn test_parse_install_options_tracks_explicit_flags() {
    let args = vec![
        "-auto_update=1".to_string(),
        "--auto-update=1".to_string(),
        "-collect=5".to_string(),
        "-connection_mode=http".to_string(),
        "-ping_mode=icmp".to_string(),
        "--install-ghproxy=https://gh-proxy.example.com".to_string(),
    ];
    let got = parse_install_options(&args).unwrap();
    for name in &[
        "auto_update",
        "collect_interval",
        "connection_mode",
        "ping_mode",
        "install_ghproxy",
    ] {
        assert!(
            got.explicit.contains(*name),
            "explicit[{:?}] = false, want true (all: {:?})",
            name,
            got.explicit
        );
    }
    assert!(!got.explicit.contains("reset_day"));
    assert!(!got.explicit.contains("interface"));
}

#[test]
fn test_parse_install_options_token_alias() {
    let got = parse_install_options(&[
        "-token=test-token-123".to_string(),
        "-url=https://worker.example.com/update".to_string(),
    ]).unwrap();
    assert_eq!(got.secret, "test-token-123");
    assert_eq!(got.worker_url, "https://worker.example.com/update");
    assert!(got.explicit.contains("secret"));
}

#[test]
fn test_get_system_hostname_non_empty() {
    let host = serverless_monitor_agent::util::command::get_system_hostname();
    assert!(!host.is_empty(), "hostname should not be empty");
}

#[test]
fn test_parse_update_options_defaults() {
    let opts = parse_update_options(&[]).unwrap();
    assert!(!opts.check_only);
    assert!(!opts.force);
    assert!(opts.proxy.is_none());
    assert!(!opts.help);
}

#[test]
fn test_parse_update_options_check_flag() {
    for flag in &["-check", "--check", "-check=1", "--check=true"] {
        let opts = parse_update_options(&[flag.to_string()]).unwrap();
        assert!(opts.check_only, "expected check_only=true for {}", flag);
    }
    let opts = parse_update_options(&["-check=0".to_string()]).unwrap();
    assert!(!opts.check_only);
}

#[test]
fn test_parse_update_options_force_flag() {
    for flag in &["-force", "--force", "-force=1", "--force=true"] {
        let opts = parse_update_options(&[flag.to_string()]).unwrap();
        assert!(opts.force, "expected force=true for {}", flag);
    }
    let opts = parse_update_options(&["-force=0".to_string()]).unwrap();
    assert!(!opts.force);
}

#[test]
fn test_parse_update_options_proxy_flag() {
    let expected = "https://ghproxy.net";
    let cases = vec![
        vec!["-install-ghproxy=https://ghproxy.net".to_string()],
        vec!["--install-ghproxy".to_string(), "https://ghproxy.net".to_string()],
        vec!["-proxy=https://ghproxy.net".to_string()],
        vec!["--proxy".to_string(), "https://ghproxy.net".to_string()],
        vec!["-install_ghproxy=https://ghproxy.net".to_string()],
    ];

    for args in cases {
        let opts = parse_update_options(&args).unwrap();
        assert_eq!(opts.proxy.as_deref(), Some(expected), "args: {:?}", args);
    }
}

#[test]
fn test_parse_update_options_help_flag() {
    for flag in &["-h", "--help", "help"] {
        let opts = parse_update_options(&[flag.to_string()]).unwrap();
        assert!(opts.help, "expected help=true for {}", flag);
    }
}

#[test]
fn test_parse_update_options_rejects_invalid() {
    let cases = vec![
        vec!["-unknown".to_string()],
        vec!["foobar".to_string()],
        vec!["-proxy=".to_string()],
        vec!["-proxy".to_string()],
    ];

    for args in cases {
        assert!(parse_update_options(&args).is_err(), "expected error for {:?}", args);
    }
}
