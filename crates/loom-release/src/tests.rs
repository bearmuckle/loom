use std::collections::BTreeMap;

use super::*;

#[test]
fn stable_tags_parse_and_prereleases_do_not() {
    assert_eq!(parse_release_tag("v0.11.0"), Some((0, 11, 0)));
    assert_eq!(parse_release_tag("v1.2.3"), Some((1, 2, 3)));
    for tag in [
        "0.11.0",
        "v0.11",
        "v0.11.0-rc.1",
        "v0.11.0+build",
        "v0.11.0.1",
        "vx.y.z",
    ] {
        assert_eq!(parse_release_tag(tag), None, "{tag}");
    }
}

#[test]
fn only_a_newer_stable_release_is_an_update() {
    assert!(is_newer_stable("v0.11.0", "v0.12.0"));
    assert!(is_newer_stable("v0.11.0", "v1.0.0"));
    assert!(!is_newer_stable("v0.12.0", "v0.12.0"));
    assert!(!is_newer_stable("v0.12.0", "v0.11.9"));
    // A development build has no release tag, so it never updates itself.
    assert!(!is_newer_stable("0.1.0", "v9.9.9"));
    // A pre-release is never selected.
    assert!(!is_newer_stable("v0.11.0", "v0.12.0-rc.1"));
    assert!(!is_newer_stable("v0.11.0", "nightly"));
}

#[test]
fn asset_names_follow_the_platform() {
    assert_eq!(
        asset_name("v1.2.3", "linux-x86_64"),
        (
            "loom-server-v1.2.3-linux-x86_64.tar.gz".to_owned(),
            ArchiveKind::TarGz
        )
    );
    assert_eq!(
        asset_name("v1.2.3", "windows-x86_64"),
        (
            "loom-server-v1.2.3-windows-x86_64.zip".to_owned(),
            ArchiveKind::Zip
        )
    );
    assert_eq!(platform_label("linux", "aarch64"), Some("linux-aarch64"));
    assert_eq!(platform_label("macos", "aarch64"), Some("macos-universal"));
    assert_eq!(platform_label("freebsd", "x86_64"), None);
    assert_eq!(
        server_executable_name(),
        if cfg!(windows) {
            "loom-server.exe"
        } else {
            "loom-server"
        }
    );
}

#[test]
fn a_checksum_manifest_accepts_only_the_listed_digest() {
    let archive = b"release bytes";
    let digest = sha256_hex(archive);
    let sums = parse_sha256sums(&format!("{digest}  loom-server-v1.tar.gz\n"));
    assert!(verify_checksum(&sums, "loom-server-v1.tar.gz", archive).is_ok());

    let error = verify_checksum(&sums, "loom-server-v1.tar.gz", b"other bytes").unwrap_err();
    assert_eq!(error.code, ErrorCode::MalformedPayload);
    let error = verify_checksum(&sums, "not-listed.tar.gz", archive).unwrap_err();
    assert_eq!(error.code, ErrorCode::MalformedPayload);
}

/// A release source backed by in-memory fixtures.
struct Fixture {
    release: ReleaseInfo,
    sums: Vec<u8>,
    archive: Vec<u8>,
}

impl ReleaseSource for Fixture {
    fn release(&self, tag: Option<&str>) -> Result<Option<ReleaseInfo>> {
        Ok(
            (tag.is_none() || tag == Some(self.release.tag_name.as_str()))
                .then(|| self.release.clone()),
        )
    }

    fn download(&self, url: &str) -> Result<Vec<u8>> {
        match url {
            "fixture://sums" => Ok(self.sums.clone()),
            "fixture://archive" => Ok(self.archive.clone()),
            other => Err(LoomError::new(
                ErrorCode::NotFound,
                format!("fixture has no download for '{other}'"),
                false,
            )),
        }
    }
}

fn fixture(tag: &str, platform: &str, archive: Vec<u8>, sums: Option<String>) -> Fixture {
    let (name, _) = asset_name(tag, platform);
    let sums = sums.unwrap_or_else(|| format!("{}  {name}\n", sha256_hex(&archive)));
    Fixture {
        release: ReleaseInfo {
            tag_name: tag.to_owned(),
            assets: vec![
                ReleaseAsset {
                    name: "SHA256SUMS".to_owned(),
                    browser_download_url: "fixture://sums".to_owned(),
                },
                ReleaseAsset {
                    name,
                    browser_download_url: "fixture://archive".to_owned(),
                },
            ],
        },
        sums: sums.into_bytes(),
        archive,
    }
}

#[test]
fn a_verified_download_returns_the_archive_and_a_tampered_one_is_refused() {
    let platform = host_platform().expect("the test host has a release platform");
    let archive = b"server archive".to_vec();
    let source = fixture("v9.9.9", platform, archive.clone(), None);
    let (downloaded, kind) = fetch_verified(&source, &source.release, platform).unwrap();
    assert_eq!(downloaded, archive);
    assert_eq!(kind, asset_name("v9.9.9", platform).1);

    let tampered = fixture(
        "v9.9.9",
        platform,
        archive,
        Some(format!("{}  x\n", sha256_hex(b"other"))),
    );
    let error = fetch_verified(&tampered, &tampered.release, platform).unwrap_err();
    assert_eq!(error.code, ErrorCode::MalformedPayload);
}

#[test]
fn an_archive_without_the_executable_is_rejected() {
    let root = std::env::temp_dir().join(format!("loom-release-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let destination = root.join("loom-server");
    let error = extract_server(b"not an archive", ArchiveKind::TarGz, &destination).unwrap_err();
    assert_eq!(error.code, ErrorCode::MalformedPayload);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_release_repository_is_the_one_client_and_worker_share() {
    assert_eq!(RELEASE_REPOSITORY, "bearmuckle/loom");
    let _: BTreeMap<String, String> = parse_sha256sums("");
}
