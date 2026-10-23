use serverless_monitor_agent::net::iplookup::{
    extract_ip_from_body, IPV4_LOOKUP_ENDPOINTS, IPV6_LOOKUP_ENDPOINTS,
};

#[test]
fn test_extract_ip_from_body() {
    struct TestCase {
        name: &'static str,
        body: &'static str,
        want_v4: bool,
        want: &'static str,
    }

    let cases = vec![
        TestCase {
            name: "trace ipv4",
            body: "fl=123f45\nh=cloudflare.com\nip=203.0.113.10\nts=1700000000.1\n",
            want_v4: true,
            want: "203.0.113.10",
        },
        TestCase {
            name: "trace ipv6",
            body: "fl=123f45\nh=cloudflare.com\nip=2001:db8::1\nts=1700000000.1\n",
            want_v4: false,
            want: "2001:db8::1",
        },
        TestCase {
            name: "trace ipv4 wants v6",
            body: "ip=203.0.113.10\n",
            want_v4: false,
            want: "",
        },
        TestCase {
            name: "trace ipv6 wants v4",
            body: "ip=2001:db8::1\n",
            want_v4: true,
            want: "",
        },
        TestCase {
            name: "json ipv4",
            body: r#"{"ip":"198.51.100.7"}"#,
            want_v4: true,
            want: "198.51.100.7",
        },
        TestCase {
            name: "plain ipv4",
            body: "192.0.2.1\n",
            want_v4: true,
            want: "192.0.2.1",
        },
        TestCase {
            name: "json ipv6",
            body: r#"{"ip":"2001:db8:85a3::8a2e:370:7334"}"#,
            want_v4: false,
            want: "2001:db8:85a3::8a2e:370:7334",
        },
        TestCase {
            name: "plain ipv6",
            body: "2001:db8::ff00:42:8329",
            want_v4: false,
            want: "2001:db8::ff00:42:8329",
        },
        TestCase {
            name: "invalid ipv4 octets",
            body: "999.999.999.999",
            want_v4: true,
            want: "",
        },
        TestCase {
            name: "empty body",
            body: "",
            want_v4: true,
            want: "",
        },
        TestCase {
            name: "geo ipv4 with extra text",
            body: r#"{"country":"CN","ip":"203.0.113.99","asn":4134}"#,
            want_v4: true,
            want: "203.0.113.99",
        },
    ];

    for tc in cases {
        let got = extract_ip_from_body(tc.body, tc.want_v4);
        assert_eq!(got, tc.want, "{}: body={}", tc.name, tc.body);
    }
}

#[test]
fn test_lookup_endpoints_configured() {
    assert!(!IPV4_LOOKUP_ENDPOINTS.is_empty());
    assert!(!IPV6_LOOKUP_ENDPOINTS.is_empty());
}
