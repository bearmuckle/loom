use std::{cell::RefCell, collections::BTreeMap, io::Write};

use super::*;

/// A release source backed by in-memory fixtures; it records every download.
#[derive(Default)]
struct FixtureReleases {
    releases: BTreeMap<String, ReleaseInfo>,
    latest: Option<String>,
    files: BTreeMap<String, Vec<u8>>,
    offline: bool,
    downloads: RefCell<Vec<String>>,
}

impl FixtureReleases {
    fn publish(&mut self, tag: &str, platform: &str, archive: Vec<u8>, sums: Option<String>) {
        let (name, _) = asset_name(tag, platform);
        let mut assets = vec![ReleaseAsset {
            name: name.clone(),
            browser_download_url: format!("fixture://{tag}/{name}"),
        }];
        let sums = sums.unwrap_or_else(|| format!("{}  {name}\n", sha256_hex(&archive)));
        assets.push(ReleaseAsset {
            name: "SHA256SUMS".to_owned(),
            browser_download_url: format!("fixture://{tag}/SHA256SUMS"),
        });
        self.files
            .insert(format!("fixture://{tag}/{name}"), archive);
        self.files
            .insert(format!("fixture://{tag}/SHA256SUMS"), sums.into_bytes());
        self.releases.insert(
            tag.to_owned(),
            ReleaseInfo {
                tag_name: tag.to_owned(),
                assets,
            },
        );
        self.latest = Some(tag.to_owned());
    }
}

impl ReleaseSource for FixtureReleases {
    fn release(&self, tag: Option<&str>) -> Result<Option<ReleaseInfo>> {
        if self.offline {
            return Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                "offline",
                true,
            ));
        }
        let tag = match tag {
            Some(tag) => tag.to_owned(),
            None => match &self.latest {
                Some(tag) => tag.clone(),
                None => return Ok(None),
            },
        };
        Ok(self.releases.get(&tag).cloned())
    }

    fn download(&self, url: &str) -> Result<Vec<u8>> {
        self.downloads.borrow_mut().push(url.to_owned());
        self.files
            .get(url)
            .cloned()
            .ok_or_else(|| LoomError::new(ErrorCode::NotFound, url.to_owned(), false))
    }
}

const SERVER_BYTES: &[u8] = b"#!/bin/sh\necho fixture loom-server\n";

fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for (path, contents) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, *contents).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn zip_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cursor = io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut cursor);
    for (path, contents) in entries {
        writer
            .start_file(
                *path,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(contents).unwrap();
    }
    writer.finish().unwrap();
    cursor.into_inner()
}

fn server_archive(platform: &str) -> Vec<u8> {
    let name = server_executable_name();
    let entries: [(&str, &[u8]); 3] = [
        ("LICENSE-AGPL", b"license"),
        (name, SERVER_BYTES),
        ("LICENSES.md", b"licenses"),
    ];
    if platform.starts_with("windows") {
        zip_archive(&entries)
    } else {
        tar_gz(&entries)
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let path = env::temp_dir().join(format!("loom-worker-{label}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn resolver<'a>(
    cache: &Path,
    source: &'a FixtureReleases,
    platform: &'static str,
) -> ServerResolver<'a> {
    ServerResolver {
        explicit: None,
        search_dirs: Vec::new(),
        cache_root: cache.to_path_buf(),
        preferred_tag: None,
        platform: Some(platform),
        source,
    }
}

#[test]
fn platforms_and_asset_names_match_the_release_workflow() {
    assert_eq!(platform_label("linux", "x86_64"), Some("linux-x86_64"));
    assert_eq!(platform_label("linux", "aarch64"), Some("linux-aarch64"));
    assert_eq!(platform_label("macos", "aarch64"), Some("macos-universal"));
    assert_eq!(platform_label("macos", "x86_64"), Some("macos-universal"));
    assert_eq!(platform_label("windows", "x86_64"), Some("windows-x86_64"));
    assert_eq!(platform_label("freebsd", "x86_64"), None);
    assert_eq!(
        asset_name("v0.11.0", "linux-x86_64"),
        (
            "loom-server-v0.11.0-linux-x86_64.tar.gz".to_owned(),
            ArchiveKind::TarGz
        )
    );
    assert_eq!(
        asset_name("v0.11.0", "windows-x86_64"),
        (
            "loom-server-v0.11.0-windows-x86_64.zip".to_owned(),
            ArchiveKind::Zip
        )
    );
    assert_eq!(parse_tag("v0.11.0"), Some((0, 11, 0)));
    assert_eq!(parse_tag("v1.2.3-rc.1"), Some((1, 2, 3)));
    assert_eq!(parse_tag("0.1.0"), None);
    assert_eq!(parse_tag("vmain"), None);
}

#[test]
fn sha256sums_are_parsed_and_enforced() {
    let archive = b"archive bytes";
    let digest = sha256_hex(archive);
    let sums = parse_sha256sums(&format!(
        "{digest}  loom-server-v1.0.0-linux-x86_64.tar.gz\n{}  *other.zip\nnot a line\n",
        "A".repeat(64)
    ));
    assert_eq!(sums.len(), 2);
    assert_eq!(sums.get("other.zip"), Some(&"a".repeat(64)));
    verify_checksum(&sums, "loom-server-v1.0.0-linux-x86_64.tar.gz", archive).unwrap();

    let mismatch =
        verify_checksum(&sums, "loom-server-v1.0.0-linux-x86_64.tar.gz", b"tampered").unwrap_err();
    assert_eq!(mismatch.code, ErrorCode::MalformedPayload);
    assert!(mismatch.message.contains("does not match"), "{mismatch}");

    let missing = verify_checksum(&sums, "missing.tar.gz", archive).unwrap_err();
    assert!(missing.message.contains("no entry"), "{missing}");
}

#[test]
fn extraction_takes_only_the_server_executable() {
    let directory = temp_dir("extract");
    for kind in [ArchiveKind::TarGz, ArchiveKind::Zip] {
        let destination = directory.join(format!("{kind:?}"));
        let archive = match kind {
            ArchiveKind::TarGz => server_archive("linux-x86_64"),
            ArchiveKind::Zip => server_archive("windows-x86_64"),
        };
        extract_server(&archive, kind, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), SERVER_BYTES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&destination).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "extracted server must be executable");
        }
    }
    let empty = tar_gz(&[("README", b"no server here")]);
    let error = extract_server(&empty, ArchiveKind::TarGz, &directory.join("none")).unwrap_err();
    assert!(error.message.contains("does not contain"), "{error}");
    let error =
        extract_server(b"not an archive", ArchiveKind::Zip, &directory.join("bad")).unwrap_err();
    assert_eq!(error.code, ErrorCode::MalformedPayload);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn download_is_verified_installed_and_then_served_from_the_cache() {
    let cache = temp_dir("download");
    let platform = "linux-x86_64";
    let mut source = FixtureReleases::default();
    source.publish("v0.10.0", platform, server_archive(platform), None);
    source.publish("v0.11.0", platform, server_archive(platform), None);

    let resolver = resolver(&cache, &source, platform);
    let path = resolver.resolve().unwrap();
    assert_eq!(
        path,
        cache
            .join("server")
            .join("v0.11.0")
            .join(server_executable_name())
    );
    assert_eq!(fs::read(&path).unwrap(), SERVER_BYTES);
    assert_eq!(source.downloads.borrow().len(), 2);
    // No staging directory is left behind.
    let leftovers: Vec<_> = fs::read_dir(cache.join("server"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(leftovers.is_empty());

    // The second resolution is a cache hit: no further download.
    assert_eq!(resolver.resolve().unwrap(), path);
    assert_eq!(source.downloads.borrow().len(), 2);

    // A client built as a tag prefers that tag over the latest release.
    let mut pinned = super::tests::resolver(&cache, &source, platform);
    pinned.preferred_tag = Some("v0.10.0".to_owned());
    let pinned_path = pinned.resolve().unwrap();
    assert!(pinned_path.starts_with(cache.join("server").join("v0.10.0")));

    // A pinned tag that is already cached needs no release lookup at all.
    let offline = FixtureReleases {
        offline: true,
        ..FixtureReleases::default()
    };
    let mut cached = super::tests::resolver(&cache, &offline, platform);
    cached.preferred_tag = Some("v0.10.0".to_owned());
    assert_eq!(cached.resolve().unwrap(), pinned_path);

    // Offline without a matching tag falls back to the newest cached version.
    let mut fallback = super::tests::resolver(&cache, &offline, platform);
    fallback.preferred_tag = Some("v0.12.0".to_owned());
    assert_eq!(fallback.resolve().unwrap(), path);
    fs::remove_dir_all(cache).unwrap();
}

#[test]
fn unknown_preferred_tag_falls_back_to_latest() {
    let cache = temp_dir("latest");
    let platform = "windows-x86_64";
    let mut source = FixtureReleases::default();
    source.publish("v0.11.0", platform, server_archive(platform), None);
    let mut resolver = resolver(&cache, &source, platform);
    resolver.preferred_tag = Some("v9.9.9".to_owned());
    let path = resolver.resolve().unwrap();
    assert!(path.starts_with(cache.join("server").join("v0.11.0")));
    fs::remove_dir_all(cache).unwrap();
}

#[test]
fn tampered_or_unlisted_archives_are_refused_and_not_cached() {
    let cache = temp_dir("tampered");
    let platform = "linux-aarch64";
    let mut source = FixtureReleases::default();
    source.publish(
        "v0.11.0",
        platform,
        server_archive(platform),
        Some(format!(
            "{}  loom-server-v0.11.0-{platform}.tar.gz\n",
            "0".repeat(64)
        )),
    );
    let error = resolver(&cache, &source, platform).resolve().unwrap_err();
    assert!(
        error.message.contains("does not match SHA256SUMS"),
        "{error}"
    );
    assert!(!cache.join("server").join("v0.11.0").exists());

    let mut unlisted = FixtureReleases::default();
    unlisted.publish(
        "v0.11.0",
        platform,
        server_archive(platform),
        Some(String::new()),
    );
    let error = resolver(&cache, &unlisted, platform).resolve().unwrap_err();
    assert!(error.message.contains("no entry"), "{error}");

    let mut no_sums = FixtureReleases::default();
    no_sums.publish("v0.11.0", platform, server_archive(platform), None);
    no_sums
        .releases
        .get_mut("v0.11.0")
        .unwrap()
        .assets
        .retain(|asset| asset.name != "SHA256SUMS");
    let error = resolver(&cache, &no_sums, platform).resolve().unwrap_err();
    assert!(error.message.contains("no SHA256SUMS"), "{error}");

    // A release without this platform's asset is reported by name.
    let error = resolver(&cache, &no_sums, "linux-x86_64")
        .resolve()
        .unwrap_err();
    assert!(error.message.contains("has no asset"), "{error}");
    fs::remove_dir_all(cache).unwrap();
}

#[test]
fn explicit_and_local_binaries_win_over_the_cache() {
    let directory = temp_dir("local");
    let source = FixtureReleases {
        offline: true,
        ..FixtureReleases::default()
    };
    let binary = directory.join(server_executable_name());
    fs::write(&binary, SERVER_BYTES).unwrap();

    let mut explicit = resolver(&directory, &source, "linux-x86_64");
    explicit.explicit = Some(binary.clone());
    assert_eq!(explicit.resolve().unwrap(), binary);
    explicit.explicit = Some(directory.join("missing"));
    assert_eq!(
        explicit.resolve().unwrap_err().code,
        ErrorCode::InvalidRequest
    );

    let mut search = resolver(&directory, &source, "linux-x86_64");
    search.search_dirs = vec![directory.join("empty"), directory.clone()];
    assert_eq!(search.resolve().unwrap(), binary);

    // Nothing local, nothing cached, and no network: a clear error.
    let empty = temp_dir("empty");
    let error = resolver(&empty, &source, "linux-x86_64")
        .resolve()
        .unwrap_err();
    assert!(error.message.contains("--server-binary"), "{error}");

    // A host without a release platform cannot download.
    let mut unsupported = resolver(&empty, &source, "linux-x86_64");
    unsupported.platform = None;
    assert_eq!(
        unsupported.resolve().unwrap_err().code,
        ErrorCode::UnsupportedCapability
    );
    fs::remove_dir_all(directory).unwrap();
    fs::remove_dir_all(empty).unwrap();
}

#[test]
fn release_metadata_parses_the_github_shape() {
    let release: ReleaseInfo = serde_json::from_str(
        r#"{"tag_name":"v0.11.0","name":"v0.11.0","assets":[
            {"name":"SHA256SUMS","size":10,
             "browser_download_url":"https://example.invalid/SHA256SUMS"}]}"#,
    )
    .unwrap();
    assert_eq!(release.tag_name, "v0.11.0");
    assert!(release.asset("SHA256SUMS").is_some());
    assert!(release.asset("other").is_none());
    let _ = client_release_tag();
    assert!(cache_root().ends_with("loom") || env::var_os(CACHE_DIR_ENV).is_some());
}

#[test]
fn private_files_are_owner_only() {
    let directory = temp_dir("token");
    let path = directory.join("nested").join("token");
    write_private_file(&path, "secret").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "secret");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn health_probe_reports_closed_ports_and_http_status() {
    let port = free_loopback_port().unwrap();
    assert!(!health_ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port))));

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for reply in [
            "HTTP/1.1 200 OK\r\n\r\nok",
            "HTTP/1.1 503 Unavailable\r\n\r\n",
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            stream.write_all(reply.as_bytes()).unwrap();
        }
    });
    assert!(health_ok(address));
    assert!(!health_ok(address));
    server.join().unwrap();
}

/// End to end against a real `loom-server` when `LOOM_TEST_SERVER_BINARY`
/// names one (for example `target/debug/loom-server`); skipped otherwise,
/// because a dev-dependency cannot provide another package's binary.
#[test]
fn a_real_server_starts_answers_and_stops() {
    let Some(binary) = env::var_os("LOOM_TEST_SERVER_BINARY").map(PathBuf::from) else {
        return;
    };
    let directory = temp_dir("real");
    let worker = ManagedWorker::start(&ManagedWorkerOptions {
        binary,
        persistence: directory.join("state.db"),
        reset_state: false,
        token_file: directory.join("worker").join("token"),
        port: None,
    })
    .unwrap();
    assert!(health_ok(worker.address()));
    let connection = crate::RemoteConnection::connect(
        &worker.websocket_url(),
        worker.token(),
        true,
        &crate::RemoteConnectionOptions::default(),
    )
    .unwrap();
    let response = connection.request(loom_protocol::RequestEnvelope::new(
        loom_protocol::ClientRequest::Control(loom_protocol::ControlRequest::DiscoverCapabilities),
    ));
    assert!(response.result.is_ok(), "{:?}", response.result);
    connection.close().unwrap();
    let address = worker.address();
    drop(worker);
    assert!(!health_ok(address));
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn a_worker_that_exits_during_startup_is_reported() {
    let directory = temp_dir("exit");
    let options = ManagedWorkerOptions {
        binary: PathBuf::from("/bin/false"),
        persistence: directory.join("state.db"),
        reset_state: true,
        token_file: directory.join("worker").join("token"),
        port: None,
    };
    let error = ManagedWorker::start(&options)
        .err()
        .expect("start must fail");
    assert!(error.message.contains("exited during startup"), "{error}");
    // The failed worker's token file is removed with it.
    assert!(!options.token_file.exists());

    let missing = ManagedWorkerOptions {
        binary: directory.join("missing"),
        ..options
    };
    let error = ManagedWorker::start(&missing)
        .err()
        .expect("spawn must fail");
    assert!(error.message.contains("could not start"), "{error}");
    fs::remove_dir_all(directory).unwrap();
}
