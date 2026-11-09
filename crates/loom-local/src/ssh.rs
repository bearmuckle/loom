//! Provisioning a worker on a remote machine over SSH.
//!
//! [`provision`] takes an [`SshTarget`] and leaves a running `loom-server` on
//! it, using only the system `ssh` and the login the user already has:
//!
//! 1. probe the remote OS and architecture (`uname -sm`);
//! 2. install the `loom-server` release that matches this client under
//!    `~/.loom/server/<tag>/`, downloaded and checked against `SHA256SUMS` on
//!    the client and uploaded over the connection, so the host needs no
//!    internet access and no root;
//! 3. create `~/.loom/token` (mode `0600`) when absent and read it back;
//! 4. start the worker bound to the remote loopback, as a systemd user unit
//!    when the host has one and as a detached process otherwise;
//! 5. wait for the worker's `GET /health`.
//!
//! [`open_tunnel`] then forwards a local loopback port to the worker, so
//! nothing listens beyond loopback on either side and no TLS is needed. The
//! whole sequence is idempotent: a host already running this version is left
//! untouched, and a different version is replaced while the token and the state
//! database are kept.
//!
//! `ssh` runs in `BatchMode`, so it never prompts. An unknown host key or a key
//! that needs a passphrase fails with a message that says how to fix it.

use std::{
    env,
    io::Write,
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use loom_core::{ErrorCode, LoomError, Result};

use crate::worker::{
    GitHubReleases, ReleaseSource, ServerResolver, client_release_tag, platform_label,
};

/// Environment variable naming the `ssh` executable, `ssh` by default.
pub const SSH_BINARY_ENV: &str = "LOOM_SSH_BINARY";
/// The loopback port a provisioned worker listens on, on the remote host.
pub const DEFAULT_REMOTE_PORT: u16 = 8765;
/// How long a connection attempt may take before `ssh` gives up.
const CONNECT_TIMEOUT_SECS: u32 = 15;
/// How long the tunnel may take to start accepting connections.
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(20);

/// A machine reachable over SSH.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshTarget {
    /// `user@host`, or a host alias from `~/.ssh/config`.
    pub destination: String,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
    /// A jump host (`ssh -J`).
    pub jump_host: Option<String>,
    /// The loopback port the worker listens on, on the remote host.
    pub remote_port: u16,
}

impl SshTarget {
    /// Parses `ssh://[user@]host[:port]` or a bare `[user@]host`.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (rest, scheme) = match text.strip_prefix("ssh://") {
            Some(rest) => (rest.trim_end_matches('/'), true),
            None => (text, false),
        };
        let (destination, port) = match rest.rsplit_once(':') {
            Some((destination, port)) if scheme && !port.contains(']') => {
                let port = port
                    .parse::<u16>()
                    .map_err(|_| invalid(format!("'{port}' is not a valid SSH port")))?;
                (destination, Some(port))
            }
            _ => (rest, None),
        };
        validate_destination(destination)?;
        Ok(Self {
            destination: destination.to_owned(),
            port,
            identity_file: None,
            jump_host: None,
            remote_port: DEFAULT_REMOTE_PORT,
        })
    }

    /// The `ssh://` form shown to the user and stored as the worker's name.
    pub fn display(&self) -> String {
        match self.port {
            Some(port) => format!("ssh://{}:{port}", self.destination),
            None => format!("ssh://{}", self.destination),
        }
    }

    fn base_args(&self) -> Vec<String> {
        let mut args = vec![
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
            "-o".to_owned(),
            "ServerAliveInterval=15".to_owned(),
            "-o".to_owned(),
            "ServerAliveCountMax=3".to_owned(),
        ];
        if let Some(port) = self.port {
            args.push("-p".to_owned());
            args.push(port.to_string());
        }
        if let Some(identity) = &self.identity_file {
            args.push("-i".to_owned());
            args.push(identity.display().to_string());
        }
        if let Some(jump) = &self.jump_host {
            args.push("-J".to_owned());
            args.push(jump.clone());
        }
        args
    }
}

/// A destination that cannot be mistaken for an `ssh` option or split into
/// several arguments.
fn validate_destination(destination: &str) -> Result<()> {
    let valid = !destination.is_empty()
        && !destination.starts_with('-')
        && destination
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._@:[]%-".contains(c));
    if valid {
        Ok(())
    } else {
        Err(invalid(format!(
            "'{destination}' is not a valid SSH destination; use user@host or ssh://user@host:port"
        )))
    }
}

/// What a remote command printed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SshOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a command on a remote host. The system implementation shells out to
/// `ssh`; tests substitute one that runs the command locally.
pub trait SshRunner {
    /// Runs `command` through `sh -c` on `target` with `stdin` as its input.
    fn run(&self, target: &SshTarget, command: &str, stdin: &[u8]) -> Result<SshOutput>;
}

/// The runner that uses the system `ssh`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSsh;

fn ssh_binary() -> String {
    env::var(SSH_BINARY_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "ssh".to_owned())
}

impl SshRunner for SystemSsh {
    fn run(&self, target: &SshTarget, command: &str, stdin: &[u8]) -> Result<SshOutput> {
        let mut child = Command::new(ssh_binary())
            .args(target.base_args())
            .arg("--")
            .arg(&target.destination)
            .arg(format!("sh -c {}", shell_quote(command)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawn_error)?;
        if let Some(mut pipe) = child.stdin.take() {
            // A command that exits before reading everything closes the pipe;
            // its own exit status then says what went wrong.
            let _ = pipe.write_all(stdin);
        }
        let output = child.wait_with_output().map_err(spawn_error)?;
        Ok(SshOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn spawn_error(error: std::io::Error) -> LoomError {
    LoomError::new(
        ErrorCode::ProviderUnavailable,
        format!(
            "could not run '{}': {error}. Install an OpenSSH client or set {SSH_BINARY_ENV}.",
            ssh_binary()
        ),
        false,
    )
}

/// Single-quotes `text` for a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn invalid(message: String) -> LoomError {
    LoomError::invalid_request(message)
}

/// Turns a failed `ssh` run into an error that says what to do about it.
fn failure(step: &str, target: &SshTarget, output: &SshOutput) -> LoomError {
    let stderr = output.stderr.trim();
    let hint = if stderr.contains("Host key verification failed")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        format!(
            "the host key of {} is not trusted yet. Run `ssh {}` once, check the fingerprint, \
             and add it to known_hosts",
            target.destination, target.destination
        )
    } else if stderr.contains("Permission denied") {
        format!(
            "{} refused the login. Loom never asks for a password: load your key into ssh-agent \
             or set an identity file",
            target.destination
        )
    } else if stderr.contains("Could not resolve hostname")
        || stderr.contains("Connection refused")
        || stderr.contains("timed out")
    {
        format!("{} is not reachable over SSH", target.destination)
    } else {
        String::new()
    };
    let detail = if stderr.is_empty() {
        output.stdout.trim()
    } else {
        stderr
    };
    let message = if hint.is_empty() {
        format!("{step} on {} failed: {detail}", target.destination)
    } else {
        format!("{step} on {} failed: {hint} ({detail})", target.destination)
    };
    LoomError::new(ErrorCode::ProviderUnavailable, message, false)
}

fn run_checked(
    runner: &dyn SshRunner,
    target: &SshTarget,
    step: &str,
    command: &str,
    stdin: &[u8],
) -> Result<String> {
    let output = runner.run(target, command, stdin)?;
    if output.success {
        Ok(output.stdout)
    } else {
        Err(failure(step, target, &output))
    }
}

/// A stage of [`provision`], reported as it starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionStep {
    Probe,
    Download,
    Upload,
    Token,
    Start,
    Health,
}

impl ProvisionStep {
    pub fn label(self) -> &'static str {
        match self {
            Self::Probe => "Checking the remote host",
            Self::Download => "Fetching loom-server",
            Self::Upload => "Installing loom-server",
            Self::Token => "Preparing the access token",
            Self::Start => "Starting the worker",
            Self::Health => "Waiting for the worker",
        }
    }
}

/// A provisioned worker: what the host runs and the token that opens it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvisionedWorker {
    pub target: SshTarget,
    /// The `loom-server` release now installed and running.
    pub tag: String,
    pub token: String,
    /// `true` when the worker runs under systemd and so survives a reboot.
    pub supervised: bool,
}

/// The release platform label for the output of `uname -sm`.
pub fn parse_uname(output: &str) -> Result<&'static str> {
    let mut words = output.split_whitespace();
    let (os, arch) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    let os = match os {
        "Linux" => "linux",
        "Darwin" => "macos",
        other => {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                format!("no loom-server release is built for the remote OS '{other}'"),
                false,
            ));
        }
    };
    let arch = match arch {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                format!("no loom-server release is built for the remote architecture '{other}'"),
                false,
            ));
        }
    };
    platform_label(os, arch).ok_or_else(|| {
        LoomError::new(
            ErrorCode::UnsupportedCapability,
            format!("no loom-server release is built for {os}-{arch}"),
            false,
        )
    })
}

fn install_check_script(tag: &str) -> String {
    format!("test -x \"$HOME/.loom/server/{tag}/loom-server\" && echo present || echo missing")
}

fn install_script(tag: &str) -> String {
    format!(
        "set -e\n\
         d=\"$HOME/.loom/server/{tag}\"\n\
         mkdir -p \"$d.tmp\"\n\
         tar -xzf - -C \"$d.tmp\" loom-server\n\
         chmod 755 \"$d.tmp/loom-server\"\n\
         rm -rf \"$d\"\n\
         mv \"$d.tmp\" \"$d\"\n"
    )
}

const TOKEN_SCRIPT: &str = "set -e\n\
    umask 077\n\
    mkdir -p \"$HOME/.loom\"\n\
    t=\"$HOME/.loom/token\"\n\
    if [ ! -s \"$t\" ]; then\n\
      printf 'loom-%s' \"$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \\n')\" > \"$t\"\n\
    fi\n\
    cat \"$t\"\n";

fn health_command(port: u16) -> String {
    format!(
        "curl -fsS -o /dev/null http://127.0.0.1:{port}/health 2>/dev/null \
         || wget -q -O /dev/null http://127.0.0.1:{port}/health 2>/dev/null"
    )
}

/// Starts the worker unless this version already runs. Prints `running`,
/// `systemd` or `process` for the way it ended up supervised.
fn start_script(tag: &str, port: u16) -> String {
    let health = health_command(port);
    format!(
        "set -e\n\
         home=\"$HOME/.loom\"\n\
         if [ \"$(cat \"$home/current\" 2>/dev/null)\" = '{tag}' ] && ( {health} ); then\n\
           cat \"$home/supervisor\" 2>/dev/null || echo running\n\
           exit 0\n\
         fi\n\
         args=\"--bind 127.0.0.1:{port} --token-file $home/token --persistence $home/state.db\"\n\
         bin=\"$home/server/{tag}/loom-server\"\n\
         if [ -z \"$LOOM_NO_SYSTEMD\" ] && systemctl --user show-environment >/dev/null 2>&1; then\n\
           if [ -f \"$home/server.pid\" ]; then kill \"$(cat \"$home/server.pid\")\" 2>/dev/null || true; rm -f \"$home/server.pid\"; fi\n\
           mkdir -p \"$HOME/.config/systemd/user\"\n\
           cat > \"$HOME/.config/systemd/user/loom-server.service\" <<'UNIT'\n\
[Unit]\n\
Description=Loom worker\n\
After=network-online.target\n\
\n\
[Service]\n\
ExecStart=%h/.loom/server/{tag}/loom-server --bind 127.0.0.1:{port} --token-file %h/.loom/token --persistence %h/.loom/state.db\n\
Restart=on-failure\n\
\n\
[Install]\n\
WantedBy=default.target\n\
UNIT\n\
           systemctl --user daemon-reload\n\
           systemctl --user enable loom-server.service >/dev/null 2>&1\n\
           systemctl --user restart loom-server.service\n\
           loginctl enable-linger \"$(id -un)\" >/dev/null 2>&1 || true\n\
           echo systemd > \"$home/supervisor\"\n\
           echo systemd\n\
         else\n\
           if [ -f \"$home/server.pid\" ]; then kill \"$(cat \"$home/server.pid\")\" 2>/dev/null || true; sleep 1; fi\n\
           nohup \"$bin\" $args >> \"$home/server.log\" 2>&1 < /dev/null &\n\
           echo $! > \"$home/server.pid\"\n\
           echo process > \"$home/supervisor\"\n\
           echo process\n\
         fi\n\
         echo '{tag}' > \"$home/current\"\n"
    )
}

fn wait_healthy_script(port: u16) -> String {
    let health = health_command(port);
    format!(
        "i=0\n\
         while [ $i -lt 60 ]; do\n\
           if {health}; then exit 0; fi\n\
           i=$((i+1)); sleep 1\n\
         done\n\
         tail -n 20 \"$HOME/.loom/server.log\" 2>/dev/null || true\n\
         journalctl --user -u loom-server -n 20 --no-pager 2>/dev/null || true\n\
         exit 1\n"
    )
}

/// Installs and starts a worker on `target`. Re-running upgrades it to this
/// client's version and keeps its token and state.
pub fn provision(
    runner: &dyn SshRunner,
    source: &dyn ReleaseSource,
    target: &SshTarget,
    progress: &mut dyn FnMut(ProvisionStep),
) -> Result<ProvisionedWorker> {
    progress(ProvisionStep::Probe);
    let uname = run_checked(runner, target, "Probing the host", "uname -sm", &[])?;
    let platform = parse_uname(&uname)?;

    progress(ProvisionStep::Download);
    let resolver = ServerResolver {
        explicit: None,
        search_dirs: Vec::new(),
        cache_root: crate::worker::cache_root(),
        preferred_tag: client_release_tag(),
        platform: Some(platform),
        source,
    };
    let release = resolver.release()?;
    let tag = release.tag_name.clone();
    let present = run_checked(
        runner,
        target,
        "Checking the installed version",
        &install_check_script(&tag),
        &[],
    )?;
    if present.trim() != "present" {
        let (archive, kind) = resolver.fetch_verified(&release, platform)?;
        if kind != crate::worker::ArchiveKind::TarGz {
            return Err(internal("a remote install needs a tar.gz release asset"));
        }
        progress(ProvisionStep::Upload);
        run_checked(
            runner,
            target,
            "Installing loom-server",
            &install_script(&tag),
            &archive,
        )?;
    }

    progress(ProvisionStep::Token);
    let token = run_checked(runner, target, "Preparing the token", TOKEN_SCRIPT, &[])?
        .trim()
        .to_owned();
    if token.is_empty() {
        return Err(internal("the remote host returned an empty access token"));
    }

    progress(ProvisionStep::Start);
    let started = run_checked(
        runner,
        target,
        "Starting the worker",
        &start_script(&tag, target.remote_port),
        &[],
    )?;

    progress(ProvisionStep::Health);
    run_checked(
        runner,
        target,
        "Waiting for the worker to answer /health",
        &wait_healthy_script(target.remote_port),
        &[],
    )?;

    Ok(ProvisionedWorker {
        target: target.clone(),
        tag,
        token,
        supervised: started.trim() == "systemd",
    })
}

/// [`provision`] against the real GitHub releases and the system `ssh`.
pub fn provision_with_system_ssh(
    target: &SshTarget,
    progress: &mut dyn FnMut(ProvisionStep),
) -> Result<ProvisionedWorker> {
    provision(&SystemSsh, &GitHubReleases::default(), target, progress)
}

fn internal(message: impl Into<String>) -> LoomError {
    LoomError::new(ErrorCode::Internal, message, false)
}

/// Stops the worker on `target` and removes its install. `delete_data` also
/// removes the token and the state database.
pub fn remove(runner: &dyn SshRunner, target: &SshTarget, delete_data: bool) -> Result<()> {
    let script = remove_script(delete_data);
    run_checked(runner, target, "Removing the worker", &script, &[]).map(|_| ())
}

/// The removal script. It only disables the user unit when that unit is the one
/// this `HOME` installed: `systemctl --user` always talks to the invoking user's
/// manager, so a test or a client running with another `HOME` must not stop the
/// worker supervising it.
fn remove_script(delete_data: bool) -> String {
    let delete = if delete_data {
        "rm -rf \"$home\""
    } else {
        "rm -rf \"$home/server\" \"$home/current\" \"$home/supervisor\" \"$home/server.pid\""
    };
    format!(
        "home=\"$HOME/.loom\"\n\
         unit=\"$HOME/.config/systemd/user/loom-server.service\"\n\
         if [ -z \"$LOOM_NO_SYSTEMD\" ] && systemctl --user show-environment >/dev/null 2>&1; then\n\
           loaded=\"$(systemctl --user show loom-server.service -p FragmentPath 2>/dev/null | cut -d= -f2-)\"\n\
           if [ \"$loaded\" = \"$unit\" ]; then\n\
             systemctl --user disable --now loom-server.service >/dev/null 2>&1 || true\n\
             systemctl --user daemon-reload >/dev/null 2>&1 || true\n\
           fi\n\
         fi\n\
         rm -f \"$unit\"\n\
         if [ -f \"$home/server.pid\" ]; then kill \"$(cat \"$home/server.pid\")\" 2>/dev/null || true; fi\n\
         {delete}\n"
    )
}

/// A local port forwarded to the remote worker. Dropping it closes the tunnel.
pub struct SshTunnel {
    child: Child,
    local_port: u16,
}

impl SshTunnel {
    /// The WebSocket endpoint of the forwarded worker.
    pub fn websocket_url(&self) -> String {
        format!("ws://127.0.0.1:{}/ws", self.local_port)
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Whether the `ssh` process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Forwards a local loopback port (`local_port`, or a free one) to the worker
/// on `target`.
pub fn open_tunnel(target: &SshTarget, local_port: Option<u16>) -> Result<SshTunnel> {
    let local_port = match local_port {
        Some(port) => port,
        None => crate::worker::free_loopback_port()?,
    };
    let mut child = Command::new(ssh_binary())
        .args(target.base_args())
        .arg("-N")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-L")
        .arg(format!(
            "127.0.0.1:{local_port}:127.0.0.1:{}",
            target.remote_port
        ))
        .arg("--")
        .arg(&target.destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, local_port));
    let deadline = Instant::now() + TUNNEL_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
            }
            return Err(failure(
                "Opening the SSH tunnel",
                target,
                &SshOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: if stderr.trim().is_empty() {
                        format!("ssh exited with {status}")
                    } else {
                        stderr
                    },
                },
            ));
        }
        if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            return Ok(SshTunnel { child, local_port });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(LoomError::new(
                ErrorCode::DeadlineExceeded,
                format!(
                    "the SSH tunnel to {} did not open in time",
                    target.destination
                ),
                true,
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests;
