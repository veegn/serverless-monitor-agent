use serverless_monitor_agent::cli::{
    parse_install_options, parse_uninstall_args,
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
    let host = serverless_monitor_agent::command::get_system_hostname();
    assert!(!host.is_empty(), "hostname should not be empty");
}
