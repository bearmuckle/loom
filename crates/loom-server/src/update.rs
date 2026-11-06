//! Self-update for a worker installed from a Loom release.
//!
//! Only a worker running from a Loom-managed install layout updates itself:
//! `<state>/server/<tag>/loom-server`, the layout both the desktop installer and
//! the SSH provisioner produce. A standalone deployment (a container, or a
//! server started from an arbitrary path) is updated by its own channel and
//! never replaces its own executable.
//!
//! A check runs periodically and considers only stable releases newer than the
//! running one. The release archive is verified against its `SHA256SUMS` and
//! installed beside the running version. The switch is applied when no agent
//! run is active: the systemd user unit is pointed at the new executable and
//! restarted, or a detached worker starts the new executable once the old
//! process has exited.
//!
//! Opt out with `--no-auto-update` or by setting `LOOM_NO_AUTO_UPDATE`.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use loom_core::{ErrorCode, LoomError, Result};
use loom_release::{
    GitHubReleases, ReleaseSource, extract_server, host_platform, is_newer_stable,
    parse_release_tag, server_executable_name,
};

use crate::InProcessBackend;

/// The systemd user unit an installed worker runs as.
pub const SERVICE_NAME: &str = "loom-server.service";
/// Setting this variable to any non-empty value disables self-update.
pub const NO_AUTO_UPDATE_ENV: &str = "LOOM_NO_AUTO_UPDATE";
/// Seconds between update checks (`LOOM_UPDATE_INTERVAL_SECS`).
pub const UPDATE_INTERVAL_ENV: &str = "LOOM_UPDATE_INTERVAL_SECS";
/// Seconds before the first update check (`LOOM_UPDATE_INITIAL_SECS`).
pub const UPDATE_INITIAL_ENV: &str = "LOOM_UPDATE_INITIAL_SECS";

const DEFAULT_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const DEFAULT_INITIAL_DELAY: Duration = Duration::from_secs(5 * 60);
/// A floor so a misconfigured interval cannot hammer the release API.
const MINIMUM_INTERVAL: Duration = Duration::from_secs(60);
const IDLE_POLL: Duration = Duration::from_secs(10);
/// Consecutive idle samples required before a restart, so a run that starts
/// just as the worker becomes idle is not interrupted.
const IDLE_SAMPLES: u8 = 3;
const PID_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The cadence of the update check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateOptions {
    pub interval: Duration,
    pub initial_delay: Duration,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            initial_delay: DEFAULT_INITIAL_DELAY,
        }
    }
}

impl UpdateOptions {
    /// The options from the environment, clamped to a usable range.
    pub fn from_environment() -> Self {
        let defaults = Self::default();
        let interval = env_seconds(UPDATE_INTERVAL_ENV)
            .unwrap_or(defaults.interval)
            .max(MINIMUM_INTERVAL);
        let initial_delay = env_seconds(UPDATE_INITIAL_ENV).unwrap_or(defaults.initial_delay);
        Self {
            interval,
            initial_delay,
        }
    }
}

fn env_seconds(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Where this worker is installed: `<state>/server/<tag>/loom-server`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallLayout {
    pub state: PathBuf,
    pub tag: String,
}

impl InstallLayout {
    /// The layout of the running executable, or `None` when it is not a Loom
    /// install (a development build, a container, an arbitrary path).
    pub fn detect() -> Option<Self> {
        std::env::current_exe()
            .ok()
            .and_then(|exe| Self::from_exe(&exe))
    }

    pub fn from_exe(exe: &Path) -> Option<Self> {
        let name = exe.file_name()?.to_str()?;
        if name != server_executable_name() {
            return None;
        }
        let tag_dir = exe.parent()?;
        let tag = tag_dir.file_name()?.to_str()?;
        parse_release_tag(tag)?;
        let server_dir = tag_dir.parent()?;
        if server_dir.file_name()?.to_str()? != "server" {
            return None;
        }
        Some(Self {
            state: server_dir.parent()?.to_path_buf(),
            tag: tag.to_owned(),
        })
    }

    pub fn binary(&self, tag: &str) -> PathBuf {
        self.state
            .join("server")
            .join(tag)
            .join(server_executable_name())
    }
}

/// The release this binary was built from, or the workspace version for a
/// development build.
pub fn running_version() -> &'static str {
    option_env!("LOOM_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Starts the background update check when this process can update itself.
///
/// `enabled` carries the `--no-auto-update` flag; the environment variable is
/// checked here so a deployment can disable it without changing its unit.
pub fn spawn(
    backend: Arc<InProcessBackend>,
    enabled: bool,
    options: UpdateOptions,
) -> Option<thread::JoinHandle<()>> {
    if !enabled || std::env::var_os(NO_AUTO_UPDATE_ENV).is_some_and(|value| !value.is_empty()) {
        log::debug!("auto-update is disabled");
        return None;
    }
    let Some(layout) = InstallLayout::detect() else {
        log::debug!("not running from a Loom install layout; auto-update is off");
        return None;
    };
    let running = running_version();
    if parse_release_tag(running).is_none() {
        log::debug!("development build {running}; auto-update is off");
        return None;
    }
    let running = running.to_owned();
    match thread::Builder::new()
        .name("loom-update".to_owned())
        .spawn(move || run(backend, layout, running, options))
    {
        Ok(handle) => Some(handle),
        Err(error) => {
            log::warn!("could not start the auto-update thread: {error}");
            None
        }
    }
}

fn run(
    backend: Arc<InProcessBackend>,
    layout: InstallLayout,
    running: String,
    options: UpdateOptions,
) {
    thread::sleep(options.initial_delay);
    loop {
        match check_and_install(&GitHubReleases::default(), &layout, &running) {
            Ok(Some(tag)) => {
                log::info!(
                    "loom-server {tag} was installed; waiting for active runs to finish before \
                     restarting"
                );
                wait_for_idle(&backend);
                log::info!("restarting into loom-server {tag}");
                if let Err(error) = apply(&layout, &tag) {
                    log::error!(
                        "could not restart into loom-server {tag}: {}; it will be used on the \
                         next worker start",
                        error.message
                    );
                }
                return;
            }
            Ok(None) => log::debug!("loom-server {running} is up to date"),
            Err(error) => log::warn!("auto-update check failed: {}", error.message),
        }
        thread::sleep(options.interval);
    }
}

/// Downloads, verifies, and installs the latest stable release when it is newer
/// than `running`. Returns the installed tag.
pub fn check_and_install(
    source: &dyn ReleaseSource,
    layout: &InstallLayout,
    running: &str,
) -> Result<Option<String>> {
    let Some(release) = source.release(None)? else {
        return Ok(None);
    };
    let latest = release.tag_name.as_str();
    if !is_newer_stable(running, latest) {
        return Ok(None);
    }
    let target = layout.binary(latest);
    if target.is_file() {
        log::info!(
            "loom-server {latest} is already installed at '{}'",
            target.display()
        );
        return Ok(Some(latest.to_owned()));
    }
    let platform = host_platform().ok_or_else(|| {
        LoomError::new(
            ErrorCode::UnsupportedCapability,
            format!(
                "no loom-server release is built for {}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            false,
        )
    })?;
    let (archive, kind) = loom_release::fetch_verified(source, &release, platform)?;
    install_verified(&target, &archive, kind)?;
    Ok(Some(latest.to_owned()))
}

fn install_verified(
    destination: &Path,
    archive: &[u8],
    kind: loom_release::ArchiveKind,
) -> Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        LoomError::invalid_request("the loom-server install path must have a parent directory")
    })?;
    fs::create_dir_all(parent).map_err(|error| io_error("create", parent, error))?;
    let staging = parent.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|error| io_error("create", &staging, error))?;
    let staged = staging.join(server_executable_name());
    let result = extract_server(archive, kind, &staged).and_then(|()| {
        fs::rename(&staged, destination).map_err(|error| io_error("install", destination, error))
    });
    let _ = fs::remove_dir_all(&staging);
    result?;
    log::info!("installed loom-server at '{}'", destination.display());
    Ok(())
}

fn io_error(context: &str, path: &Path, error: io::Error) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("could not {context} '{}': {error}", path.display()),
        false,
    )
}

/// Blocks until no run has been queued or executing for three samples.
fn wait_for_idle(backend: &InProcessBackend) {
    let mut quiet = 0u8;
    let mut waiting = false;
    loop {
        let active = backend.active_runs();
        if active == 0 {
            quiet += 1;
            if quiet >= IDLE_SAMPLES {
                if waiting {
                    log::info!("the worker is idle; applying the update");
                }
                return;
            }
        } else {
            if !waiting {
                log::info!("waiting for {active} active run(s) before restarting");
                waiting = true;
            }
            quiet = 0;
        }
        thread::sleep(IDLE_POLL);
    }
}

/// Switches the worker to the installed `new_tag`.
fn apply(layout: &InstallLayout, new_tag: &str) -> Result<()> {
    let binary = layout.binary(new_tag);
    if !binary.is_file() {
        return Err(LoomError::new(
            ErrorCode::NotFound,
            format!("the installed loom-server {new_tag} is missing"),
            false,
        ));
    }
    if std::env::var_os("INVOCATION_ID").is_some() {
        return apply_unit(&layout.tag, new_tag);
    }
    restart_detached(layout, &binary)
}

fn systemd_user_dir() -> PathBuf {
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(config).join("systemd").join("user");
    }
    std::env::var_os("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("systemd")
                .join("user")
        })
        .unwrap_or_else(|| PathBuf::from(".config").join("systemd").join("user"))
}

/// Points the user unit at the new tag and restarts it. The restart sends the
/// running worker SIGTERM, which it handles by shutting down cleanly.
fn apply_unit(old_tag: &str, new_tag: &str) -> Result<()> {
    let unit = systemd_user_dir().join(SERVICE_NAME);
    let text = fs::read_to_string(&unit).map_err(|error| {
        LoomError::new(
            ErrorCode::NotFound,
            format!(
                "could not read the worker unit '{}': {error}; reinstall the worker",
                unit.display()
            ),
            false,
        )
    })?;
    let Some(updated) = rewritten_unit_text(&text, old_tag, new_tag) else {
        return Err(LoomError::new(
            ErrorCode::Internal,
            format!(
                "the worker unit '{}' does not reference loom-server {old_tag}; reinstall the \
                 worker",
                unit.display()
            ),
            false,
        ));
    };
    fs::write(&unit, updated).map_err(|error| io_error("write", &unit, error))?;
    run_systemctl(&["daemon-reload"])?;
    // The old process is stopped as part of the restart; `--no-block` keeps
    // this thread from waiting on the unit that contains it.
    run_systemctl(&["restart", "--no-block", SERVICE_NAME])
}

/// Replaces the `server/<old>/loom-server` suffix in a unit. Both the installer's
/// absolute `ExecStart` and the SSH provisioner's `%h` form contain it, so one
/// rule updates either.
fn rewritten_unit_text(text: &str, old_tag: &str, new_tag: &str) -> Option<String> {
    let name = server_executable_name();
    let from = format!("/server/{old_tag}/{name}");
    let to = format!("/server/{new_tag}/{name}");
    if !text.contains(&from) {
        return None;
    }
    Some(text.replace(&from, &to))
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

/// Starts the new executable once this process has exited, when no systemd
/// supervises the worker.
fn restart_detached(layout: &InstallLayout, binary: &Path) -> Result<()> {
    let mut args = detached_args();
    args.push(OsString::from("--restart-after"));
    args.push(OsString::from(std::process::id().to_string()));

    let log = detached_log(layout);
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error("create", parent, error))?;
    }
    let output = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|error| io_error("open", &log, error))?;
    let errors = output
        .try_clone()
        .map_err(|error| io_error("open", &log, error))?;
    let mut command = Command::new(binary);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(output)
        .stderr(errors);
    detach_from_parent(&mut command);
    let child = command.spawn().map_err(|error| {
        LoomError::new(
            ErrorCode::ProviderUnavailable,
            format!(
                "could not start the updated loom-server '{}': {error}",
                binary.display()
            ),
            false,
        )
    })?;
    let pidfile = detached_pidfile(layout);
    if let Some(parent) = pidfile.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&pidfile, child.id().to_string())
        .map_err(|error| io_error("write", &pidfile, error))?;
    log::info!(
        "started loom-server {} as process {}; stopping this one",
        binary.display(),
        child.id()
    );
    #[cfg(unix)]
    {
        // SAFETY: the pid is this process, so the signal cannot be misdirected.
        unsafe {
            libc::kill(std::process::id() as i32, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    std::process::exit(0);
    Ok(())
}

/// The arguments this process was started with, without a previous
/// `--restart-after` so nested updates cannot accumulate it.
fn detached_args() -> Vec<OsString> {
    let mut args = Vec::new();
    let mut iter = std::env::args_os().skip(1);
    while let Some(argument) = iter.next() {
        if argument == "--restart-after" {
            let _ = iter.next();
            continue;
        }
        if argument.to_string_lossy().starts_with("--restart-after=") {
            continue;
        }
        args.push(argument);
    }
    args
}

fn detached_log(layout: &InstallLayout) -> PathBuf {
    let local = layout.state.join("local-worker");
    if local.is_dir() {
        local.join("server.log")
    } else {
        layout.state.join("server.log")
    }
}

fn detached_pidfile(layout: &InstallLayout) -> PathBuf {
    let local = layout.state.join("local-worker");
    if local.is_dir() {
        local.join("server.pid")
    } else {
        layout.state.join("server.pid")
    }
}

#[cfg(unix)]
fn detach_from_parent(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure only calls the async-signal-safe `setsid`, so the
    // new worker leaves this process's session.
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
fn detach_from_parent(_: &mut Command) {}

/// Waits until `pid` exits, bounded so a recycled or stuck pid cannot hold
/// startup forever.
pub fn wait_for_pid_exit(pid: u32) {
    let deadline = Instant::now() + PID_WAIT_TIMEOUT;
    while process_exists(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    // SAFETY: signal 0 only checks whether the process exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn process_exists(_: u32) -> bool {
    false
}

#[cfg(test)]
mod tests;
