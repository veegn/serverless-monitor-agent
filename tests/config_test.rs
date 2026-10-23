use serverless_monitor_agent::storage::config::{
    default_config, normalize_interface_list, read_config, write_config,
};
use serverless_monitor_agent::types::*;

#[test]
fn test_normalize_interface_list() {
    let got = normalize_interface_list(" eth0,ens3,eth0, pppoe-wan ").unwrap();
    assert_eq!(got, "eth0,ens3,pppoe-wan");
}

#[test]
fn test_normalize_interface_list_rejects_bad_name() {
    assert!(normalize_interface_list("eth0, bad/name").is_err());
}

#[test]
fn test_normalize_interface_list_allows_glob() {
    let got = normalize_interface_list(" eth*,en[ops]* ").unwrap();
    assert_eq!(got, "eth*,en[ops]*");
}

#[test]
fn test_config_persists_update_proxy() {
    let dir = std::env::temp_dir().join(format!("test-config-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("config.conf");

    let mut cfg = default_config();
    cfg.server_id = "sid".to_string();
    cfg.secret = "secret".to_string();
    cfg.worker_url = "https://worker.example.com/report".to_string();
    cfg.auto_update = true;
    cfg.update_proxy = "https://gh-proxy.example.com".to_string();
    cfg.connection_mode = CONNECTION_MODE_HTTP.to_string();
    cfg.ping_mode = PING_MODE_ICMP.to_string();

    write_config(&path, cfg.clone()).expect("writeConfig failed");
    let got = read_config(&path).expect("readConfig failed");

    assert_eq!(got.update_proxy, cfg.update_proxy);
    assert!(got.auto_update);
    assert_eq!(got.connection_mode, CONNECTION_MODE_HTTP);
    assert_eq!(got.ping_mode, PING_MODE_ICMP);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_read_config_defaults_connection_mode_auto() {
    let dir = std::env::temp_dir().join(format!("test-config-default-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("config.conf");

    let data = b"SERVER_ID=\"sid\"\nSECRET=\"secret\"\nWORKER_URL=\"https://worker.example.com/update\"\n";
    std::fs::write(&path, data).expect("write config fixture");

    let got = read_config(&path).expect("readConfig failed");
    assert_eq!(got.connection_mode, CONNECTION_MODE_AUTO);
    assert_eq!(got.ping_mode, PING_MODE_TCP);

    let _ = std::fs::remove_dir_all(&dir);
}
