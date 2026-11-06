//! Installing and running a worker on this machine.
//!
//! The native client used to start a `loom-server` child and stop it again on
//! quit. A worker installed here instead is a service that outlives the client:
//!
//! 1. the latest published `loom-server` release is downloaded, checked against
//!    its `SHA256SUMS`, and installed under `<state>/server/<tag>/`;
//! 2. `<state>/local-worker/token` (mode `0600`) is created when absent and
//!    read back;
//! 3. a systemd user unit starts the worker on `127.0.0.1:8765`, with linger
//!    enabled so it also starts at boot. A host without a user systemd gets a
//!    detached process with a pidfile instead, which does not survive a reboot;
//! 4. the install waits for `GET /health` before it reports success.
//!
//! The worker keeps its state in `<state>/state.db`, the database the client
//! used before workers ran as a service, so installing a worker does not lose
//! the sessions a user already has.

use std::{
    env, fs, io,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use loom_core::{ErrorCode, LoomError, Result};

use crate::worker::{
    ArchiveKind, GitHubReleases, ReleaseSource, ServerResolver, cache_root, client_release_tag,
    extract_server, host_platform, server_executable_name,
};

/// The loopback port the installed worker listens on.
pub const DEFAULT_PORT: u16 = 8765;
/// The systemd user unit the installed worker runs as.
pub const SERVICE_NAME: &str = "loom-server.service";
/// Environment variable that forces the detached-process mode, as it does for
/// a worker provisioned over SSH.
pub const NO_SYSTEMD_ENV: &str = "LOOM_NO_SYSTEMD";
/// How long a starting worker may take to answer `GET /health`.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a stopping worker may take to exit before the pidfile path kills it.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// The files of a worker installed on this machine.
#[derive(Clone, Debug)]
pub struct LocalWorkerPaths {
    /// Loom's state directory, which holds the server, token, and database.
    pub state: PathBuf,
}

impl LocalWorkerPaths {
    /// The worker belonging to this user and this state directory.
    pub fn for_state() -> Self {
        Self {
            state: loom_core::state_dir(),
        }
    }

    pub fn for_state_dir(state: impl Into<PathBuf>) -> Self {
        Self {
            state: state.into(),
        }
    }

    /// The bearer token the worker accepts, mode `0600`.
    pub fn token(&self) -> PathBuf {
        self.state.join("local-worker").join("token")
    }

    pub fn log(&self) -> PathBuf {
        self.state.join("local-worker").join("server.log")
    }

    pub fn pid(&self) -> PathBuf {
        self.state.join("local-worker").join("server.pid")
    }

    /// The database the client used before workers ran as a service, so
    /// existing sessions stay visible.
    pub fn persistence(&self) -> PathBuf {
        self.state.join("state.db")
    }

    /// The installed executable of release `tag`.
    pub fn binary(&self, tag: &str) -> PathBuf {
        self.state
            .join("server")
            .join(tag)
            .join(server_executable_name())
    }
}

/// A stage of the local install, reported as it starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallStep {
    Resolve,
    Download,
    Install,
    Token,
    Start,
    Health,
}

impl InstallStep {
    pub fn label(self) -> &'static str {
        match self {
            Self::Resolve => "Finding the latest loom-server release",
            Self::Download => "Downloading loom-server",
            Self::Install => "Installing loom-server",
            Self::Token => "Preparing the access token",
            Self::Start => "Starting the worker",
            Self::Health => "Waiting for the worker",
        }
    }
}

/// What to install and how to run it.
#[derive(Clone, Debug)]
pub struct LocalInstallOptions {
    /// A `loom-server` executable to run instead of downloading a release.
    pub explicit_binary: Option<PathBuf>,
    /// Delete the worker's database before starting it.
    pub reset_state: bool,
    pub port: u16,
}

impl Default for LocalInstallOptions {
    fn default() -> Self {
        Self {
            explicit_binary: None,
            reset_state: false,
            port: DEFAULT_PORT,
        }
    }
}

/// A worker installed and started on this machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledWorker {
    pub url: String,
    pub token: String,
    pub tag: String,
    /// `true` when a user service supervises it, so it also starts at boot.
    pub supervised: bool,
}

/// The WebSocket URL of a worker on `port`.
pub fn websocket_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}/ws")
}

/// Installs the latest released worker when needed and starts it.
pub fn install_and_start(
    source: &dyn ReleaseSource,
    options: &LocalInstallOptions,
    progress: &mut dyn FnMut(InstallStep),
) -> Result<InstalledWorker> {
    install_and_start_at(source, &LocalWorkerPaths::for_state(), options, progress)
}

/// [`install_and_start`] against an explicit state directory.
fn install_and_start_at(
    source: &dyn ReleaseSource,
    paths: &LocalWorkerPaths,
    options: &LocalInstallOptions,
    progress: &mut dyn FnMut(InstallStep),
) -> Result<InstalledWorker> {
    progress(InstallStep::Resolve);
    let (binary, tag, token) = match &options.explicit_binary {
        Some(binary) => {
            if !binary.is_file() {
                return Err(LoomError::invalid_request(format!(
                    "the loom-server executable '{}' does not exist",
                    binary.display()
                )));
            }
            let tag = client_release_tag().unwrap_or_else(|| "explicit".to_owned());
            progress(InstallStep::Token);
            let token = ensure_token(&paths.token())?;
            (binary.clone(), tag, token)
        }
        None => {
            let (binary, tag) = match bundled_server() {
                Some((source, tag)) => {
                    let binary = paths.binary(&tag);
                    progress(InstallStep::Install);
                    install_bundled(&source, &binary)?;
                    (binary, tag)
                }
                None => download_latest(source, paths, progress)?,
            };
            progress(InstallStep::Token);
            let token = ensure_token(&paths.token())?;
            (binary, tag, token)
        }
    };

    if options.reset_state {
        stop_service(paths);
        let database = paths.persistence();
        if database.exists() {
            fs::remove_file(&database).map_err(|error| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!(
                        "could not delete the worker database '{}': {error}",
                        database.display()
                    ),
                    false,
                )
            })?;
        }
    }

    progress(InstallStep::Start);
    let supervised = if worker_is_running(options.port) && running_this_version(&binary) {
        // The same release already serves this port: an install click is a
        // reconnect, not a reason to drop every connection for a restart.
        log::info!("local worker {tag} is already running");
        true
    } else {
        let supervised = start_service(paths, &binary, &tag, options.port)?;
        progress(InstallStep::Health);
        wait_until_healthy(paths, options.port, HEALTH_TIMEOUT)?;
        supervised
    };
    log::info!(
        "local worker {tag} is running on 127.0.0.1:{} ({})",
        options.port,
        if supervised {
            "user service"
        } else {
            "detached process"
        }
    );
    Ok(InstalledWorker {
        url: websocket_url(options.port),
        token,
        tag,
        supervised,
    })
}

/// Whether a worker already answers `/health` on `port`.
pub fn worker_is_running(port: u16) -> bool {
    health(port).is_some()
}

/// The token of an installed worker, when it can still be read.
pub fn installed_token() -> Option<String> {
    read_token(&LocalWorkerPaths::for_state().token())
}

/// Starts the worker already installed on this machine, when one is installed.
///
/// `None` means no installed executable was found, so a first run still shows
/// the setup screen; `Some` means the worker now answers `/health` with the
/// token the client will present.
pub fn ensure_running() -> Result<Option<InstalledWorker>> {
    let paths = LocalWorkerPaths::for_state();
    let Some(token) = read_token(&paths.token()) else {
        return Ok(None);
    };
    let Some((binary, tag)) = installed_binary(&paths) else {
        return Ok(None);
    };
    if worker_is_running(DEFAULT_PORT) {
        return Ok(Some(InstalledWorker {
            url: websocket_url(DEFAULT_PORT),
            token,
            tag,
            supervised: true,
        }));
    }
    let supervised = start_service(&paths, &binary, &tag, DEFAULT_PORT)?;
    wait_until_healthy(&paths, DEFAULT_PORT, HEALTH_TIMEOUT)?;
    Ok(Some(InstalledWorker {
        url: websocket_url(DEFAULT_PORT),
        token,
        tag,
        supervised,
    }))
}

/// The newest executable installed below `<state>/server`.
fn installed_binary(paths: &LocalWorkerPaths) -> Option<(PathBuf, String)> {
    let entries = fs::read_dir(paths.state.join("server")).ok()?;
    entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|tag| {
            let version = crate::worker::parse_tag(&tag)?;
            let binary = paths.binary(&tag);
            binary.is_file().then_some((version, tag, binary))
        })
        .max_by_key(|(version, _, _)| *version)
        .map(|(_, tag, binary)| (binary, tag))
}

/// Whether the running worker is this exact installed version, so an install
/// of the same release can connect instead of restarting it.
fn running_this_version(binary: &Path) -> bool {
    fs::read_to_string(systemd_user_dir().join(SERVICE_NAME))
        .map(|unit| unit.contains(&binary.display().to_string()))
        .unwrap_or(false)
}

/// Stops a worker installed on this machine, if one is running.
pub fn stop_installed_worker() {
    stop_service(&LocalWorkerPaths::for_state());
}

/// A `loom-server` shipped next to the client or on `PATH`.
///
/// Release archives, the AppImage, and the `.deb` install one beside the
/// client, so those installs keep working offline instead of downloading a
/// release the package already carries.
fn bundled_server() -> Option<(PathBuf, String)> {
    let name = server_executable_name();
    let mut directories = Vec::new();
    if let Some(directory) = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        directories.push(directory);
    }
    if let Some(path) = env::var_os("PATH") {
        directories.extend(env::split_paths(&path));
    }
    let source = directories
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())?;
    let tag = client_release_tag().unwrap_or_else(|| "bundled".to_owned());
    Some((source, tag))
}

/// Copies a bundled executable into the state directory, so the service runs
/// from a path that survives the client being replaced.
fn install_bundled(source: &Path, destination: &Path) -> Result<()> {
    if destination.is_file() && fs::read(destination).ok() == fs::read(source).ok() {
        return Ok(());
    }
    let parent = destination.parent().ok_or_else(|| {
        LoomError::invalid_request("the loom-server install path must have a parent directory")
    })?;
    fs::create_dir_all(parent).map_err(|error| install_io_error("create", parent, error))?;
    let staging = parent.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    fs::copy(source, &staging).map_err(|error| install_io_error("copy", &staging, error))?;
    make_server_executable(&staging)?;
    fs::rename(&staging, destination)
        .map_err(|error| install_io_error("install", destination, error))?;
    log::info!(
        "installed the bundled loom-server from '{}' at '{}'",
        source.display(),
        destination.display()
    );
    Ok(())
}

fn make_server_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(|error| install_io_error("mark executable", path, error))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn download_latest(
    source: &dyn ReleaseSource,
    paths: &LocalWorkerPaths,
    progress: &mut dyn FnMut(InstallStep),
) -> Result<(PathBuf, String)> {
    let platform = host_platform().ok_or_else(|| {
        LoomError::new(
            ErrorCode::UnsupportedCapability,
            format!(
                "no loom-server release is built for {}-{}",
                env::consts::OS,
                env::consts::ARCH
            ),
            false,
        )
    })?;
    let resolver = ServerResolver {
        explicit: None,
        search_dirs: Vec::new(),
        cache_root: cache_root(),
        // The wizard installs the latest published worker, whatever version
        // this client was built as.
        preferred_tag: None,
        platform: Some(platform),
        source,
    };
    let release = resolver.release()?;
    let tag = release.tag_name.clone();
    let binary = paths.binary(&tag);
    if binary.is_file() {
        log::info!(
            "loom-server {tag} is already installed at '{}'",
            binary.display()
        );
        return Ok((binary, tag));
    }
    progress(InstallStep::Download);
    let (archive, kind) = resolver.fetch_verified(&release, platform)?;
    progress(InstallStep::Install);
    install_verified(&binary, &archive, kind)?;
    Ok((binary, tag))
}

/// Extracts a verified archive into place, so the installed binary is either
/// complete or absent.
fn install_verified(destination: &Path, archive: &[u8], kind: ArchiveKind) -> Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        LoomError::invalid_request("the loom-server install path must have a parent directory")
    })?;
    fs::create_dir_all(parent).map_err(|error| install_io_error("create", parent, error))?;
    let staging = parent.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|error| install_io_error("create", &staging, error))?;
    let staged = staging.join(server_executable_name());
    let result = extract_server(archive, kind, &staged).and_then(|()| {
        fs::rename(&staged, destination)
            .map_err(|error| install_io_error("install", destination, error))
    });
    let _ = fs::remove_dir_all(&staging);
    result?;
    log::info!("installed loom-server at '{}'", destination.display());
    Ok(())
}

fn install_io_error(context: &str, path: &Path, error: io::Error) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("could not {context} '{}': {error}", path.display()),
        false,
    )
}

fn read_token(path: &Path) -> Option<String> {
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim().to_owned();
    (!token.is_empty()).then_some(token)
}

/// The worker's token, created owner-only the first time.
fn ensure_token(path: &Path) -> Result<String> {
    if let Some(token) = read_token(path) {
        return Ok(token);
    }
    if let Some(parent) = path.parent() {
        loom_core::create_private_dir(parent)
            .map_err(|error| install_io_error("create", parent, error))?;
    }
    let token = format!("loom-{}", uuid::Uuid::new_v4().simple());
    write_private_file(path, &token)?;
    Ok(token)
}

fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| install_io_error("write", path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| install_io_error("restrict", path, error))?;
    }
    file.write_all(contents.as_bytes())
        .map_err(|error| install_io_error("write", path, error))
}

/// `~/.../systemd/user`, where the user unit lives.
fn systemd_user_dir() -> PathBuf {
    if let Some(config) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(config).join("systemd").join("user");
    }
    env::var_os("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("systemd")
                .join("user")
        })
        .unwrap_or_else(|| PathBuf::from(".config").join("systemd").join("user"))
}

/// The unit that runs `binary` on loopback under a user systemd.
fn unit_contents(binary: &Path, token: &Path, persistence: &Path, port: u16) -> String {
    format!(
        "[Unit]\n\
         Description=Loom worker\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={} --bind 127.0.0.1:{port} --token-file {} --persistence {}\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote_unit_argument(binary),
        quote_unit_argument(token),
        quote_unit_argument(persistence)
    )
}

fn quote_unit_argument(path: &Path) -> String {
    let text = path.display().to_string();
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn user_systemd_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Starts the worker as a user service when the host has one, otherwise as a
/// detached process. The result says which one supervised it.
fn start_service(paths: &LocalWorkerPaths, binary: &Path, tag: &str, port: u16) -> Result<bool> {
    if env::var_os(NO_SYSTEMD_ENV).is_none() && user_systemd_available() {
        let unit = systemd_user_dir().join(SERVICE_NAME);
        write_unit(&unit, binary, &paths.token(), &paths.persistence(), port)?;
        run_systemctl(&["daemon-reload"])?;
        run_systemctl(&["enable", SERVICE_NAME])?;
        // Linger is what lets the unit start at boot instead of at login. It
        // can be refused by policy, so a failure is reported but not fatal.
        if !linger_enabled() {
            log::warn!(
                "could not enable lingering for this user; the worker starts at login, not at boot"
            );
        }
        run_systemctl(&["restart", SERVICE_NAME]).or_else(|restart_error| {
            // A restart that stopped the unit but could not start it is worse
            // than an older worker: try a plain start before reporting.
            run_systemctl(&["start", SERVICE_NAME]).map_err(|_| restart_error)
        })?;
        log::info!("local worker {tag} runs as {SERVICE_NAME}");
        return Ok(true);
    }
    start_detached(paths, binary, tag, port)?;
    Ok(false)
}

fn write_unit(
    unit: &Path,
    binary: &Path,
    token: &Path,
    persistence: &Path,
    port: u16,
) -> Result<()> {
    if let Some(parent) = unit.parent() {
        fs::create_dir_all(parent).map_err(|error| install_io_error("create", parent, error))?;
    }
    let contents = unit_contents(binary, token, persistence, port);
    if fs::read_to_string(unit).ok().as_deref() == Some(contents.as_str()) {
        return Ok(());
    }
    fs::write(unit, contents).map_err(|error| install_io_error("write", unit, error))
}

fn run_systemctl(arguments: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("could not run systemctl: {error}"),
                false,
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(LoomError::new(
        ErrorCode::ProviderUnavailable,
        format!(
            "systemctl --user {} failed: {}",
            arguments.join(" "),
            stderr.trim()
        ),
        false,
    ))
}

fn linger_enabled() -> bool {
    let user = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_default();
    if user.trim().is_empty() {
        return false;
    }
    let enabled = Command::new("loginctl")
        .args(["enable-linger", &user])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !enabled {
        return false;
    }
    Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .stdin(Stdio::null())
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "yes")
        .unwrap_or(false)
}

/// Starts the worker as a process that is not a child of the client.
fn start_detached(paths: &LocalWorkerPaths, binary: &Path, tag: &str, port: u16) -> Result<()> {
    kill_pidfile(&paths.pid());
    if let Some(parent) = paths.log().parent() {
        loom_core::create_private_dir(parent)
            .map_err(|error| install_io_error("create", parent, error))?;
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log())
        .map_err(|error| install_io_error("open", &paths.log(), error))?;
    let errors = log
        .try_clone()
        .map_err(|error| install_io_error("open", &paths.log(), error))?;
    let mut command = Command::new(binary);
    command
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--token-file")
        .arg(paths.token())
        .arg("--persistence")
        .arg(paths.persistence())
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(errors);
    detach_from_client(&mut command);
    let child = command.spawn().map_err(|error| {
        LoomError::new(
            ErrorCode::ProviderUnavailable,
            format!(
                "could not start the loom-server worker '{}': {error}",
                binary.display()
            ),
            false,
        )
    })?;
    let pid = child.id();
    fs::write(paths.pid(), pid.to_string())
        .map_err(|error| install_io_error("write", &paths.pid(), error))?;
    log::info!("local worker {tag} runs as detached process {pid}");
    Ok(())
}

#[cfg(unix)]
fn detach_from_client(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure only calls the async-signal-safe `setsid`, so the
    // worker leaves the client's session and survives it.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_from_client(_: &mut Command) {}

/// Stops a worker this module started.
fn stop_service(paths: &LocalWorkerPaths) {
    if user_systemd_available() && systemd_user_dir().join(SERVICE_NAME).is_file() {
        let _ = run_systemctl(&["stop", SERVICE_NAME]);
    }
    kill_pidfile(&paths.pid());
}

fn kill_pidfile(path: &Path) {
    let Ok(pid) = fs::read_to_string(path) else {
        return;
    };
    let Ok(pid) = pid.trim().parse::<i32>() else {
        let _ = fs::remove_file(path);
        return;
    };
    #[cfg(unix)]
    {
        // SAFETY: `kill` only signals the pid this module recorded; a recycled
        // pid could in theory be signalled, which the pidfile makes unlikely.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            // Signal 0 only checks that the process still exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if unsafe { libc::kill(pid, 0) } == 0 {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = fs::remove_file(path);
}

fn wait_until_healthy(paths: &LocalWorkerPaths, port: u16, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if worker_is_running(port) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(LoomError::new(
                ErrorCode::DeadlineExceeded,
                format!(
                    "the local worker did not answer /health on 127.0.0.1:{port} within {}s; \
                     see '{}' or `journalctl --user -u {SERVICE_NAME}`",
                    timeout.as_secs(),
                    paths.log().display()
                ),
                true,
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// The body of `GET /health`, or `None` when nothing answers.
fn health(port: u16) -> Option<String> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    stream
        .write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .ok()?;
    let mut response = Vec::new();
    let _ = stream.take(4096).read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response);
    let (head, body) = response.split_once("\r\n\r\n")?;
    let status = head.lines().next()?.split_whitespace().nth(1)?;
    (status == "200").then(|| body.trim().to_owned())
}

/// The GitHub-backed installer the native client uses.
pub fn install_latest(progress: &mut dyn FnMut(InstallStep)) -> Result<InstalledWorker> {
    let source = GitHubReleases::default();
    install_and_start(&source, &LocalInstallOptions::default(), progress)
}

#[cfg(test)]
mod tests;
