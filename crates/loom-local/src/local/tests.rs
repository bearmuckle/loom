use std::{collections::BTreeMap, fs, path::Path};

use flate2::{Compression, write::GzEncoder};

use super::*;
use crate::worker::{
    ArchiveKind, ReleaseAsset, ReleaseInfo, asset_name, server_executable_name, sha256_hex,
};

fn temporary_state() -> PathBuf {
    std::env::temp_dir().join(format!("loom-local-worker-{}", uuid::Uuid::new_v4()))
}

fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (name, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, *bytes).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

#[derive(Default)]
struct Fixture {
    release: Option<ReleaseInfo>,
    downloads: BTreeMap<String, Vec<u8>>,
}

impl ReleaseSource for Fixture {
    fn release(&self, tag: Option<&str>) -> Result<Option<ReleaseInfo>> {
        let release = self.release.clone();
        Ok(release.filter(|release| tag.is_none_or(|tag| tag == release.tag_name)))
    }

    fn download(&self, url: &str) -> Result<Vec<u8>> {
        self.downloads.get(url).cloned().ok_or_else(|| {
            LoomError::new(
                ErrorCode::NotFound,
                format!("fixture has no download for '{url}'"),
                false,
            )
        })
    }
}

#[test]
fn the_state_directory_holds_the_server_token_and_database() {
    let paths = LocalWorkerPaths::for_state_dir("/state/loom");
    assert_eq!(paths.token(), Path::new("/state/loom/local-worker/token"));
    assert_eq!(
        paths.log(),
        Path::new("/state/loom/local-worker/server.log")
    );
    assert_eq!(
        paths.pid(),
        Path::new("/state/loom/local-worker/server.pid")
    );
    assert_eq!(paths.persistence(), Path::new("/state/loom/state.db"));
    assert_eq!(
        paths.binary("v1.2.3"),
        Path::new("/state/loom/server/v1.2.3").join(server_executable_name())
    );
}

#[test]
fn the_unit_runs_the_installed_binary_and_restarts_it() {
    let unit = unit_contents(
        Path::new("/state/loom/server/v1.2.3/loom-server"),
        Path::new("/state/loom/local-worker/token"),
        Path::new("/state/loom/state.db"),
        8765,
    );
    assert!(unit.contains(
        "ExecStart=\"/state/loom/server/v1.2.3/loom-server\" --bind 127.0.0.1:8765 \
         --token-file \"/state/loom/local-worker/token\" --persistence \"/state/loom/state.db\""
    ));
    assert!(unit.contains("Restart=on-failure"));
    assert!(unit.contains("WantedBy=default.target"));
}

#[test]
fn unit_arguments_are_quoted_so_a_path_with_spaces_stays_one_argument() {
    let unit = unit_contents(
        Path::new("/home/a b/loom-server"),
        Path::new("/home/a b/token"),
        Path::new("/home/a b/state.db"),
        9000,
    );
    assert!(
        unit.contains("ExecStart=\"/home/a b/loom-server\""),
        "{unit}"
    );
    assert!(unit.contains("--token-file \"/home/a b/token\""), "{unit}");
    assert!(
        unit.contains("--persistence \"/home/a b/state.db\""),
        "{unit}"
    );
}

#[cfg(unix)]
#[test]
fn the_token_is_owner_only_and_survives_a_second_install() {
    use std::os::unix::fs::PermissionsExt;

    let root = temporary_state();
    let paths = LocalWorkerPaths::for_state_dir(&root);
    let token = ensure_token(&paths.token()).unwrap();
    assert!(token.starts_with("loom-"));
    let mode = fs::metadata(paths.token()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    assert_eq!(ensure_token(&paths.token()).unwrap(), token);
    assert_eq!(installed_token_for(&paths).as_deref(), Some(token.as_str()));
    fs::remove_dir_all(root).unwrap();
}

fn installed_token_for(paths: &LocalWorkerPaths) -> Option<String> {
    read_token(&paths.token())
}

#[cfg(unix)]
#[test]
fn an_install_writes_the_executable_owner_executable() {
    use std::os::unix::fs::PermissionsExt;

    let root = temporary_state();
    let paths = LocalWorkerPaths::for_state_dir(&root);
    let destination = paths.binary("v1.2.3");
    let archive = tar_gz(&[
        ("README.md", b"release notes"),
        (server_executable_name(), b"fake server"),
    ]);
    install_verified(&destination, &archive, ArchiveKind::TarGz).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"fake server");
    let mode = fs::metadata(&destination).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
    // The staging directory is removed, so only complete installs remain.
    let siblings: Vec<_> = fs::read_dir(destination.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(siblings, vec![server_executable_name()]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_archive_without_the_server_is_refused() {
    let root = temporary_state();
    let paths = LocalWorkerPaths::for_state_dir(&root);
    let archive = tar_gz(&[("notes.txt", b"nothing to install")]);
    let error =
        install_verified(&paths.binary("v1.2.3"), &archive, ArchiveKind::TarGz).unwrap_err();
    assert_eq!(error.code, ErrorCode::MalformedPayload);
    assert!(!paths.binary("v1.2.3").exists());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn a_bundled_server_is_copied_into_the_state_directory() {
    use std::os::unix::fs::PermissionsExt;

    let root = temporary_state();
    let paths = LocalWorkerPaths::for_state_dir(&root);
    let source = root.join("bundle").join(server_executable_name());
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, b"bundled server").unwrap();
    let destination = paths.binary("v1.2.3");

    install_bundled(&source, &destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"bundled server");
    let mode = fs::metadata(&destination).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);

    // Installing the same binary again keeps the existing file.
    install_bundled(&source, &destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"bundled server");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_latest_release_is_downloaded_verified_and_installed() {
    let root = temporary_state();
    let paths = LocalWorkerPaths::for_state_dir(&root);
    let tag = "v9.9.9";
    let platform = host_platform().expect("the test host has a release platform");
    let (name, _) = asset_name(tag, platform);
    let archive = tar_gz(&[(server_executable_name(), b"released server")]);
    let sums = format!("{}  {name}\n", sha256_hex(&archive));
    let source = Fixture {
        release: Some(ReleaseInfo {
            tag_name: tag.to_owned(),
            assets: vec![
                ReleaseAsset {
                    name: "SHA256SUMS".to_owned(),
                    browser_download_url: "fixture://sums".to_owned(),
                },
                ReleaseAsset {
                    name: name.clone(),
                    browser_download_url: "fixture://archive".to_owned(),
                },
            ],
        }),
        downloads: BTreeMap::from([
            ("fixture://sums".to_owned(), sums.into_bytes()),
            ("fixture://archive".to_owned(), archive),
        ]),
    };

    let mut steps = Vec::new();
    let (binary, installed_tag) = download_latest(&source, &paths, &mut |step| {
        steps.push(step);
    })
    .unwrap();
    assert_eq!(installed_tag, tag);
    assert_eq!(binary, paths.binary(tag));
    assert_eq!(fs::read(&binary).unwrap(), b"released server");
    assert_eq!(
        steps,
        vec![InstallStep::Download, InstallStep::Install],
        "the install reports the work it did"
    );

    // A second run reuses the installed release without downloading again.
    let mut steps = Vec::new();
    let (binary, installed_tag) = download_latest(&source, &paths, &mut |step| {
        steps.push(step);
    })
    .unwrap();
    assert_eq!(installed_tag, tag);
    assert_eq!(binary, paths.binary(tag));
    assert!(steps.is_empty(), "{steps:?}");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_checksum_mismatch_refuses_the_install() {
    let root = temporary_state();
    let paths = LocalWorkerPaths::for_state_dir(&root);
    let tag = "v9.9.9";
    let platform = host_platform().expect("the test host has a release platform");
    let (name, _) = asset_name(tag, platform);
    let archive = tar_gz(&[(server_executable_name(), b"tampered server")]);
    let wrong = format!("{}  {name}\n", sha256_hex(b"something else"));
    let source = Fixture {
        release: Some(ReleaseInfo {
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
        }),
        downloads: BTreeMap::from([
            ("fixture://sums".to_owned(), wrong.into_bytes()),
            ("fixture://archive".to_owned(), archive),
        ]),
    };

    let error = download_latest(&source, &paths, &mut |_| {})
        .expect_err("a mismatched archive must be refused");
    assert_eq!(error.code, ErrorCode::MalformedPayload);
    assert!(!paths.binary(tag).exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_missing_release_is_reported_before_any_install() {
    let paths = LocalWorkerPaths::for_state_dir(temporary_state());
    let source = Fixture::default();
    // `host_platform` is compile-time; on a supported host the fixture instead
    // fails because it has no release, which still proves no install happened.
    let result = download_latest(&source, &paths, &mut |_| {});
    assert!(result.is_err());
    assert!(!paths.state.exists());
}

/// End to end against a real `loom-server` binary named by
/// `LOOM_TEST_SERVER_BINARY`; skipped otherwise, because this crate does not
/// build that binary.
#[test]
fn a_real_server_installs_starts_and_answers_health() {
    let Some(binary) = std::env::var_os("LOOM_TEST_SERVER_BINARY").map(PathBuf::from) else {
        return;
    };
    let root = temporary_state();
    let paths = LocalWorkerPaths::for_state_dir(&root);
    let port = crate::worker::free_loopback_port().unwrap();
    // A test must not install a user service, so force the detached path.
    // SAFETY: the variable is read by the worker this test starts alone.
    unsafe {
        std::env::set_var(NO_SYSTEMD_ENV, "1");
    }
    let options = LocalInstallOptions {
        explicit_binary: Some(binary),
        reset_state: true,
        port,
    };
    let worker = install_and_start_at(&Fixture::default(), &paths, &options, &mut |_| {})
        .expect("the worker installs and starts");
    assert!(worker_is_running(port));
    assert_eq!(worker.url, websocket_url(port));
    assert!(!worker.token.is_empty());
    assert!(!worker.supervised, "the detached path was forced");
    assert!(paths.token().is_file());

    stop_service(&paths);
    assert!(!worker_is_running(port));
    unsafe {
        std::env::remove_var(NO_SYSTEMD_ENV);
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn the_health_probe_reads_ok_from_a_real_loopback_response() {
    use std::io::Read as _;
    use std::net::TcpListener;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        }
    });
    assert_eq!(health(port).as_deref(), Some("ok"));
    assert!(worker_is_running(port));
    server.join().unwrap();

    let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    assert!(!worker_is_running(closed_port));
}
