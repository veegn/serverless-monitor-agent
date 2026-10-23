use serverless_monitor_agent::net::probe::{median_int, split_probe_target};

#[test]
fn test_split_probe_target() {
    struct TestCase {
        target: &'static str,
        want_host: &'static str,
        want_port: u16,
    }

    let cases = vec![
        TestCase {
            target: "example.com",
            want_host: "example.com",
            want_port: 80,
        },
        TestCase {
            target: "example.com:8443",
            want_host: "example.com",
            want_port: 8443,
        },
        TestCase {
            target: "[2001:db8::1]:443",
            want_host: "2001:db8::1",
            want_port: 443,
        },
        TestCase {
            target: "2001:db8::1",
            want_host: "2001:db8::1",
            want_port: 80,
        },
    ];

    for tc in cases {
        let (host, port) = split_probe_target(tc.target, 80).expect("split_probe_target failed");
        assert_eq!(host, tc.want_host);
        assert_eq!(port, tc.want_port);
    }
}

#[test]
fn test_median_int() {
    assert_eq!(median_int(&[10]), 10);
    assert_eq!(median_int(&[10, 20]), 15);
    assert_eq!(median_int(&[30, 10, 20]), 20);
    assert_eq!(median_int(&[40, 10, 30, 20]), 25);
    assert_eq!(median_int(&[]), -1);
}
