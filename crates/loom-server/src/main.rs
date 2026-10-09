//! The standalone `loom-server` executable.
//!
//! It serves the remote JSON WebSocket backend so native clients and browser
//! clients can use one Loom worker, or so a backend can be kept out of the
//! desktop client process entirely. The transport is plain `ws://` guarded by a
//! single bearer token, so the default bind address is loopback; see
//! `SECURITY.md` before exposing it further.

use std::{
    env, fs,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use loom_core::{ErrorCode, LoomError};
use loom_server::{
    AuthTokenStore, AuthorizationScope, InProcessBackend, RemoteServer, RemoteServerConfig,
    RunningRemoteServer,
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
    token: Option<String>,
    token_file: Option<PathBuf>,
    persistence: Option<PathBuf>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("valid default bind address"),
            token: None,
            token_file: None,
            persistence: None,
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
            "--token-file" => {
                options.token_file =
                    Some(PathBuf::from(required_value(&mut args, "--token-file")?));
            }
            "--persistence" => {
                options.persistence =
                    Some(PathBuf::from(required_value(&mut args, "--persistence")?));
            }
            "--help" | "-h" => return Ok(ParsedArgs::Help),
            "--version" | "-V" => return Ok(ParsedArgs::Version),
            value if value.starts_with("--bind=") => {
                options.bind = parse_bind(value["--bind=".len()..].to_owned())?;
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

/// Resolves the single bearer token the server accepts. Exactly one of
/// `--token` and `--token-file` must be supplied, and the result must not be
/// empty.
fn resolve_token(options: &ServerOptions) -> Result<String, LoomError> {
    match (&options.token, &options.token_file) {
        (Some(_), Some(_)) => Err(LoomError::invalid_request(
            "--token and --token-file are mutually exclusive; provide exactly one",
        )),
        (None, None) => Err(LoomError::invalid_request(
            "provide a client bearer token with --token or --token-file",
        )),
        (Some(token), None) => {
            let token = token.trim();
            if token.is_empty() {
                return Err(LoomError::invalid_request("--token must not be empty"));
            }
            Ok(token.to_owned())
        }
        (None, Some(path)) => {
            let contents = fs::read_to_string(path).map_err(|error| {
                LoomError::invalid_request(format!(
                    "could not read token file '{}': {error}",
                    path.display()
                ))
            })?;
            let token = contents.trim();
            if token.is_empty() {
                return Err(LoomError::invalid_request(format!(
                    "token file '{}' is empty",
                    path.display()
                )));
            }
            Ok(token.to_owned())
        }
    }
}

fn help_text() -> String {
    format!(
        "Usage: loom-server [--bind <addr>] [--token <token> | --token-file <path>] \
         [--persistence <path>]\n\
         \n\
         A standalone Loom backend that serves the remote WebSocket protocol.\n\
         \n\
         Options:\n\
         \x20 --bind <addr>         address to listen on (default {DEFAULT_BIND})\n\
         \x20 --token <token>       bearer token clients must present\n\
         \x20 --token-file <path>   file whose trimmed contents are the bearer token\n\
         \x20 --persistence <path>  keep state in the database at this path\n\
         \x20 -h, --help            print this help and exit\n\
         \x20 -V, --version         print the version and exit\n\
         \n\
         Exactly one of --token and --token-file is required. Without --persistence \
         the backend keeps its state in memory for the lifetime of the process."
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

/// Opens the backend the operator asked for: a `--persistence` path selects the
/// durable backend, and without one the backend keeps its state in memory. This
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
    /// The `ws://` URL clients connect to.
    fn websocket_url(&self) -> &str {
        self.remote.websocket_url()
    }

    /// The plain HTTP URL of the unauthenticated health endpoint.
    fn health_url(&self) -> String {
        format!("http://{}/health", self.remote.local_addr())
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
) -> Result<RunningServer, LoomError> {
    let token = resolve_token(options)?;
    let auth = Arc::new(AuthTokenStore::new());
    let _issued = auth.insert(token, AuthorizationScope::all())?;
    let config = RemoteServerConfig {
        bind_addr: options.bind,
        ..RemoteServerConfig::default()
    };
    if let Some(warning) = bind_exposure_warning(config.bind_addr) {
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

/// A warning when `--bind` publishes the backend beyond the local machine. The
/// backend speaks plain `ws://` with a single shared bearer token, so it is not
/// safe to expose to an untrusted network; this warns without refusing to run.
fn bind_exposure_warning(bind: SocketAddr) -> Option<String> {
    if bind.ip().is_loopback() {
        return None;
    }
    Some(format!(
        "loom-server is bound to {bind}, which is not a loopback address; the standalone \
         backend speaks plain ws:// and must not be exposed to an untrusted network"
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
    let backend = build_backend(options.persistence.as_deref())?;
    let server = start(&options, backend).await?;
    shutdown.await?;
    log::info!("Shutting down loom-server");
    server.stop().await
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

    use loom_server::InProcessBackend;

    use super::{
        ParsedArgs, ServerOptions, bind_exposure_warning, build_version, help_text, parse_args,
        resolve_token, run, serve, start, version_label, version_text,
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

    #[test]
    fn parser_keeps_defaults_and_accepts_spaced_and_equals_values() {
        let defaults = serve_options(&[]);
        assert_eq!(defaults.bind.to_string(), "127.0.0.1:8765");
        assert_eq!(defaults.token, None);
        assert_eq!(defaults.token_file, None);
        assert_eq!(defaults.persistence, None);

        let spaced = serve_options(&[
            "--bind",
            "127.0.0.1:9000",
            "--token",
            "spaced-token",
            "--persistence",
            "/tmp/loom.db",
        ]);
        assert_eq!(spaced.bind.to_string(), "127.0.0.1:9000");
        assert_eq!(spaced.token.as_deref(), Some("spaced-token"));
        assert_eq!(
            spaced.persistence.as_deref(),
            Some(Path::new("/tmp/loom.db"))
        );

        let equals = serve_options(&[
            "--bind=0.0.0.0:9",
            "--token-file=/tmp/token",
            "--persistence=/tmp/other.db",
        ]);
        assert_eq!(equals.bind.to_string(), "0.0.0.0:9");
        assert_eq!(equals.token, None);
        assert_eq!(equals.token_file.as_deref(), Some(Path::new("/tmp/token")));
        assert_eq!(
            equals.persistence.as_deref(),
            Some(Path::new("/tmp/other.db"))
        );
    }

    #[test]
    fn parser_rejects_missing_values_invalid_binds_and_unknown_arguments() {
        for flag in ["--bind", "--token", "--token-file", "--persistence"] {
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
        assert!(help.contains("--token-file <path>"));
        assert!(help.contains("127.0.0.1:8765"));
        assert!(version_text().starts_with("loom-server "));
        assert!(version_text().contains(build_version()));
        assert!(!build_version().is_empty());
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
    fn token_resolution_requires_exactly_one_source() {
        let neither = resolve_token(&ServerOptions::default()).unwrap_err();
        assert!(neither.message.contains("--token or --token-file"));

        let spaced = ServerOptions {
            token: Some("  flag-token  ".to_owned()),
            ..ServerOptions::default()
        };
        assert_eq!(resolve_token(&spaced).unwrap(), "flag-token");

        let both = ServerOptions {
            token: Some("flag-token".to_owned()),
            token_file: Some(temporary_path("both.token")),
            ..ServerOptions::default()
        };
        assert!(
            resolve_token(&both)
                .unwrap_err()
                .message
                .contains("mutually exclusive")
        );

        let empty = ServerOptions {
            token: Some("   ".to_owned()),
            ..ServerOptions::default()
        };
        assert!(
            resolve_token(&empty)
                .unwrap_err()
                .message
                .contains("must not be empty")
        );
    }

    #[test]
    fn token_resolution_reads_and_trims_the_token_file() {
        let path = temporary_path("token");
        std::fs::write(&path, "  file-secret\n").unwrap();
        let options = ServerOptions {
            token_file: Some(path.clone()),
            ..ServerOptions::default()
        };
        assert_eq!(resolve_token(&options).unwrap(), "file-secret");

        std::fs::write(&path, "\n   \n").unwrap();
        assert!(
            resolve_token(&options)
                .unwrap_err()
                .message
                .contains("is empty")
        );

        std::fs::remove_file(&path).unwrap();
        assert!(
            resolve_token(&options)
                .unwrap_err()
                .message
                .contains("could not read token file")
        );
    }

    #[test]
    fn bind_exposure_warning_flags_non_loopback_addresses_only() {
        for local in ["127.0.0.1:8765", "[::1]:8765"] {
            assert_eq!(
                bind_exposure_warning(local.parse().unwrap()),
                None,
                "{local}"
            );
        }
        for exposed in ["0.0.0.0:8765", "192.0.2.10:8765", "[::]:8765"] {
            let warning = bind_exposure_warning(exposed.parse().unwrap())
                .unwrap_or_else(|| panic!("{exposed} should warn"));
            assert!(warning.contains("not a loopback address"), "{warning}");
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
            token_file: None,
            persistence: None,
        };
        let server = start(&options, InProcessBackend::new()).await.unwrap();
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
        let options = ServerOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Some("in-memory-token".to_owned()),
            ..ServerOptions::default()
        };
        serve(options, async { Ok(()) }).await.unwrap();
    }

    #[tokio::test]
    async fn serve_opens_a_persistent_backend_at_the_requested_path() {
        let path = temporary_path("state.db");
        let options = ServerOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Some("persistent-token".to_owned()),
            persistence: Some(path.clone()),
            ..ServerOptions::default()
        };
        serve(options, async { Ok(()) }).await.unwrap();
        assert!(
            path.exists(),
            "the persistent backend should create {path:?}"
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(path.with_extension("credentials.json")).ok();
        std::fs::remove_file(path.with_extension("session-roots")).ok();
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
