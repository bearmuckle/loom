//! The standalone `loom-server` executable.
//!
//! It serves the remote JSON WebSocket backend so native clients and browser
//! clients can use one Loom worker, or so a backend can be kept out of the
//! desktop client process entirely. The transport carries a single bearer token,
//! so it defaults to a loopback bind; publishing it further requires either TLS
//! (`--tls-cert` with `--tls-key`) or an explicit `--allow-insecure-remote`
//! opt-in. See `SECURITY.md` before exposing it further.

use std::{
    env,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use loom_core::{ErrorCode, LoomError};
use loom_protocol::ArchiveRetentionPolicy;
use loom_server::{
    ARCHIVE_RETENTION_ENV, ARCHIVE_RETENTION_FORCE_DISCARD_ENV, ARCHIVE_RETENTION_SUB_AGENT_ENV,
    ARCHIVE_RETENTION_SWEEP_INTERVAL, DEFAULT_MAX_FRAME_BYTES, DiagnosticsSettings,
    GitHubSignInConfig, InProcessBackend, Instance, InstanceLayout, InstanceOptions,
    MAX_FRAME_BYTES_ENV, NODE_NAME_ENV, ProcessContext, RemoteServerConfig, ReopenBackoff,
    ServerTlsConfig, ServingBackend, TlsState, apply_startup_archive_retention, diagnostics_lines,
    fail_stop_record_path, open_backend_named, reopen_after_fail_stop,
    resolve_archive_retention_with_sub_agent, resolve_max_frame_bytes, resolve_node_name,
    run_archive_retention_sweeps, start_remote_server, startup_summary,
    sweep_archive_retention_at_startup,
};

/// The bind address used when `--bind` is not supplied.
const DEFAULT_BIND: &str = "127.0.0.1:8765";

fn main() -> std::process::ExitCode {
    init_logging();
    // Bound how long a stalled remote may hold the backend thread that serves a
    // Git clone, fetch, or push. libgit2's socket timeouts are process globals
    // that are only safe to write before any thread exists, so this must stay
    // before `run` builds the async runtime; later it would be a silent no-op at
    // best. See `loom_vcs::configure_remote_timeouts`.
    //
    // SAFETY: no thread has been spawned yet: this is the first statement after
    // logging setup, and the runtime starts inside `run`.
    unsafe { loom_vcs::configure_remote_timeouts() };
    match run(env::args().skip(1)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            log::error!("loom-server: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Initializes the `log` facade so operator diagnostics are visible. The default
/// filter keeps the backend's own startup and health lines.
fn init_logging() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("loom_server=info"))
        .format_timestamp_millis()
        .init();
}

/// The parsed command line of `loom-server`.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ParsedArgs {
    /// `--help`/`-h`: print the usage text and exit successfully.
    Help,
    /// `--version`/`-V`: print the version and exit successfully.
    Version,
    /// `--diagnostics`: print the resolved deployment context and exit.
    Diagnostics(ServerOptions),
    /// Serve the standalone backend.
    Serve(ServerOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ServerOptions {
    bind: SocketAddr,
    instance_name: Option<String>,
    /// Raw `--node-name` value, before it is resolved against the environment.
    node_name: Option<String>,
    token: Option<String>,
    token_file: Option<PathBuf>,
    persistence: Option<PathBuf>,
    reset_state: bool,
    /// Raw `--archive-retention` value, before it is resolved into a policy.
    archive_retention: Option<String>,
    /// Raw `--archive-retention-force-discard` value.
    archive_retention_force_discard: Option<String>,
    /// Raw `--archive-retention-sub-agent` value.
    archive_retention_sub_agent: Option<String>,
    /// Raw `--max-frame-bytes` value, before it is resolved into a byte count.
    max_frame_bytes: Option<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    allow_insecure_remote: bool,
    /// GitHub logins allowed to sign in to this worker. Empty allows every
    /// GitHub account, which starts with no session grant.
    allow_github_login: Vec<String>,
    /// Refuses GitHub sign-in, so a WebSocket upgrade must carry an operator
    /// bearer token.
    no_github_sign_in: bool,
    /// OAuth app client id for the sign-in device flow; `None` uses the
    /// embedded GitHub CLI public client.
    github_oauth_client_id: Option<String>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("valid default bind address"),
            instance_name: None,
            node_name: None,
            token: None,
            token_file: None,
            persistence: None,
            reset_state: false,
            archive_retention: None,
            archive_retention_force_discard: None,
            archive_retention_sub_agent: None,
            max_frame_bytes: None,
            tls_cert: None,
            tls_key: None,
            allow_insecure_remote: false,
            allow_github_login: Vec::new(),
            no_github_sign_in: false,
            github_oauth_client_id: None,
        }
    }
}

impl ServerOptions {
    /// The archive-retention policy this start asks for, resolving the
    /// environment fallback exactly like the flags and refusing an unusable
    /// value with the flag or variable that carried it. `None` means no
    /// retention input was given, so the persisted policy applies.
    fn retention_policy(&self) -> Result<Option<ArchiveRetentionPolicy>, LoomError> {
        resolve_archive_retention_with_sub_agent(
            self.archive_retention.as_deref(),
            self.archive_retention_force_discard.as_deref(),
            self.archive_retention_sub_agent.as_deref(),
            env::var(ARCHIVE_RETENTION_ENV).ok().as_deref(),
            env::var(ARCHIVE_RETENTION_FORCE_DISCARD_ENV)
                .ok()
                .as_deref(),
            env::var(ARCHIVE_RETENTION_SUB_AGENT_ENV).ok().as_deref(),
        )
    }

    /// The most bytes the listener accepts in one inbound WebSocket frame:
    /// `--max-frame-bytes`, `LOOM_MAX_FRAME_BYTES`, or the documented 4 MiB
    /// default. An unusable value fails the start and names the input to fix.
    fn max_frame_bytes(&self) -> Result<usize, LoomError> {
        resolve_max_frame_bytes(
            self.max_frame_bytes.as_deref(),
            env::var(MAX_FRAME_BYTES_ENV).ok().as_deref(),
        )
    }

    /// The name this worker reports to clients: `--node-name`,
    /// `LOOM_NODE_NAME`, or the derived `hostname · short-id` display name when
    /// neither is given. An unusable value fails the start and names the input
    /// to fix.
    fn node_name(&self) -> Result<Option<String>, LoomError> {
        resolve_node_name(
            self.node_name.as_deref(),
            env::var(NODE_NAME_ENV).ok().as_deref(),
        )
    }

    /// The GitHub sign-in policy these flags ask for.
    ///
    /// Sign-in is on unless `--no-github-sign-in` was given, and an empty
    /// `--allow-github-login` set lets any GitHub account sign in. The client
    /// id is reported separately at startup, so an operator can see which OAuth
    /// app the device flow uses.
    fn github_sign_in_config(&self) -> GitHubSignInConfig {
        if self.no_github_sign_in {
            return GitHubSignInConfig::disabled();
        }
        let config = GitHubSignInConfig::new(
            self.allow_github_login
                .iter()
                .map(|login| login.trim().to_owned())
                .filter(|login| !login.is_empty()),
        );
        match &self.github_oauth_client_id {
            Some(client_id) => config.with_client_id(client_id.trim()),
            None => config,
        }
    }

    /// The settings [`Instance`] applies its defaults to. The archive-retention
    /// policy is resolved here because it is operator input that fails a start
    /// when it cannot be parsed.
    fn instance_options(&self) -> Result<InstanceOptions, LoomError> {
        Ok(InstanceOptions {
            bind: self.bind,
            instance_name: self.instance_name.clone(),
            persistence: self.persistence.clone(),
            token: self.token.clone(),
            token_file: self.token_file.clone(),
            reset_state: self.reset_state,
            archive_retention: self.retention_policy()?,
        })
    }
}

/// Runs the command line, with the argument iterator injected so the help,
/// version, and reporting paths can be exercised without spawning a process.
fn run(args: impl Iterator<Item = String>) -> Result<(), LoomError> {
    match parse_args(args)? {
        ParsedArgs::Help => {
            println!("{}", help_text());
            Ok(())
        }
        ParsedArgs::Version => {
            println!("{}", version_text());
            Ok(())
        }
        ParsedArgs::Diagnostics(options) => {
            println!("{}", diagnostics_report(&options)?);
            Ok(())
        }
        ParsedArgs::Serve(options) => serve_blocking(options),
    }
}

/// Parses the command line. Both `--flag value` and `--flag=value` are
/// accepted; `--help` and `--version` short-circuit the remaining arguments.
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<ParsedArgs, LoomError> {
    let mut options = ServerOptions::default();
    let mut diagnostics = false;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--bind" => options.bind = parse_bind(required_value(&mut args, "--bind")?)?,
            "--token" => options.token = Some(required_value(&mut args, "--token")?),
            "--instance-name" => {
                options.instance_name = Some(required_value(&mut args, "--instance-name")?);
            }
            "--node-name" => {
                options.node_name = Some(required_value(&mut args, "--node-name")?);
            }
            "--token-file" => {
                options.token_file =
                    Some(PathBuf::from(required_value(&mut args, "--token-file")?));
            }
            "--persistence" => {
                options.persistence =
                    Some(PathBuf::from(required_value(&mut args, "--persistence")?));
            }
            "--reset-state" => options.reset_state = true,
            "--archive-retention" => {
                options.archive_retention = Some(required_value(&mut args, "--archive-retention")?);
            }
            "--archive-retention-force-discard" => {
                options.archive_retention_force_discard = Some(required_value(
                    &mut args,
                    "--archive-retention-force-discard",
                )?);
            }
            "--archive-retention-sub-agent" => {
                options.archive_retention_sub_agent =
                    Some(required_value(&mut args, "--archive-retention-sub-agent")?);
            }
            "--max-frame-bytes" => {
                options.max_frame_bytes = Some(required_value(&mut args, "--max-frame-bytes")?);
            }
            "--diagnostics" => diagnostics = true,
            "--tls-cert" => {
                options.tls_cert = Some(PathBuf::from(required_value(&mut args, "--tls-cert")?));
            }
            "--tls-key" => {
                options.tls_key = Some(PathBuf::from(required_value(&mut args, "--tls-key")?));
            }
            "--allow-insecure-remote" => options.allow_insecure_remote = true,
            "--no-github-sign-in" => options.no_github_sign_in = true,
            "--allow-github-login" => options
                .allow_github_login
                .push(required_value(&mut args, "--allow-github-login")?),
            "--github-oauth-client-id" => {
                options.github_oauth_client_id =
                    Some(required_value(&mut args, "--github-oauth-client-id")?);
            }
            "--help" | "-h" => return Ok(ParsedArgs::Help),
            "--version" | "-V" => return Ok(ParsedArgs::Version),
            value if value.starts_with("--bind=") => {
                options.bind = parse_bind(value["--bind=".len()..].to_owned())?;
            }
            value if value.starts_with("--instance-name=") => {
                options.instance_name = Some(value["--instance-name=".len()..].to_owned());
            }
            value if value.starts_with("--node-name=") => {
                options.node_name = Some(value["--node-name=".len()..].to_owned());
            }
            value if value.starts_with("--token-file=") => {
                options.token_file = Some(PathBuf::from(&value["--token-file=".len()..]));
            }
            value if value.starts_with("--token=") => {
                options.token = Some(value["--token=".len()..].to_owned());
            }
            value if value.starts_with("--persistence=") => {
                options.persistence = Some(PathBuf::from(&value["--persistence=".len()..]));
            }
            value if value.starts_with("--archive-retention=") => {
                options.archive_retention = Some(value["--archive-retention=".len()..].to_owned());
            }
            value if value.starts_with("--archive-retention-force-discard=") => {
                options.archive_retention_force_discard =
                    Some(value["--archive-retention-force-discard=".len()..].to_owned());
            }
            value if value.starts_with("--archive-retention-sub-agent=") => {
                options.archive_retention_sub_agent =
                    Some(value["--archive-retention-sub-agent=".len()..].to_owned());
            }
            value if value.starts_with("--max-frame-bytes=") => {
                options.max_frame_bytes = Some(value["--max-frame-bytes=".len()..].to_owned());
            }
            value if value.starts_with("--tls-cert=") => {
                options.tls_cert = Some(PathBuf::from(&value["--tls-cert=".len()..]));
            }
            value if value.starts_with("--tls-key=") => {
                options.tls_key = Some(PathBuf::from(&value["--tls-key=".len()..]));
            }
            value if value.starts_with("--allow-github-login=") => {
                options
                    .allow_github_login
                    .push(value["--allow-github-login=".len()..].to_owned());
            }
            value if value.starts_with("--github-oauth-client-id=") => {
                options.github_oauth_client_id =
                    Some(value["--github-oauth-client-id=".len()..].to_owned());
            }
            unknown => {
                return Err(LoomError::invalid_request(format!(
                    "unknown argument '{unknown}'; use --help for usage"
                )));
            }
        }
    }
    Ok(if diagnostics {
        ParsedArgs::Diagnostics(options)
    } else {
        ParsedArgs::Serve(options)
    })
}

fn required_value(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, LoomError> {
    args.next()
        .ok_or_else(|| LoomError::invalid_request(format!("{flag} requires a value")))
}

fn parse_bind(value: String) -> Result<SocketAddr, LoomError> {
    value.parse().map_err(|error| {
        LoomError::invalid_request(format!("--bind must be a socket address: {error}"))
    })
}

/// Resolves the optional TLS material. `--tls-cert` and `--tls-key` only mean
/// something together, so supplying one without the other is an error rather
/// than a silently plaintext listener.
fn resolve_tls(options: &ServerOptions) -> Result<Option<ServerTlsConfig>, LoomError> {
    match (&options.tls_cert, &options.tls_key) {
        (Some(certificate), Some(private_key)) => {
            ServerTlsConfig::from_pem_files(certificate, private_key).map(Some)
        }
        (None, None) => Ok(None),
        (Some(_), None) => Err(LoomError::invalid_request(
            "--tls-cert requires the matching --tls-key; supply both or neither",
        )),
        (None, Some(_)) => Err(LoomError::invalid_request(
            "--tls-key requires the matching --tls-cert; supply both or neither",
        )),
    }
}

fn help_text() -> String {
    format!(
        "Usage: loom-server [--bind <addr>] [--instance-name <name>] \
         [--node-name <name>] \
         [--token <token> | --token-file <path>] [--persistence <path>] \
         [--reset-state] [--archive-retention <duration>] \
         [--archive-retention-force-discard <bool>] \
         [--max-frame-bytes <bytes>] \
         [--tls-cert <pem> --tls-key <pem>] \
         [--allow-insecure-remote] [--diagnostics] \
         [--allow-github-login <login> ...] [--no-github-sign-in] \
         [--github-oauth-client-id <id>]\n\
         \n\
         A standalone Loom backend that serves the remote WebSocket protocol.\n\
         \n\
         Options:\n\
         \x20 --bind <addr>         address to listen on (default {DEFAULT_BIND})\n\
         \x20 --instance-name <name>\n\
         \x20                        pin this instance's directory instead of naming it \
         after --bind\n\
         \x20 --node-name <name>   name this worker in client status lines instead of \
         the derived hostname and node id (LOOM_NODE_NAME is the fallback)\n\
         \x20 --token <token>       bearer token clients must present\n\
         \x20 --token-file <path>   file whose trimmed contents are the bearer token\n\
         \x20 --persistence <path>  keep state in the database at this path\n\
         \x20 --reset-state         wipe an incompatible state database and start empty\n\
         \x20 --archive-retention <duration>\n\
         \x20                        delete archived projects older than <duration> \
         (LOOM_ARCHIVE_RETENTION is the fallback)\n\
         \x20 --archive-retention-force-discard <bool>\n\
         \x20                        let the retention sweep discard dirty or locked \
         worktrees (LOOM_ARCHIVE_RETENTION_FORCE_DISCARD is the fallback)\n\
         \x20 --archive-retention-sub-agent <duration>\n\
         \x20                        delete archived sub-agent sessions older than \
         <duration>; default 10m, off disables (LOOM_ARCHIVE_RETENTION_SUB_AGENT is the \
         fallback)\n\
         \x20 --max-frame-bytes <bytes>\n\
         \x20                        accept at most this many bytes in one WebSocket frame \
         (default {DEFAULT_MAX_FRAME_BYTES}; LOOM_MAX_FRAME_BYTES is the fallback)\n\
         \x20 --tls-cert <pem>      serve wss:// with this PEM certificate chain\n\
         \x20 --tls-key <pem>       PEM private key matching --tls-cert\n\
         \x20 --allow-insecure-remote\n\
         \x20                        allow a plaintext ws:// bind beyond loopback\n\
         \x20 --allow-github-login <login>\n\
         \x20                        restrict sign-in to this GitHub login; repeat for each \
         person (default: any GitHub account may sign in, starting with no session access)\n\
         \x20 --no-github-sign-in   refuse GitHub sign-in, so every client must present \
         an operator bearer token\n\
         \x20 --github-oauth-client-id <id>\n\
         \x20                        OAuth app client id for the sign-in device flow \
         (default: the embedded GitHub CLI public client id)\n\
         \x20 --diagnostics         print the resolved deployment context and exit\n\
         \x20 -h, --help            print this help and exit\n\
         \x20 -V, --version         print the version and exit\n\
         \n\
         Without --persistence this server keeps its durable state in \
         <state-dir>/<instance-name or bind-key>/state.db, where <state-dir> follows \
         LOOM_STATE_DIR, then $XDG_STATE_HOME, then $HOME/.local/state, plus a loom \
         subdirectory; the directory is created with mode 0700. A bind port of 0 has no \
         stable identity and keeps its state in memory.\n\
         \n\
         Without --token or --token-file this server reads <instance-dir>/token and \
         generates a loom-<uuid> token there when it is missing, written with mode 0600 \
         like the credential store. The path is always logged; the value is printed only \
         when stdout is a terminal.\n\
         \n\
         Archive retention is disabled by default, so an absent or zero value never \
         auto-deletes. A duration is <n><unit> with unit ms, s, m, h, d, or w; a bare \
         integer means seconds, and off and never mean disabled. The sweep runs once at \
         startup and every {} minutes while the server runs.\n\
         \n\
         One WebSocket frame is limited to {} bytes ({} MiB) by default; \
         --max-frame-bytes or LOOM_MAX_FRAME_BYTES raises or lowers that bound for this \
         listener, and a client that reaches it sets its own limit. The protocol has no \
         chunking, so a frame above the limit is reported as payload_too_large naming the \
         request, the received size, and the limit.\n\
         \n\
         A bind address that is not loopback needs TLS; --allow-insecure-remote accepts \
         the risk of sending the bearer token and all traffic in plaintext instead.\n\
         \n\
         --diagnostics prints where agent commands would execute, as which account, which \
         state a start would use, and the intended egress policy, without printing the \
         token value.",
        ARCHIVE_RETENTION_SWEEP_INTERVAL.as_secs() / 60,
        DEFAULT_MAX_FRAME_BYTES,
        DEFAULT_MAX_FRAME_BYTES / (1024 * 1024)
    )
}

/// The deployment diagnostics report for `options`, resolved against the state
/// directory [`loom_core::state_dir`] returns.
fn diagnostics_report(options: &ServerOptions) -> Result<String, LoomError> {
    diagnostics_report_in(options, &loom_core::state_dir())
}

/// [`diagnostics_report`] with the state directory injected, so a test can
/// describe a deployment without writing into the operator's state directory.
fn diagnostics_report_in(
    options: &ServerOptions,
    state_directory: &Path,
) -> Result<String, LoomError> {
    let instance_options = options.instance_options()?;
    // Resolving the layout is the whole report: it creates no directory, opens no
    // database, and never generates a token, so running it against a live
    // deployment changes nothing.
    let layout = InstanceLayout::resolve(&instance_options, state_directory)?;
    let settings = diagnostics_settings(options, &instance_options, &layout, state_directory);
    Ok(diagnostics_lines(&ProcessContext::detect(), &settings).join("\n"))
}

/// The facts both the diagnostics report and the startup log line describe.
fn diagnostics_settings<'a>(
    options: &ServerOptions,
    instance_options: &'a InstanceOptions,
    layout: &'a InstanceLayout,
    state_directory: &Path,
) -> DiagnosticsSettings<'a> {
    DiagnosticsSettings {
        version: version_text(),
        options: instance_options,
        layout,
        state_root: loom_core::state_root(),
        state_directory: state_directory.to_path_buf(),
        tls: TlsState::from_flags(options.tls_cert.is_some(), options.tls_key.is_some()),
        allow_insecure_remote: options.allow_insecure_remote,
    }
}

/// The line printed by `--version`: the release version plus the revision when
/// one was stamped at build time.
fn version_text() -> String {
    format!(
        "loom-server {}",
        version_label(build_version(), git_revision())
    )
}

/// The release version stamped by `build.rs`, or the crate version when the
/// build did not stamp one.
fn build_version() -> &'static str {
    option_env!("LOOM_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// The revision stamped by `build.rs`, or the literal `unknown`.
fn git_revision() -> &'static str {
    option_env!("LOOM_GIT_REVISION").unwrap_or("unknown")
}

/// Joins a version and revision the way the client's About pane does, so a
/// release reads `v0.8.1 · <sha>`.
fn version_label(version: &str, revision: &str) -> String {
    let revision = revision.trim();
    if revision.is_empty() || revision == "unknown" {
        version.to_owned()
    } else {
        format!("{version} · {revision}")
    }
}

/// Binds the backend through the shared serve bootstrap, after resolving the
/// TLS material `--tls-cert` and `--tls-key` ask for. The bootstrap itself —
/// the bearer token store, the plaintext-exposure warning, and the URL logging —
/// is the same one the native shell's `--serve` mode uses.
async fn start(
    options: &ServerOptions,
    backend: Arc<InProcessBackend>,
    instance: &Instance,
) -> Result<ServingBackend, LoomError> {
    let tls = resolve_tls(options)?;
    let config = RemoteServerConfig {
        bind_addr: options.bind,
        max_frame_bytes: options.max_frame_bytes()?,
        tls,
        allow_insecure_remote: options.allow_insecure_remote,
        github_sign_in: options.github_sign_in_config(),
        ..RemoteServerConfig::default()
    };
    start_remote_server(backend, instance.token().token(), config).await
}

fn serve_blocking(options: ServerOptions) -> Result<(), LoomError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not start async runtime: {error}"),
                false,
            )
        })?;
    runtime.block_on(serve(options, wait_for_shutdown_signal()))
}

/// Binds the backend, waits for `shutdown`, then stops the server. The shutdown
/// future is a parameter so a test can drive the whole lifecycle without
/// raising a process signal.
async fn serve(
    options: ServerOptions,
    shutdown: impl Future<Output = Result<(), LoomError>>,
) -> Result<(), LoomError> {
    serve_in(options, &loom_core::state_dir(), shutdown, fail_stop_reopen).await
}

/// The reopen of a fail-stopped backend, as [`serve_in`] takes it: it is built
/// from the backend that is actually served, and it resolves only after the
/// recorded wait so a still-present failure does not restart in a tight loop.
type FailStopReopen = Pin<Box<dyn Future<Output = LoomError>>>;

fn fail_stop_reopen(backend: Arc<InProcessBackend>, record: Option<PathBuf>) -> FailStopReopen {
    Box::pin(reopen_after_fail_stop(
        backend,
        record,
        ReopenBackoff::default(),
    ))
}

/// [`serve`] with the state directory injected, so a test can drive the whole
/// lifecycle without writing into the operator's real state directory, and with
/// the fail-stop reopen injected, so a test can drive the exit path without
/// latching a live backend.
async fn serve_in(
    options: ServerOptions,
    state_directory: &Path,
    shutdown: impl Future<Output = Result<(), LoomError>>,
    reopen: impl FnOnce(Arc<InProcessBackend>, Option<PathBuf>) -> FailStopReopen,
) -> Result<(), LoomError> {
    // The node name is validated before preparation, which can wipe an
    // incompatible database under `--reset-state` and generate a token file, so
    // a bad value fails the start without changing state.
    let node_name = options.node_name()?;
    let instance = prepare_instance(&options, state_directory)?;
    let instance_options = options.instance_options()?;
    let settings = diagnostics_settings(
        &options,
        &instance_options,
        instance.layout(),
        state_directory,
    );
    log::info!("{}", startup_summary(&ProcessContext::detect(), &settings));
    let backend = open_backend_named(instance.state_db(), node_name.as_deref())?;
    // An explicit flag or variable wins over the persisted policy and is
    // persisted in turn; otherwise the policy loaded when the durable state was
    // opened applies; otherwise retention stays disabled. Either way the
    // startup sweep runs before the listener accepts clients, and a failed sweep
    // is reported without stopping the server.
    apply_startup_archive_retention(&backend, instance_options.archive_retention)?;
    sweep_archive_retention_at_startup(&backend);
    let server = start(&options, Arc::clone(&backend), &instance).await?;
    let sweeps =
        run_archive_retention_sweeps(Arc::clone(&backend), ARCHIVE_RETENTION_SWEEP_INTERVAL);
    // A latched persistence fail-stop never clears itself, and the process would
    // otherwise keep refusing every durable request until an operator noticed.
    // Exiting lets `Restart=on-failure` reopen the backend from its last
    // committed state, and the recorded wait keeps a failure that is still
    // present (a full filesystem) from restarting in a tight loop.
    let reopen = reopen(
        Arc::clone(&backend),
        instance.state_db().map(fail_stop_record_path),
    );
    tokio::select! {
        result = shutdown => result?,
        // The driver sweeps immediately and then on every interval until it is
        // dropped by the shutdown branch.
        () = sweeps => {}
        fail_stop = reopen => {
            // Release the database before exiting, so the reopened process can
            // take exclusive ownership of it.
            if let Err(error) = server.stop().await {
                log::error!(
                    "loom-server: could not stop cleanly after a persistence fail-stop: {}",
                    error.message
                );
            }
            return Err(fail_stop);
        }
    }
    log::info!("Shutting down loom-server");
    server.stop().await
}

/// Resolves this instance's paths and bearer token, reporting both so an
/// operator can see which database and token this start used. The token value
/// itself is printed only when stdout is a terminal, so it does not land in
/// journald or `docker logs`.
fn prepare_instance(
    options: &ServerOptions,
    state_directory: &Path,
) -> Result<Instance, LoomError> {
    let instance = Instance::prepare(&options.instance_options()?, state_directory)?;
    if let Some(directory) = instance.layout().directory() {
        log::info!("Instance directory: {}", directory.display());
    }
    if let Some(path) = instance.state_db() {
        log::info!("State database: {}", path.display());
    }
    instance.token().log_source();
    instance.token().print_generated();
    Ok(instance)
}

/// Resolves once the process receives SIGINT, or SIGTERM on unix, so systemd
/// and container runtimes can stop the backend the way they expect.
async fn wait_for_shutdown_signal() -> Result<(), LoomError> {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not install the SIGTERM handler: {error}"),
                false,
            )
        })?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
        .map_err(shutdown_signal_error)
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map_err(shutdown_signal_error)
    }
}

fn shutdown_signal_error(error: std::io::Error) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("could not wait for shutdown: {error}"),
        false,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        path::{Path, PathBuf},
    };

    use loom_protocol::ArchiveRetentionPolicy;
    use loom_server::{
        DEFAULT_MAX_FRAME_BYTES, InProcessBackend, Instance, TokenSource, bind_exposure_warning,
        open_backend,
    };

    use super::{
        FailStopReopen, ParsedArgs, ServerOptions, build_version, diagnostics_report_in,
        fail_stop_record_path, fail_stop_reopen, help_text, parse_args, prepare_instance,
        resolve_tls, run, serve_in, start, version_label, version_text,
    };
    use loom_server::resolve_archive_retention;

    fn arguments<'a>(values: &'a [&str]) -> impl Iterator<Item = String> + 'a {
        values.iter().map(|value| (*value).to_owned())
    }

    fn parse(values: &[&str]) -> Result<ParsedArgs, loom_core::LoomError> {
        parse_args(arguments(values))
    }

    fn serve_options(values: &[&str]) -> ServerOptions {
        match parse(values) {
            Ok(ParsedArgs::Serve(options)) => options,
            other => panic!("expected serve options, got {other:?}"),
        }
    }

    fn parse_error(values: &[&str]) -> loom_core::LoomError {
        parse(values).expect_err("arguments should have been rejected")
    }

    fn temporary_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("loom-server-{label}-{}", loom_core::RunId::new()))
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let path = temporary_path(&format!("dir-{label}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// The instance a test server runs as. Callers always pass a bind address
    /// with an ephemeral port or an explicit token, so resolving it below the
    /// temp directory writes nothing into the operator's real state directory.
    fn instance(options: &ServerOptions) -> Instance {
        prepare_instance(options, &std::env::temp_dir()).unwrap()
    }

    /// A fail-stop reopen that never fires, for the tests that drive a server
    /// that keeps serving.
    fn never_reopen(
        _backend: std::sync::Arc<InProcessBackend>,
        _record: Option<PathBuf>,
    ) -> FailStopReopen {
        Box::pin(std::future::pending())
    }

    #[test]
    fn parser_keeps_defaults_and_accepts_spaced_and_equals_values() {
        let defaults = serve_options(&[]);
        assert_eq!(defaults.bind.to_string(), "127.0.0.1:8765");
        assert_eq!(defaults.instance_name, None);
        assert_eq!(defaults.node_name, None);
        assert_eq!(defaults.token, None);
        assert_eq!(defaults.token_file, None);
        assert_eq!(defaults.persistence, None);
        assert!(!defaults.reset_state);
        assert_eq!(defaults.tls_cert, None);
        assert_eq!(defaults.tls_key, None);
        assert!(!defaults.allow_insecure_remote);
        assert_eq!(defaults.archive_retention, None);
        assert_eq!(defaults.archive_retention_force_discard, None);
        // GitHub sign-in is on by default and lets any GitHub account in; the
        // embedded GitHub CLI client id is the default OAuth app.
        assert!(defaults.allow_github_login.is_empty());
        assert!(!defaults.no_github_sign_in);
        assert_eq!(defaults.github_oauth_client_id, None);
        let open = defaults.github_sign_in_config();
        assert!(open.is_enabled());
        assert_eq!(open.allowlist_size(), 0);
        assert!(open.allows_login("octocat"));

        // An allowlist and an off switch narrow or disable it.
        let restricted = serve_options(&["--allow-github-login", "octocat"]);
        assert!(restricted.github_sign_in_config().allows_login("octocat"));
        assert!(!restricted.github_sign_in_config().allows_login("mallory"));
        let disabled = serve_options(&["--no-github-sign-in"]);
        assert!(!disabled.github_sign_in_config().is_enabled());

        let spaced = serve_options(&[
            "--bind",
            "127.0.0.1:9000",
            "--instance-name",
            "worker",
            "--node-name",
            "prod-builder",
            "--token",
            "spaced-token",
            "--persistence",
            "/tmp/loom.db",
            "--reset-state",
            "--archive-retention",
            "14d",
            "--archive-retention-force-discard",
            "yes",
            "--tls-cert",
            "/tmp/cert.pem",
            "--tls-key",
            "/tmp/key.pem",
            "--allow-insecure-remote",
            "--allow-github-login",
            "bjornharrtell",
            "--allow-github-login",
            "octocat",
            "--github-oauth-client-id",
            "own-client-id",
        ]);
        assert_eq!(spaced.bind.to_string(), "127.0.0.1:9000");
        assert_eq!(spaced.instance_name.as_deref(), Some("worker"));
        assert_eq!(spaced.node_name.as_deref(), Some("prod-builder"));
        assert!(spaced.reset_state);
        assert_eq!(spaced.token.as_deref(), Some("spaced-token"));
        assert_eq!(
            spaced.persistence.as_deref(),
            Some(Path::new("/tmp/loom.db"))
        );
        assert_eq!(spaced.archive_retention.as_deref(), Some("14d"));
        assert_eq!(
            spaced.archive_retention_force_discard.as_deref(),
            Some("yes")
        );
        assert_eq!(spaced.tls_cert.as_deref(), Some(Path::new("/tmp/cert.pem")));
        assert_eq!(spaced.tls_key.as_deref(), Some(Path::new("/tmp/key.pem")));
        assert!(spaced.allow_insecure_remote);
        let sign_in = spaced.github_sign_in_config();
        assert!(sign_in.is_enabled());
        assert_eq!(sign_in.allowlist_size(), 2);
        assert_eq!(sign_in.client_id.as_deref(), Some("own-client-id"));

        let equals = serve_options(&[
            "--bind=0.0.0.0:9",
            "--instance-name=pinned",
            "--node-name=prod-builder",
            "--token-file=/tmp/token",
            "--persistence=/tmp/other.db",
            "--archive-retention=30m",
            "--archive-retention-force-discard=0",
            "--tls-cert=/tmp/other-cert.pem",
            "--tls-key=/tmp/other-key.pem",
            "--allow-github-login=bjornharrtell",
            "--github-oauth-client-id=other-client-id",
        ]);
        assert_eq!(equals.bind.to_string(), "0.0.0.0:9");
        assert_eq!(equals.instance_name.as_deref(), Some("pinned"));
        assert_eq!(equals.node_name.as_deref(), Some("prod-builder"));
        assert_eq!(equals.token, None);
        assert_eq!(equals.token_file.as_deref(), Some(Path::new("/tmp/token")));
        assert_eq!(
            equals.persistence.as_deref(),
            Some(Path::new("/tmp/other.db"))
        );
        assert_eq!(equals.archive_retention.as_deref(), Some("30m"));
        assert_eq!(equals.archive_retention_force_discard.as_deref(), Some("0"));
        assert_eq!(
            equals.tls_cert.as_deref(),
            Some(Path::new("/tmp/other-cert.pem"))
        );
        assert_eq!(
            equals.tls_key.as_deref(),
            Some(Path::new("/tmp/other-key.pem"))
        );
        assert!(!equals.allow_insecure_remote);
        assert_eq!(equals.allow_github_login, vec!["bjornharrtell".to_owned()]);
        assert_eq!(
            equals.github_sign_in_config().client_id.as_deref(),
            Some("other-client-id")
        );
    }

    #[test]
    fn parser_rejects_missing_values_invalid_binds_and_unknown_arguments() {
        for flag in [
            "--bind",
            "--instance-name",
            "--node-name",
            "--token",
            "--token-file",
            "--persistence",
            "--archive-retention",
            "--archive-retention-force-discard",
            "--tls-cert",
            "--tls-key",
            "--allow-github-login",
            "--github-oauth-client-id",
        ] {
            let error = parse_error(&[flag]);
            assert!(
                error.message.contains("requires a value"),
                "{flag}: {error}"
            );
        }
        for values in [
            vec!["--bind", "not-an-address"],
            vec!["--bind=not-an-address"],
        ] {
            let error = parse_error(&values);
            assert!(error.message.contains("socket address"), "{error}");
        }
        assert!(
            parse_error(&["--mystery"])
                .message
                .contains("unknown argument")
        );
    }

    #[test]
    fn archive_retention_flags_and_environment_resolve_one_policy() {
        // The flags reach the settings the backend is configured from.
        let flagged = serve_options(&[
            "--archive-retention",
            "14d",
            "--archive-retention-force-discard",
            "yes",
        ]);
        assert_eq!(
            flagged.instance_options().unwrap().archive_retention,
            Some(ArchiveRetentionPolicy {
                retention_ms: Some(1_209_600_000),
                force_discard_worktrees: true,
                sub_agent_retention_ms: Some(600_000),
            })
        );

        // The environment is the fallback, and a flag always wins over it.
        assert_eq!(
            resolve_archive_retention(None, None, Some("2h"), Some("yes")).unwrap(),
            Some(ArchiveRetentionPolicy {
                retention_ms: Some(7_200_000),
                force_discard_worktrees: true,
                sub_agent_retention_ms: Some(600_000),
            })
        );
        let disabled = serve_options(&[
            "--archive-retention=off",
            "--archive-retention-force-discard=0",
        ])
        .instance_options()
        .unwrap()
        .archive_retention;
        assert_eq!(disabled, Some(ArchiveRetentionPolicy::disabled()));
        if std::env::var("LOOM_ARCHIVE_RETENTION").is_err()
            && std::env::var("LOOM_ARCHIVE_RETENTION_FORCE_DISCARD").is_err()
        {
            // No input at all leaves the policy to the durable state, so a
            // start neither enables nor disables retention by itself.
            assert_eq!(
                serve_options(&[])
                    .instance_options()
                    .unwrap()
                    .archive_retention,
                None
            );
            assert_eq!(
                resolve_archive_retention(None, None, None, None).unwrap(),
                None
            );
        }

        // A rejected value names the flag it came from.
        let error = serve_options(&["--archive-retention", "soon"])
            .instance_options()
            .unwrap_err();
        assert_eq!(error.code, loom_core::ErrorCode::InvalidRequest);
        assert!(error.message.contains("--archive-retention"), "{error}");
        let error = serve_options(&["--archive-retention-force-discard=maybe"])
            .instance_options()
            .unwrap_err();
        assert!(
            error.message.contains("--archive-retention-force-discard"),
            "{error}"
        );
        assert!(
            parse(&["--archive-retention"])
                .unwrap_err()
                .message
                .contains("requires a value")
        );
        assert!(
            parse_error(&["--max-frame-bytes"])
                .message
                .contains("requires a value")
        );
    }

    #[test]
    fn the_frame_limit_resolves_from_the_flag_then_the_environment() {
        assert_eq!(
            ServerOptions::default().max_frame_bytes().unwrap(),
            DEFAULT_MAX_FRAME_BYTES
        );

        let spaced = ServerOptions {
            max_frame_bytes: Some("8192".to_owned()),
            ..ServerOptions::default()
        };
        assert_eq!(spaced.max_frame_bytes().unwrap(), 8192);

        let equals = serve_options(&["--max-frame-bytes=16384"]);
        assert_eq!(equals.max_frame_bytes.as_deref(), Some("16384"));
        assert_eq!(equals.max_frame_bytes().unwrap(), 16384);

        // A rejected value names the input to fix.
        let rejected = ServerOptions {
            max_frame_bytes: Some("several".to_owned()),
            ..ServerOptions::default()
        };
        let error = rejected.max_frame_bytes().unwrap_err();
        assert_eq!(error.code, loom_core::ErrorCode::InvalidRequest);
        assert!(error.message.contains("--max-frame-bytes"), "{error}");
        let zero = ServerOptions {
            max_frame_bytes: Some("0".to_owned()),
            ..ServerOptions::default()
        };
        assert!(
            zero.max_frame_bytes()
                .unwrap_err()
                .message
                .contains("greater than zero")
        );
    }

    #[test]
    fn the_node_name_resolves_from_the_flag_then_the_environment() {
        // Neither input keeps the derived `hostname · short-id` display name.
        if std::env::var(loom_server::NODE_NAME_ENV).is_err() {
            assert_eq!(ServerOptions::default().node_name().unwrap(), None);
        }

        // Both the spaced and the `=` form reach the resolver, which trims the
        // value it is given.
        let spaced = serve_options(&["--node-name", "  prod-builder  "]);
        assert_eq!(spaced.node_name.as_deref(), Some("  prod-builder  "));
        assert_eq!(spaced.node_name().unwrap().as_deref(), Some("prod-builder"));
        let equals = serve_options(&["--node-name=prod-builder"]);
        assert_eq!(equals.node_name().unwrap().as_deref(), Some("prod-builder"));

        // A rejected value names the flag it came from.
        let empty = serve_options(&["--node-name", "   "]);
        let error = empty.node_name().unwrap_err();
        assert_eq!(error.code, loom_core::ErrorCode::InvalidRequest);
        assert!(error.message.contains("--node-name"), "{error}");
    }

    #[test]
    fn help_and_version_short_circuit_the_remaining_arguments() {
        assert_eq!(parse(&["--help"]).unwrap(), ParsedArgs::Help);
        assert_eq!(parse(&["-h", "--mystery"]).unwrap(), ParsedArgs::Help);
        assert_eq!(parse(&["--version"]).unwrap(), ParsedArgs::Version);
        assert_eq!(parse(&["-V", "--bind"]).unwrap(), ParsedArgs::Version);
    }

    #[test]
    fn help_and_version_output_report_the_stamped_build() {
        run(arguments(&["--help"])).unwrap();
        run(arguments(&["-V"])).unwrap();
        let help = help_text();
        assert!(help.contains("--instance-name <name>"));
        assert!(help.contains("--node-name <name>"));
        assert!(help.contains("LOOM_NODE_NAME"));
        assert!(help.contains("--reset-state"));
        assert!(help.contains("--token-file <path>"));
        assert!(help.contains("--tls-cert <pem>"));
        assert!(help.contains("--tls-key <pem>"));
        assert!(help.contains("--allow-insecure-remote"));
        assert!(help.contains("--diagnostics"));
        assert!(help.contains("--archive-retention <duration>"));
        assert!(help.contains("--archive-retention-force-discard <bool>"));
        assert!(help.contains("LOOM_ARCHIVE_RETENTION"));
        assert!(help.contains("LOOM_ARCHIVE_RETENTION_FORCE_DISCARD"));
        assert!(help.contains("--max-frame-bytes <bytes>"));
        assert!(help.contains("LOOM_MAX_FRAME_BYTES"));
        assert!(help.contains("--allow-github-login <login>"));
        assert!(help.contains("--github-oauth-client-id <id>"));
        assert!(
            !help.contains("--participants-file"),
            "per-person bearer tokens are retired"
        );
        assert!(help.contains("4194304"));
        assert!(help.contains("127.0.0.1:8765"));
        assert!(version_text().starts_with("loom-server "));
        assert!(version_text().contains(build_version()));
        assert!(!build_version().is_empty());
    }

    #[test]
    fn diagnostics_mode_prints_the_resolved_context_and_help_still_wins() {
        assert_eq!(
            parse(&["--diagnostics"]).unwrap(),
            ParsedArgs::Diagnostics(ServerOptions::default())
        );
        let parsed = parse(&["--bind", "0.0.0.0:9", "--diagnostics"]).unwrap();
        assert!(
            matches!(parsed, ParsedArgs::Diagnostics(options) if options.bind.to_string() == "0.0.0.0:9")
        );
        // A flag after the mode argument is parsed too, so the report describes
        // the deployment the operator asked about rather than the defaults.
        let parsed = parse(&["--diagnostics", "--instance-name=worker"]).unwrap();
        assert!(
            matches!(parsed, ParsedArgs::Diagnostics(options) if options.instance_name.as_deref() == Some("worker"))
        );
        assert_eq!(
            parse(&["--diagnostics", "--help"]).unwrap(),
            ParsedArgs::Help
        );
        assert_eq!(parse(&["-h", "--diagnostics"]).unwrap(), ParsedArgs::Help);
    }

    #[test]
    fn diagnostics_report_describes_the_deployment_without_the_token_value() {
        let state = temporary_directory("diagnostics");
        let directory = state.join("127.0.0.1_8765");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("token"), "top-secret-token\n").unwrap();

        let report =
            diagnostics_report_in(&serve_options(&["--bind", "127.0.0.1:8765"]), &state).unwrap();
        assert!(report.starts_with("loom-server "), "{report}");
        assert!(
            report.contains("execution-context: host")
                || report.contains("execution-context: container"),
            "{report}"
        );
        assert!(report.contains("deployment-role: "), "{report}");
        assert!(report.contains("state-persistence: durable"), "{report}");
        assert!(
            report.contains(&format!(
                "state-database: {}",
                directory.join("state.db").display()
            )),
            "{report}"
        );
        assert!(
            report.contains("bearer-token: from the existing file"),
            "{report}"
        );
        assert!(report.contains("network-egress: "), "{report}");
        // The report is safe to paste into a bug report, and it resolves paths
        // without creating them: `--diagnostics` never touches state.
        assert!(!report.contains("top-secret-token"), "{report}");
        assert!(!directory.join("state.db").exists());
        assert!(directory.join("token").exists());
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn diagnostics_runs_and_reports_a_refused_instance_name() {
        run(arguments(&["--diagnostics"])).unwrap();
        let state = temporary_directory("diagnostics-invalid");
        let error =
            diagnostics_report_in(&serve_options(&["--instance-name", "../escape"]), &state)
                .unwrap_err();
        assert_eq!(error.code, loom_core::ErrorCode::InvalidRequest);
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn tls_resolution_requires_the_certificate_and_key_together() {
        assert!(resolve_tls(&ServerOptions::default()).unwrap().is_none());
        let only_certificate = ServerOptions {
            tls_cert: Some(temporary_path("cert.pem")),
            ..ServerOptions::default()
        };
        assert!(
            resolve_tls(&only_certificate)
                .unwrap_err()
                .message
                .contains("--tls-key")
        );
        let only_key = ServerOptions {
            tls_key: Some(temporary_path("key.pem")),
            ..ServerOptions::default()
        };
        assert!(
            resolve_tls(&only_key)
                .unwrap_err()
                .message
                .contains("--tls-cert")
        );
        let missing_files = ServerOptions {
            tls_cert: Some(temporary_path("absent-cert.pem")),
            tls_key: Some(temporary_path("absent-key.pem")),
            ..ServerOptions::default()
        };
        assert!(
            resolve_tls(&missing_files)
                .unwrap_err()
                .message
                .contains("could not read the TLS certificate file")
        );
    }

    #[tokio::test]
    async fn standalone_server_can_serve_tls_with_a_certificate_and_key() {
        let fixture = |name: &str| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("tls")
                .join(name)
        };
        let options = ServerOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Some("tls-server-token".to_owned()),
            tls_cert: Some(fixture("server-cert.pem")),
            tls_key: Some(fixture("server-key.pem")),
            ..ServerOptions::default()
        };
        let server = start(&options, InProcessBackend::new(), &instance(&options))
            .await
            .unwrap();
        assert!(server.websocket_url().starts_with("wss://127.0.0.1:"));
        assert_eq!(
            server.health_url(),
            format!("https://{}/health", server.local_addr())
        );
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn standalone_server_refuses_a_plaintext_non_loopback_bind_without_the_flag() {
        let options = ServerOptions {
            bind: "0.0.0.0:0".parse().unwrap(),
            token: Some("exposed-token".to_owned()),
            ..ServerOptions::default()
        };
        let error = start(&options, InProcessBackend::new(), &instance(&options))
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, loom_core::ErrorCode::InvalidRequest);
        assert!(error.message.contains("TLS"), "{error}");

        let opted_in = ServerOptions {
            allow_insecure_remote: true,
            ..options
        };
        let server = start(&opted_in, InProcessBackend::new(), &instance(&opted_in))
            .await
            .unwrap();
        assert!(!server.tls_enabled());
        assert!(server.websocket_url().starts_with("ws://0.0.0.0:"));
        server.stop().await.unwrap();
    }

    #[test]
    fn version_label_omits_an_unknown_or_empty_revision() {
        assert_eq!(version_label("0.1.0", "a1b2c3d"), "0.1.0 · a1b2c3d");
        assert_eq!(version_label("v0.8.1", "a1b2c3d"), "v0.8.1 · a1b2c3d");
        assert_eq!(version_label("0.1.0", "unknown"), "0.1.0");
        assert_eq!(version_label("0.1.0", ""), "0.1.0");
        assert_eq!(version_label("0.1.0", "   "), "0.1.0");
    }

    #[test]
    fn token_resolution_honours_the_explicit_flags() {
        let state = temporary_directory("flag-token");
        let flagged =
            prepare_instance(&serve_options(&["--token", "  flag-token  "]), &state).unwrap();
        assert_eq!(flagged.token().token(), "flag-token");
        assert_eq!(flagged.token().source(), &TokenSource::Flag);
        assert_eq!(flagged.token().printable(), None);

        let path = state.join("mounted-token");
        std::fs::write(&path, "  file-secret\n").unwrap();
        let served = prepare_instance(
            &serve_options(&["--token-file", path.to_str().unwrap()]),
            &state,
        )
        .unwrap();
        assert_eq!(served.token().token(), "file-secret");
        assert_eq!(served.token().source(), &TokenSource::File(path.clone()));

        let both = serve_options(&["--token", "flag-token", "--token-file", "/tmp/other"]);
        let error = prepare_instance(&both, &state).unwrap_err();
        assert!(error.message.contains("mutually exclusive"), "{error}");

        let empty = serve_options(&["--token", "   "]);
        let error = prepare_instance(&empty, &state).unwrap_err();
        assert!(error.message.contains("must not be empty"), "{error}");

        // An operator-named token file is never created for: a missing file is
        // reported as unreadable, and its directory is left to the deployment,
        // which may not even be writable by the user running the server.
        let deployment = state.join("deployment");
        let named_token = deployment.join("token");
        let missing = serve_options(&["--token-file", named_token.to_str().unwrap()]);
        let error = prepare_instance(&missing, &state).unwrap_err();
        assert!(
            error.message.contains("could not read token file"),
            "{error}"
        );
        assert!(
            !deployment.exists(),
            "an operator-named token directory must not be created"
        );
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn defaulted_start_uses_the_instance_directory_for_state_and_token() {
        let state = temporary_directory("defaults");
        let directory = state.join("127.0.0.1_8765");
        let options = serve_options(&["--bind", "127.0.0.1:8765"]);

        let first = prepare_instance(&options, &state).unwrap();
        assert_eq!(first.layout().directory(), Some(directory.as_path()));
        assert_eq!(first.state_db(), Some(directory.join("state.db").as_path()));
        assert_eq!(first.layout().token_file(), directory.join("token"));
        assert!(matches!(
            first.token().source(),
            TokenSource::GeneratedFile(path) if path == &directory.join("token")
        ));
        assert_eq!(first.token().printable(), Some(first.token().token()));

        // A second start on the same bind reuses the database and the token.
        let second = prepare_instance(&options, &state).unwrap();
        assert_eq!(second.state_db(), first.state_db());
        assert!(matches!(
            second.token().source(),
            TokenSource::ReusedFile(_)
        ));
        assert_eq!(second.token().token(), first.token().token());

        // A different port or host is a different instance.
        let other_port =
            prepare_instance(&serve_options(&["--bind", "127.0.0.1:8766"]), &state).unwrap();
        assert_ne!(other_port.state_db(), first.state_db());
        let other_host =
            prepare_instance(&serve_options(&["--bind", "0.0.0.0:8765"]), &state).unwrap();
        assert_ne!(other_host.state_db(), first.state_db());

        // --instance-name pins one directory for a host-aliased bind.
        let pinned = prepare_instance(
            &serve_options(&["--bind", "0.0.0.0:8765", "--instance-name", "worker"]),
            &state,
        )
        .unwrap();
        assert_eq!(
            pinned.layout().directory(),
            Some(state.join("worker").as_path())
        );
        assert_eq!(
            pinned.state_db(),
            Some(state.join("worker").join("state.db").as_path())
        );

        // An explicit --persistence still wins over the default.
        let explicit = prepare_instance(
            &serve_options(&[
                "--bind",
                "127.0.0.1:8765",
                "--persistence",
                "/tmp/explicit-loom.db",
            ]),
            &state,
        )
        .unwrap();
        assert_eq!(
            explicit.state_db(),
            Some(Path::new("/tmp/explicit-loom.db"))
        );
        std::fs::remove_dir_all(&state).ok();
    }

    #[tokio::test]
    async fn serve_in_keeps_an_ephemeral_port_in_memory_and_generates_a_token() {
        let state = temporary_directory("ephemeral");
        serve_in(
            serve_options(&["--bind", "127.0.0.1:0"]),
            &state,
            async { Ok(()) },
            never_reopen,
        )
        .await
        .unwrap();
        let token = std::fs::read_to_string(state.join("token")).unwrap();
        assert!(token.trim().starts_with("loom-"), "{token}");
        // An OS-assigned port has no stable identity, so no instance directory
        // is created and the backend stays in memory.
        assert!(!state.join("127.0.0.1_0").exists());
        std::fs::remove_dir_all(&state).ok();
    }

    #[tokio::test]
    async fn serve_in_sweeps_expired_archived_sessions_before_binding() {
        let state = temporary_directory("retention");
        let database = temporary_path("retention-state.db");
        let (workspace_id, session_id) = archived_session_in(&database);

        let options = ServerOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Some("retention-token".to_owned()),
            persistence: Some(database.clone()),
            archive_retention: Some("1ms".to_owned()),
            ..ServerOptions::default()
        };
        serve_in(options, &state, async { Ok(()) }, never_reopen)
            .await
            .unwrap();

        // The startup sweep ran before the listener accepted clients, so the
        // archived session is already gone from the durable database.
        let backend = InProcessBackend::new_persistent(&database).unwrap();
        let connection = backend.connect();
        assert_eq!(
            listed_sessions(&connection, workspace_id)
                .iter()
                .filter(|session| session.id == session_id)
                .count(),
            0
        );
        backend.shutdown().unwrap();
        std::fs::remove_file(&database).ok();
        std::fs::remove_file(database.with_extension("credentials.json")).ok();
        std::fs::remove_dir_all(database.with_extension("session-roots")).ok();
        std::fs::remove_dir_all(database.with_extension("clone-cache")).ok();
        std::fs::remove_dir_all(&state).ok();
    }

    /// Negotiates the capabilities these tests use and returns the workspace's
    /// sessions, including archived ones.
    fn listed_sessions(
        connection: &loom_server::InProcessConnection,
        workspace_id: loom_core::WorkspaceId,
    ) -> Vec<loom_core::AgentSessionSnapshot> {
        use loom_core::{Capability, CapabilitySet};
        use loom_protocol::{
            CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest, RequestEnvelope,
            ServerResponse, SessionResponse, WorkspaceRequest,
        };

        let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities: CapabilitySet::new([
                    Capability::ManageWorkspaces,
                    Capability::CreateAgentSession,
                    Capability::ControlAgentSession,
                    Capability::ReadAgentSession,
                ]),
            },
        )));
        assert!(negotiated.result.is_ok(), "{:?}", negotiated.result);
        match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id,
                    include_archived: true,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => sessions,
            response => panic!("unexpected session list response: {response:?}"),
        }
    }

    /// Creates one archived session in the durable database at `path`, the way a
    /// previous run would have left it.
    fn archived_session_in(path: &Path) -> (loom_core::WorkspaceId, loom_core::AgentSessionId) {
        use loom_core::{Capability, CapabilitySet};
        use loom_protocol::{
            CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest, RequestEnvelope,
            ServerResponse, SessionRequest, SessionResponse, WorkspaceRequest, WorkspaceResponse,
        };

        let backend = InProcessBackend::new_persistent(path).unwrap();
        let connection = backend.connect();
        let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities: CapabilitySet::new([
                    Capability::ManageWorkspaces,
                    Capability::CreateAgentSession,
                    Capability::ControlAgentSession,
                ]),
            },
        )));
        assert!(negotiated.result.is_ok(), "{:?}", negotiated.result);
        let workspace = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateWorkspace {
                    name: "Archive retention".to_owned(),
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
            response => panic!("unexpected workspace response: {response:?}"),
        };
        let session = match connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Archived".to_owned(),
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
            response => panic!("unexpected session response: {response:?}"),
        };
        let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::ArchiveAgentSession {
                session_id: session.id,
            },
        )));
        assert!(matches!(
            archived.result,
            Ok(ServerResponse::Session(
                SessionResponse::AgentSessionArchived(_)
            ))
        ));
        backend.shutdown().unwrap();
        (workspace.id, session.id)
    }

    #[test]
    fn reset_state_wipes_a_defaulted_database_and_needs_one_to_wipe() {
        let state = temporary_directory("reset");
        let database = state.join("127.0.0.1_8765").join("state.db");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        std::fs::write(
            &database,
            br#"{"schema_version":1,"state":{"broken":true}}"#,
        )
        .unwrap();

        // Without --reset-state the incompatible database is reported with the
        // flag that recovers from it, and it is left alone.
        let error =
            prepare_instance(&serve_options(&["--bind", "127.0.0.1:8765"]), &state).unwrap_err();
        assert!(error.message.contains("--reset-state"), "{error}");
        assert!(database.exists());

        let reset = prepare_instance(
            &serve_options(&["--bind", "127.0.0.1:8765", "--reset-state"]),
            &state,
        )
        .unwrap();
        assert!(!database.exists(), "--reset-state wipes the database");
        assert!(matches!(
            reset.token().source(),
            TokenSource::GeneratedFile(_)
        ));

        // An in-memory instance has nothing to reset.
        let error = prepare_instance(
            &serve_options(&["--bind", "127.0.0.1:0", "--reset-state"]),
            &state,
        )
        .unwrap_err();
        assert!(error.message.contains("--reset-state"), "{error}");
        std::fs::remove_dir_all(&state).ok();
    }

    #[tokio::test]
    async fn an_invalid_node_name_is_rejected_before_state_is_prepared() {
        let state = temporary_directory("invalid-node-name");
        let database = state.join("127.0.0.1_8765").join("state.db");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        std::fs::write(
            &database,
            br#"{"schema_version":1,"state":{"broken":true}}"#,
        )
        .unwrap();

        // Preparation would wipe this incompatible database under
        // `--reset-state` and generate the token file, so a rejected node name
        // must fail before either side effect.
        let error = serve_in(
            serve_options(&["--bind", "127.0.0.1:8765", "--reset-state", "--node-name="]),
            &state,
            async { Ok(()) },
            never_reopen,
        )
        .await
        .unwrap_err();

        assert!(error.message.contains("--node-name"), "{error}");
        assert!(
            database.exists(),
            "an invalid node name must not reset the database"
        );
        assert!(
            !state.join("token").exists(),
            "an invalid node name must not generate a token"
        );
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn bind_exposure_warning_flags_plaintext_non_loopback_addresses_only() {
        for local in ["127.0.0.1:8765", "[::1]:8765"] {
            let address = local.parse().unwrap();
            assert_eq!(
                bind_exposure_warning(address, false, false),
                None,
                "{local}"
            );
            assert_eq!(bind_exposure_warning(address, false, true), None, "{local}");
            assert_eq!(bind_exposure_warning(address, true, false), None, "{local}");
        }
        for exposed in ["0.0.0.0:8765", "192.0.2.10:8765", "[::]:8765"] {
            let address = exposed.parse().unwrap();
            // The library refuses this bind, so no warning is emitted and the
            // message never claims the opt-in was given when it was not.
            assert_eq!(
                bind_exposure_warning(address, false, false),
                None,
                "{exposed}"
            );
            let warning = bind_exposure_warning(address, false, true)
                .unwrap_or_else(|| panic!("{exposed} should warn"));
            assert!(warning.contains("without TLS"), "{warning}");
            assert!(warning.contains("--allow-insecure-remote"), "{warning}");
            assert_eq!(
                bind_exposure_warning(address, true, true),
                None,
                "{exposed}"
            );
        }
    }

    async fn health_body(url: &str) -> String {
        let address = url.strip_prefix("http://").expect("health url is http");
        let (address, path) = address.split_once('/').expect("health url has a path");
        let address = address.to_owned();
        let request =
            format!("GET /{path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
        tokio::task::spawn_blocking(move || {
            let mut stream = std::net::TcpStream::connect(&address).expect("connect to health");
            stream
                .write_all(request.as_bytes())
                .expect("write health request");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("read health response");
            response
        })
        .await
        .expect("health request task")
    }

    #[tokio::test]
    async fn standalone_server_binds_reports_urls_serves_health_and_stops() {
        let options = ServerOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Some("server-test-token".to_owned()),
            ..ServerOptions::default()
        };
        let server = start(&options, InProcessBackend::new(), &instance(&options))
            .await
            .unwrap();
        assert!(server.websocket_url().starts_with("ws://127.0.0.1:"));
        assert!(server.websocket_url().ends_with("/ws"));
        assert_eq!(
            server.health_url(),
            format!("http://{}/health", server.local_addr())
        );

        let response = health_body(&server.health_url()).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("ok"), "{response}");

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn serve_opens_the_in_memory_backend_and_stops_on_shutdown() {
        let state = temporary_directory("in-memory");
        let options = ServerOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Some("in-memory-token".to_owned()),
            ..ServerOptions::default()
        };
        serve_in(options, &state, async { Ok(()) }, never_reopen)
            .await
            .unwrap();
        std::fs::remove_dir_all(&state).ok();
    }

    #[tokio::test]
    async fn serve_opens_a_persistent_backend_at_the_requested_path() {
        let state = temporary_directory("persistent");
        let path = temporary_path("state.db");
        let options = ServerOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Some("persistent-token".to_owned()),
            persistence: Some(path.clone()),
            ..ServerOptions::default()
        };
        serve_in(options, &state, async { Ok(()) }, never_reopen)
            .await
            .unwrap();
        assert!(
            path.exists(),
            "the persistent backend should create {path:?}"
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(path.with_extension("credentials.json")).ok();
        std::fs::remove_file(path.with_extension("session-roots")).ok();
        std::fs::remove_dir_all(&state).ok();
    }

    #[tokio::test]
    async fn the_production_reopen_watches_a_serving_backend_without_firing() {
        let backend = InProcessBackend::new();
        let record = temporary_path("reopen-watch");

        let reopened = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            fail_stop_reopen(backend, Some(record.clone())),
        )
        .await;

        assert!(reopened.is_err(), "a serving backend must not be reopened");
        assert!(
            !record.exists(),
            "a serving backend records no fail-stop beside its database"
        );
    }

    #[tokio::test]
    async fn serve_exits_a_fail_stopped_backend_after_releasing_its_database() {
        let state = temporary_directory("fail-stop-exit");
        let database = temporary_path("fail-stop-exit.db");
        let options = ServerOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Some("fail-stop-token".to_owned()),
            persistence: Some(database.clone()),
            ..ServerOptions::default()
        };

        let error = serve_in(
            options,
            &state,
            // A latched fail-stop wins over a process that is asked to keep
            // running, which is what the supervisor stands in for here.
            std::future::pending::<Result<(), loom_core::LoomError>>(),
            |_, record| {
                assert_eq!(
                    record,
                    Some(fail_stop_record_path(&database)),
                    "the fail-stop is recorded beside the state database it failed to save"
                );
                Box::pin(async {
                    loom_core::LoomError::new(
                        loom_core::ErrorCode::Persistence,
                        "test persistence fail-stop",
                        true,
                    )
                })
            },
        )
        .await
        .expect_err("a fail-stopped server must report the failure as a process exit");

        assert_eq!(error.code, loom_core::ErrorCode::Persistence);
        // The database was released before the exit, so the reopened process can
        // take exclusive ownership of it.
        let reopened = InProcessBackend::new_persistent(&database)
            .expect("a fail-stopped server must release the database before exiting");
        reopened.shutdown().unwrap();
        std::fs::remove_file(&database).ok();
        std::fs::remove_file(database.with_extension("credentials.json")).ok();
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn backend_selection_reports_errors_for_an_unusable_persistence_path() {
        let directory = temporary_path("state-dir");
        std::fs::create_dir_all(&directory).unwrap();
        assert!(open_backend(Some(&directory)).is_err());
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn a_failed_shutdown_wait_is_reported_as_an_internal_error() {
        let error = super::shutdown_signal_error(std::io::Error::other("no signal"));
        assert!(error.message.contains("could not wait for shutdown"));
        assert_eq!(error.code, loom_core::ErrorCode::Internal);
        assert!(!error.retryable);
    }
}
