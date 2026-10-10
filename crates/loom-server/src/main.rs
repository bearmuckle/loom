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
    sync::Arc,
};

use loom_core::{ErrorCode, LoomError};
use loom_server::{
    AuthTokenStore, AuthorizationScope, InProcessBackend, Instance, InstanceOptions, RemoteServer,
    RemoteServerConfig, RunningRemoteServer, ServerTlsConfig,
};

/// The bind address used when `--bind` is not supplied.
const DEFAULT_BIND: &str = "127.0.0.1:8765";

fn main() -> std::process::ExitCode {
    init_logging();
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
    /// Serve the standalone backend.
    Serve(ServerOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ServerOptions {
    bind: SocketAddr,
    instance_name: Option<String>,
    token: Option<String>,
    token_file: Option<PathBuf>,
    persistence: Option<PathBuf>,
    reset_state: bool,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    allow_insecure_remote: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("valid default bind address"),
            instance_name: None,
            token: None,
            token_file: None,
            persistence: None,
            reset_state: false,
            tls_cert: None,
            tls_key: None,
            allow_insecure_remote: false,
        }
    }
}

impl ServerOptions {
    /// The settings [`Instance`] applies its defaults to.
    fn instance_options(&self) -> InstanceOptions {
        InstanceOptions {
            bind: self.bind,
            instance_name: self.instance_name.clone(),
            persistence: self.persistence.clone(),
            token: self.token.clone(),
            token_file: self.token_file.clone(),
            reset_state: self.reset_state,
        }
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
        ParsedArgs::Serve(options) => serve_blocking(options),
    }
}

/// Parses the command line. Both `--flag value` and `--flag=value` are
/// accepted; `--help` and `--version` short-circuit the remaining arguments.
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<ParsedArgs, LoomError> {
    let mut options = ServerOptions::default();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--bind" => options.bind = parse_bind(required_value(&mut args, "--bind")?)?,
            "--token" => options.token = Some(required_value(&mut args, "--token")?),
            "--instance-name" => {
                options.instance_name = Some(required_value(&mut args, "--instance-name")?);
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
            "--tls-cert" => {
                options.tls_cert = Some(PathBuf::from(required_value(&mut args, "--tls-cert")?));
            }
            "--tls-key" => {
                options.tls_key = Some(PathBuf::from(required_value(&mut args, "--tls-key")?));
            }
            "--allow-insecure-remote" => options.allow_insecure_remote = true,
            "--help" | "-h" => return Ok(ParsedArgs::Help),
            "--version" | "-V" => return Ok(ParsedArgs::Version),
            value if value.starts_with("--bind=") => {
                options.bind = parse_bind(value["--bind=".len()..].to_owned())?;
            }
            value if value.starts_with("--instance-name=") => {
                options.instance_name = Some(value["--instance-name=".len()..].to_owned());
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
            value if value.starts_with("--tls-cert=") => {
                options.tls_cert = Some(PathBuf::from(&value["--tls-cert=".len()..]));
            }
            value if value.starts_with("--tls-key=") => {
                options.tls_key = Some(PathBuf::from(&value["--tls-key=".len()..]));
            }
            unknown => {
                return Err(LoomError::invalid_request(format!(
                    "unknown argument '{unknown}'; use --help for usage"
                )));
            }
        }
    }
    Ok(ParsedArgs::Serve(options))
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
         [--token <token> | --token-file <path>] [--persistence <path>] \
         [--reset-state] [--tls-cert <pem> --tls-key <pem>] \
         [--allow-insecure-remote]\n\
         \n\
         A standalone Loom backend that serves the remote WebSocket protocol.\n\
         \n\
         Options:\n\
         \x20 --bind <addr>         address to listen on (default {DEFAULT_BIND})\n\
         \x20 --instance-name <name>\n\
         \x20                        pin this instance's directory instead of naming it \
         after --bind\n\
         \x20 --token <token>       bearer token clients must present\n\
         \x20 --token-file <path>   file whose trimmed contents are the bearer token\n\
         \x20 --persistence <path>  keep state in the database at this path\n\
         \x20 --reset-state         wipe an incompatible state database and start empty\n\
         \x20 --tls-cert <pem>      serve wss:// with this PEM certificate chain\n\
         \x20 --tls-key <pem>       PEM private key matching --tls-cert\n\
         \x20 --allow-insecure-remote\n\
         \x20                        allow a plaintext ws:// bind beyond loopback\n\
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
         A bind address that is not loopback needs TLS; --allow-insecure-remote accepts \
         the risk of sending the bearer token and all traffic in plaintext instead."
    )
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

/// Opens the backend the operator asked for: the path [`Instance`] resolved, or
/// the in-memory backend when this instance has no durable database. This
/// mirrors the standalone server mode of the native `loom` shell.
fn build_backend(persistence: Option<&Path>) -> Result<Arc<InProcessBackend>, LoomError> {
    match persistence {
        Some(path) => InProcessBackend::new_persistent_with_github_copilot(path),
        None => InProcessBackend::new_with_github_copilot(),
    }
}

/// The running standalone backend. It is owned by the caller so a test can stop
/// it without raising a process signal.
struct RunningServer {
    remote: RunningRemoteServer,
    backend: Arc<InProcessBackend>,
}

impl RunningServer {
    /// The WebSocket URL clients connect to, using the scheme the listener
    /// serves (`ws://` or `wss://`).
    fn websocket_url(&self) -> &str {
        self.remote.websocket_url()
    }

    /// The health endpoint URL of the listener.
    fn health_url(&self) -> String {
        self.remote.health_url()
    }

    /// Stops accepting connections and shuts the backend down.
    async fn stop(self) -> Result<(), LoomError> {
        self.remote.stop().await?;
        self.backend.shutdown()
    }
}

/// Binds the backend and returns the running server.
async fn start(
    options: &ServerOptions,
    backend: Arc<InProcessBackend>,
    instance: &Instance,
) -> Result<RunningServer, LoomError> {
    let tls = resolve_tls(options)?;
    let tls_configured = tls.is_some();
    let auth = Arc::new(AuthTokenStore::new());
    let _issued = auth.insert(instance.token().token(), AuthorizationScope::all())?;
    let config = RemoteServerConfig {
        bind_addr: options.bind,
        tls,
        allow_insecure_remote: options.allow_insecure_remote,
        ..RemoteServerConfig::default()
    };
    if let Some(warning) = bind_exposure_warning(
        config.bind_addr,
        tls_configured,
        options.allow_insecure_remote,
    ) {
        log::warn!("{warning}");
    }
    let remote = RemoteServer::new(backend.clone(), auth, config)
        .bind()
        .await?;
    let server = RunningServer { remote, backend };
    log::info!(
        "Loom remote backend listening at {}",
        server.websocket_url()
    );
    log::info!("Health endpoint: {}", server.health_url());
    Ok(server)
}

/// A warning when `--bind` publishes the backend beyond the local machine
/// without TLS. The listener then speaks plain `ws://` with a single shared
/// bearer token, which the library only allows because the operator passed
/// `--allow-insecure-remote`; this warns loudly so it cannot happen quietly.
/// Without that opt-in the bind is refused, so there is nothing to warn about
/// and the message must not claim a flag that was never given.
fn bind_exposure_warning(
    bind: SocketAddr,
    tls_configured: bool,
    allow_insecure_remote: bool,
) -> Option<String> {
    if bind.ip().is_loopback() || tls_configured || !allow_insecure_remote {
        return None;
    }
    Some(format!(
        "loom-server is bound to {bind} without TLS: because --allow-insecure-remote was given, \
         the bearer token and every protocol frame are sent in plaintext; do not expose this \
         listener to an untrusted network"
    ))
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
    serve_in(options, &loom_core::state_dir(), shutdown).await
}

/// [`serve`] with the state directory injected, so a test can drive the whole
/// lifecycle without writing into the operator's real state directory.
async fn serve_in(
    options: ServerOptions,
    state_directory: &Path,
    shutdown: impl Future<Output = Result<(), LoomError>>,
) -> Result<(), LoomError> {
    let instance = prepare_instance(&options, state_directory)?;
    let backend = build_backend(instance.state_db())?;
    let server = start(&options, backend, &instance).await?;
    shutdown.await?;
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
    let instance = Instance::prepare(&options.instance_options(), state_directory)?;
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

    use loom_server::{InProcessBackend, Instance, TokenSource};

    use super::{
        ParsedArgs, ServerOptions, bind_exposure_warning, build_version, help_text, parse_args,
        prepare_instance, resolve_tls, run, serve_in, start, version_label, version_text,
    };

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

    #[test]
    fn parser_keeps_defaults_and_accepts_spaced_and_equals_values() {
        let defaults = serve_options(&[]);
        assert_eq!(defaults.bind.to_string(), "127.0.0.1:8765");
        assert_eq!(defaults.instance_name, None);
        assert_eq!(defaults.token, None);
        assert_eq!(defaults.token_file, None);
        assert_eq!(defaults.persistence, None);
        assert!(!defaults.reset_state);
        assert_eq!(defaults.tls_cert, None);
        assert_eq!(defaults.tls_key, None);
        assert!(!defaults.allow_insecure_remote);

        let spaced = serve_options(&[
            "--bind",
            "127.0.0.1:9000",
            "--instance-name",
            "worker",
            "--token",
            "spaced-token",
            "--persistence",
            "/tmp/loom.db",
            "--reset-state",
            "--tls-cert",
            "/tmp/cert.pem",
            "--tls-key",
            "/tmp/key.pem",
            "--allow-insecure-remote",
        ]);
        assert_eq!(spaced.bind.to_string(), "127.0.0.1:9000");
        assert_eq!(spaced.instance_name.as_deref(), Some("worker"));
        assert!(spaced.reset_state);
        assert_eq!(spaced.token.as_deref(), Some("spaced-token"));
        assert_eq!(
            spaced.persistence.as_deref(),
            Some(Path::new("/tmp/loom.db"))
        );
        assert_eq!(spaced.tls_cert.as_deref(), Some(Path::new("/tmp/cert.pem")));
        assert_eq!(spaced.tls_key.as_deref(), Some(Path::new("/tmp/key.pem")));
        assert!(spaced.allow_insecure_remote);

        let equals = serve_options(&[
            "--bind=0.0.0.0:9",
            "--instance-name=pinned",
            "--token-file=/tmp/token",
            "--persistence=/tmp/other.db",
            "--tls-cert=/tmp/other-cert.pem",
            "--tls-key=/tmp/other-key.pem",
        ]);
        assert_eq!(equals.bind.to_string(), "0.0.0.0:9");
        assert_eq!(equals.instance_name.as_deref(), Some("pinned"));
        assert_eq!(equals.token, None);
        assert_eq!(equals.token_file.as_deref(), Some(Path::new("/tmp/token")));
        assert_eq!(
            equals.persistence.as_deref(),
            Some(Path::new("/tmp/other.db"))
        );
        assert_eq!(
            equals.tls_cert.as_deref(),
            Some(Path::new("/tmp/other-cert.pem"))
        );
        assert_eq!(
            equals.tls_key.as_deref(),
            Some(Path::new("/tmp/other-key.pem"))
        );
        assert!(!equals.allow_insecure_remote);
    }

    #[test]
    fn parser_rejects_missing_values_invalid_binds_and_unknown_arguments() {
        for flag in [
            "--bind",
            "--instance-name",
            "--token",
            "--token-file",
            "--persistence",
            "--tls-cert",
            "--tls-key",
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
        assert!(help.contains("--reset-state"));
        assert!(help.contains("--token-file <path>"));
        assert!(help.contains("--tls-cert <pem>"));
        assert!(help.contains("--tls-key <pem>"));
        assert!(help.contains("--allow-insecure-remote"));
        assert!(help.contains("127.0.0.1:8765"));
        assert!(version_text().starts_with("loom-server "));
        assert!(version_text().contains(build_version()));
        assert!(!build_version().is_empty());
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
            format!("https://{}/health", server.remote.local_addr())
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
        assert!(!server.remote.tls_enabled());
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
        serve_in(serve_options(&["--bind", "127.0.0.1:0"]), &state, async {
            Ok(())
        })
        .await
        .unwrap();
        let token = std::fs::read_to_string(state.join("token")).unwrap();
        assert!(token.trim().starts_with("loom-"), "{token}");
        // An OS-assigned port has no stable identity, so no instance directory
        // is created and the backend stays in memory.
        assert!(!state.join("127.0.0.1_0").exists());
        std::fs::remove_dir_all(&state).ok();
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
            format!("http://{}/health", server.remote.local_addr())
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
        serve_in(options, &state, async { Ok(()) }).await.unwrap();
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
        serve_in(options, &state, async { Ok(()) }).await.unwrap();
        assert!(
            path.exists(),
            "the persistent backend should create {path:?}"
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(path.with_extension("credentials.json")).ok();
        std::fs::remove_file(path.with_extension("session-roots")).ok();
        std::fs::remove_dir_all(&state).ok();
    }

    #[test]
    fn backend_selection_reports_errors_for_an_unusable_persistence_path() {
        let directory = temporary_path("state-dir");
        std::fs::create_dir_all(&directory).unwrap();
        assert!(super::build_backend(Some(&directory)).is_err());
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
