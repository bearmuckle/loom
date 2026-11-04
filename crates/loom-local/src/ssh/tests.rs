use std::{cell::RefCell, collections::BTreeMap, fs, io::Write, process::Command, sync::Mutex};

use super::*;
use crate::worker::{ReleaseAsset, ReleaseInfo, asset_name, sha256_hex};

/// Serializes the tests that change `LOOM_SSH_BINARY`.
static SSH_BINARY_LOCK: Mutex<()> = Mutex::new(());

fn target() -> SshTarget {
    SshTarget::parse("ssh://dev@box.example:2222").unwrap()
}

#[test]
fn parses_ssh_urls_and_bare_destinations() {
    let parsed = target();
    assert_eq!(parsed.destination, "dev@box.example");
    assert_eq!(parsed.port, Some(2222));
    assert_eq!(parsed.remote_port, DEFAULT_REMOTE_PORT);
    assert_eq!(parsed.display(), "ssh://dev@box.example:2222");

    let bare = SshTarget::parse(" build-box ").unwrap();
    assert_eq!(bare.destination, "build-box");
    assert_eq!(bare.port, None);
    assert_eq!(bare.display(), "ssh://build-box");
    assert_eq!(SshTarget::parse("ssh://u@h/").unwrap().destination, "u@h");
}

#[test]
fn rejects_destinations_that_could_inject_options_or_arguments() {
    for bad in [
        "",
        "-oProxyCommand=evil",
        "host name",
        "host;rm",
        "host$(x)",
        "ssh://",
        "ssh://host:notaport",
        "ssh://host:99999",
    ] {
        assert!(SshTarget::parse(bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn builds_base_arguments_from_the_target() {
    let mut target = target();
    target.identity_file = Some("/keys/id".into());
    target.jump_host = Some("jump".to_owned());
    let args = target.base_args().join(" ");
    assert!(args.contains("BatchMode=yes"));
    assert!(args.contains("-p 2222"));
    assert!(args.contains("-i /keys/id"));
    assert!(args.contains("-J jump"));
}

#[test]
fn quotes_for_a_posix_shell() {
    assert_eq!(shell_quote("a b"), "'a b'");
    assert_eq!(shell_quote("it's"), "'it'\\''s'");
}

#[test]
fn maps_uname_output_to_release_platforms() {
    assert_eq!(parse_uname("Linux x86_64\n").unwrap(), "linux-x86_64");
    assert_eq!(parse_uname("Linux aarch64").unwrap(), "linux-aarch64");
    assert_eq!(parse_uname("Darwin arm64").unwrap(), "macos-universal");
    assert_eq!(
        parse_uname("FreeBSD amd64").unwrap_err().code,
        ErrorCode::UnsupportedCapability
    );
    assert_eq!(
        parse_uname("Linux riscv64").unwrap_err().code,
        ErrorCode::UnsupportedCapability
    );
}

#[test]
fn failures_say_how_to_fix_the_common_causes() {
    let target = target();
    let run = |stderr: &str| {
        failure(
            "Probing the host",
            &target,
            &SshOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.to_owned(),
            },
        )
        .message
    };
    assert!(run("Host key verification failed.").contains("Run `ssh dev@box.example` once"));
    assert!(run("Permission denied (publickey).").contains("ssh-agent"));
    assert!(run("ssh: Could not resolve hostname x").contains("not reachable"));
    let plain = run("boom");
    assert!(plain.contains("failed: boom"), "{plain}");
    let empty = failure("Step", &target, &SshOutput::default()).message;
    assert!(empty.starts_with("Step on dev@box.example failed"));
}

#[test]
fn the_generated_scripts_are_valid_shell() {
    for script in [
        start_script("v1.2.3", 8765),
        install_script("v1.2.3"),
        install_check_script("v1.2.3"),
        wait_healthy_script(8765),
        TOKEN_SCRIPT.to_owned(),
    ] {
        let mut child = Command::new("sh")
            .arg("-n")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{script}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn the_systemd_unit_names_the_installed_binary_and_port() {
    let script = start_script("v1.2.3", 9100);
    assert!(script.contains("ExecStart=%h/.loom/server/v1.2.3/loom-server --bind 127.0.0.1:9100"));
    assert!(script.contains("--token-file %h/.loom/token"));
}

#[test]
fn every_step_has_a_label() {
    for step in [
        ProvisionStep::Probe,
        ProvisionStep::Download,
        ProvisionStep::Upload,
        ProvisionStep::Token,
        ProvisionStep::Start,
        ProvisionStep::Health,
    ] {
        assert!(!step.label().is_empty());
    }
}

/// A scratch directory removed when it goes out of scope.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = env::temp_dir().join(format!("loom-ssh-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs the "remote" command on this machine with a private `HOME`.
struct LocalRunner {
    home: std::path::PathBuf,
}

impl SshRunner for LocalRunner {
    fn run(&self, _: &SshTarget, command: &str, stdin: &[u8]) -> Result<SshOutput> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .env("HOME", &self.home)
            .env("LOOM_NO_SYSTEMD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let output = child.wait_with_output().unwrap();
        Ok(SshOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// A release with one archive for the host platform.
struct OneRelease {
    release: ReleaseInfo,
    files: BTreeMap<String, Vec<u8>>,
    downloads: RefCell<usize>,
}

impl ReleaseSource for OneRelease {
    fn release(&self, _: Option<&str>) -> Result<Option<ReleaseInfo>> {
        Ok(Some(self.release.clone()))
    }

    fn download(&self, url: &str) -> Result<Vec<u8>> {
        *self.downloads.borrow_mut() += 1;
        Ok(self.files[url].clone())
    }
}

/// A `loom-server` stand-in that answers `GET /health` on the `--bind` port.
const FAKE_SERVER: &[u8] = b"#!/bin/sh\n\
    dir=\"$(dirname \"$0\")/www\"\n\
    mkdir -p \"$dir\"\n\
    : > \"$dir/health\"\n\
    port=\"${2##*:}\"\n\
    exec python3 -m http.server \"$port\" --bind 127.0.0.1 --directory \"$dir\"\n";

fn release_for(platform: &str) -> OneRelease {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(FAKE_SERVER.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, "loom-server", FAKE_SERVER)
        .unwrap();
    let archive = builder.into_inner().unwrap().finish().unwrap();
    let (name, _) = asset_name("v9.9.9", platform);
    let sums = format!("{}  {name}\n", sha256_hex(&archive));
    let mut files = BTreeMap::new();
    files.insert("fixture://archive".to_owned(), archive);
    files.insert("fixture://sums".to_owned(), sums.into_bytes());
    OneRelease {
        release: ReleaseInfo {
            tag_name: "v9.9.9".to_owned(),
            assets: vec![
                ReleaseAsset {
                    name,
                    browser_download_url: "fixture://archive".to_owned(),
                },
                ReleaseAsset {
                    name: "SHA256SUMS".to_owned(),
                    browser_download_url: "fixture://sums".to_owned(),
                },
            ],
        },
        files,
        downloads: RefCell::new(0),
    }
}

fn host_uname_platform() -> Option<&'static str> {
    let output = Command::new("uname").arg("-sm").output().ok()?;
    parse_uname(&String::from_utf8_lossy(&output.stdout)).ok()
}

fn stop_fake_worker(home: &std::path::Path) {
    if let Ok(pid) = fs::read_to_string(home.join(".loom/server.pid")) {
        let _ = Command::new("kill").arg(pid.trim()).status();
    }
}

#[test]
fn provisions_upgrades_and_removes_a_worker() {
    let Some(platform) = host_uname_platform() else {
        return;
    };
    if Command::new("python3").arg("--version").output().is_err()
        || Command::new("curl").arg("--version").output().is_err()
    {
        return;
    }
    let home = TempDir::new();
    let runner = LocalRunner {
        home: home.path().to_owned(),
    };
    let source = release_for(platform);
    let mut target = SshTarget::parse("dev@box").unwrap();
    target.remote_port = crate::worker::free_loopback_port().unwrap();

    let mut steps = Vec::new();
    let worker = provision(&runner, &source, &target, &mut |step| steps.push(step)).unwrap();
    assert_eq!(worker.tag, "v9.9.9");
    assert!(worker.token.starts_with("loom-"));
    assert!(!worker.supervised);
    assert_eq!(
        steps,
        [
            ProvisionStep::Probe,
            ProvisionStep::Download,
            ProvisionStep::Upload,
            ProvisionStep::Token,
            ProvisionStep::Start,
            ProvisionStep::Health,
        ]
    );
    assert_eq!(*source.downloads.borrow(), 2);
    let token_file = home.path().join(".loom/token");
    assert_eq!(fs::read_to_string(&token_file).unwrap(), worker.token);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&token_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the token must be owner-only");
    }
    let pid = fs::read_to_string(home.path().join(".loom/server.pid")).unwrap();

    // A second run finds this version running: no download, no restart, and
    // the same token.
    let mut again = Vec::new();
    let second = provision(&runner, &source, &target, &mut |step| again.push(step)).unwrap();
    assert_eq!(second.token, worker.token);
    assert!(!again.contains(&ProvisionStep::Upload));
    assert_eq!(*source.downloads.borrow(), 2);
    assert_eq!(
        fs::read_to_string(home.path().join(".loom/server.pid")).unwrap(),
        pid
    );

    remove(&runner, &target, false).unwrap();
    assert!(!home.path().join(".loom/server").exists());
    assert!(token_file.exists());
    remove(&runner, &target, true).unwrap();
    assert!(!home.path().join(".loom").exists());
    stop_fake_worker(home.path());
}

#[test]
fn a_failed_probe_surfaces_the_remote_error() {
    struct Failing;
    impl SshRunner for Failing {
        fn run(&self, _: &SshTarget, _: &str, _: &[u8]) -> Result<SshOutput> {
            Ok(SshOutput {
                success: false,
                stdout: String::new(),
                stderr: "Host key verification failed.".to_owned(),
            })
        }
    }
    let source = release_for("linux-x86_64");
    let error = provision(&Failing, &source, &target(), &mut |_| {}).unwrap_err();
    assert!(
        error.message.contains("Probing the host"),
        "{}",
        error.message
    );
    assert!(error.message.contains("host key"), "{}", error.message);
    assert_eq!(*source.downloads.borrow(), 0);
}

#[test]
fn the_tunnel_forwards_a_port_and_reports_ssh_failures() {
    let _guard = SSH_BINARY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let dir = TempDir::new();
    let shim = dir.path().join("ssh-shim");
    // Listens on the local end of the `-L` forward, like a live tunnel.
    fs::write(
        &shim,
        "#!/bin/sh\n\
         while [ $# -gt 0 ]; do\n\
           if [ \"$1\" = -L ]; then spec=\"$2\"; fi\n\
           shift\n\
         done\n\
         port=\"$(echo \"$spec\" | cut -d: -f2)\"\n\
         exec python3 -c \"import socket,sys; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(('127.0.0.1', int(sys.argv[1]))); s.listen(5)\n\
while True: s.accept()\" \"$port\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // SAFETY: the lock above serializes every test that touches this variable.
    unsafe { env::set_var(SSH_BINARY_ENV, &shim) };
    let mut tunnel = open_tunnel(&target(), None).unwrap();
    assert!(tunnel.is_alive());
    assert_eq!(
        tunnel.websocket_url(),
        format!("ws://127.0.0.1:{}/ws", tunnel.local_port())
    );
    drop(tunnel);

    let failing = dir.path().join("ssh-fail");
    fs::write(
        &failing,
        "#!/bin/sh\necho 'Permission denied (publickey).' >&2\nexit 255\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&failing, fs::Permissions::from_mode(0o755)).unwrap();
    }
    unsafe { env::set_var(SSH_BINARY_ENV, &failing) };
    let error = match open_tunnel(&target(), None) {
        Ok(_) => panic!("a failing ssh must not open a tunnel"),
        Err(error) => error,
    };
    assert!(error.message.contains("ssh-agent"), "{}", error.message);
    unsafe { env::remove_var(SSH_BINARY_ENV) };
}

#[test]
fn the_system_runner_reports_a_missing_ssh_binary() {
    let _guard = SSH_BINARY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: serialized by the lock above.
    unsafe { env::set_var(SSH_BINARY_ENV, "/nonexistent/ssh") };
    let error = SystemSsh.run(&target(), "true", &[]).unwrap_err();
    assert!(error.message.contains("Install an OpenSSH client"));
    unsafe { env::remove_var(SSH_BINARY_ENV) };
}
