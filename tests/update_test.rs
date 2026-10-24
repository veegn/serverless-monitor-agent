use serverless_monitor_agent::update::{
    checksum_for_asset, compare_update_version, expected_update_asset_name,
    parse_update_version, select_latest_snapshot_release, select_latest_stable_release,
    update_asset_download_url, update_asset_name_for_arm, GithubRelease, GithubReleaseAsset,
    DEFAULT_UPDATE_REPO,
};
use std::cmp::Ordering;

#[test]
fn test_parse_update_version_and_compare() {
    struct TestCase {
        current: &'static str,
        latest: &'static str,
        want: Ordering,
    }

    let cases = vec![
        TestCase {
            current: "1.0.0",
            latest: "v1.0.1",
            want: Ordering::Greater,
        },
        TestCase {
            current: "v1.2.3",
            latest: "1.2.3",
            want: Ordering::Equal,
        },
        TestCase {
            current: "1.2.4",
            latest: "1.2.3",
            want: Ordering::Less,
        },
        TestCase {
            current: "1.2.3-beta.1",
            latest: "1.2.3",
            want: Ordering::Greater,
        },
        TestCase {
            current: "1.2.3",
            latest: "1.2.3-beta.1",
            want: Ordering::Less,
        },
        TestCase {
            current: "1.2.3-beta.1",
            latest: "1.2.3-beta.2",
            want: Ordering::Greater,
        },
    ];

    for tc in cases {
        let current = parse_update_version(tc.current).expect("parse current version");
        let latest = parse_update_version(tc.latest).expect("parse latest version");
        let got = compare_update_version(&latest, &current);
        assert_eq!(
            got, tc.want,
            "compare {} to {}",
            tc.latest, tc.current
        );
    }
}

#[test]
fn test_expected_update_asset_name() {
    assert_eq!(
        expected_update_asset_name("linux", "amd64"),
        "sma-linux-amd64"
    );
    assert_eq!(
        expected_update_asset_name("darwin", "arm64"),
        "sma-darwin-arm64"
    );
    assert_eq!(
        expected_update_asset_name("windows", "amd64"),
        "sma-windows-amd64.exe"
    );
}

#[test]
fn test_linux_arm_update_matches_published_assets() {
    let cases = vec![
        ("5", "sma-linux-armv5"),
        ("5,softfloat", "sma-linux-armv5"),
        ("5,hardfloat", "sma-linux-armv5"),
        ("6", "sma-linux-armv6"),
        ("6,hardfloat", "sma-linux-armv6"),
        ("6,softfloat", ""),
        ("7", "sma-linux-armv7"),
        ("7,hardfloat", "sma-linux-armv7"),
        ("7,softfloat", ""),
        ("", ""),
        ("8", ""),
        ("6,unknown", ""),
        ("7,hardfloat,softfloat", ""),
    ];

    for (goarm, want) in cases {
        let asset = update_asset_name_for_arm("linux", "arm", goarm);
        assert_eq!(asset, want, "GOARM={}", goarm);
    }
    assert_eq!(
        update_asset_name_for_arm("freebsd", "arm", ""),
        "sma-freebsd-arm"
    );
}

#[test]
fn test_checksum_for_asset() {
    let checksums = r#"
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  dist/sma-linux-amd64
ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb *sma-windows-amd64.exe
"#;
    assert_eq!(
        checksum_for_asset(checksums, "sma-linux-amd64").unwrap(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        checksum_for_asset(checksums, "sma-windows-amd64.exe").unwrap(),
        "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"
    );
    assert!(checksum_for_asset(checksums, "sma-darwin-arm64").is_none());
}

fn make_release(tag: &str, draft: bool, prerelease: bool, asset: &str) -> GithubRelease {
    GithubRelease {
        tag_name: tag.to_string(),
        name: tag.to_string(),
        draft,
        prerelease,
        published_at: None,
        assets: vec![GithubReleaseAsset {
            name: asset.to_string(),
            size: 1024,
        }],
    }
}

#[test]
fn test_select_latest_stable_release() {
    let asset_name = "sma-linux-amd64";
    let current = parse_update_version("1.0.0").unwrap();
    let releases = vec![
        make_release("v1.0.1", false, false, "sma-linux-arm64"),
        make_release("v1.0.2", false, true, asset_name),
        make_release("v1.0.3", true, false, asset_name),
        make_release("v1.0.1", false, false, asset_name),
        make_release("v0.9.9", false, false, asset_name),
    ];

    let candidate = select_latest_stable_release(&releases, asset_name, &current);
    assert!(candidate.is_some());
    assert_eq!(candidate.unwrap().tag_name, "v1.0.1");
}

#[test]
fn test_select_latest_snapshot_release() {
    let asset_name = "sma-linux-amd64";
    let releases = vec![
        make_release("v9.9.9", false, false, asset_name),
        make_release("Snapshot-2608051400", true, true, asset_name), // draft=true
        make_release("beta-2608051500", false, true, asset_name),    // not Snapshot- prefix
        make_release("Snapshot-2608051600", false, true, "sma-linux-arm64"), // different asset
        make_release("Snapshot-2608051200", false, true, asset_name),
        make_release("Snapshot-2608051300", false, true, asset_name),
    ];

    let candidate = select_latest_snapshot_release(&releases, asset_name);
    assert!(candidate.is_some());
    assert_eq!(candidate.unwrap().tag_name, "Snapshot-2608051300");
}

#[test]
fn test_update_asset_download_url() {
    let url = update_asset_download_url("v1.2.3", "sma-linux-amd64", "").unwrap();
    let expected = format!(
        "https://github.com/{}/releases/download/v1.2.3/sma-linux-amd64",
        DEFAULT_UPDATE_REPO
    );
    assert_eq!(url, expected);

    let proxy_url = update_asset_download_url(
        "v1.2.3",
        "sma-linux-amd64",
        "https://ghproxy.example.com",
    )
    .unwrap();
    assert_eq!(proxy_url, format!("https://ghproxy.example.com/{}", expected));
}

#[test]
fn test_select_latest_stable_release_force_selects_highest_even_if_current_is_latest() {
    let asset_name = "sma-linux-amd64";
    let releases = vec![
        make_release("v1.0.23", false, false, asset_name),
        make_release("v1.0.22", false, false, asset_name),
    ];

    let current = parse_update_version("1.0.23").unwrap();
    // Regular check: already up to date
    let regular = select_latest_stable_release(&releases, asset_name, &current);
    assert!(regular.is_none());

    // Force check (with 0.0.0): selects v1.0.23
    let zero = parse_update_version("0.0.0").unwrap();
    let forced = select_latest_stable_release(&releases, asset_name, &zero);
    assert!(forced.is_some());
    assert_eq!(forced.unwrap().tag_name, "v1.0.23");
}
