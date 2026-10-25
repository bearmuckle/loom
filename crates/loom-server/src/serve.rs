//! The shared startup path for a remote backend.
//!
//! The standalone `loom-server` binary and the native shell's `--serve` mode
//! stand up the same backend, so the decisions they both need live here: how the
//! durable state database is prepared, which backend is opened for it, and how
//! the listener is bound, warned about, and reported. The two binaries keep
//! their own command lines and their own reset policy: the standalone server
//! fails closed unless `--reset-state` was given, while the shell can ask
//! interactively through [`ensure_state_database`].

use std::{
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::Path,
    sync::Arc,
};

use loom_core::{ErrorCode, LoomError};
use loom_persistence::SchemaStatus;

use crate::{
    AuthTokenStore, AuthorizationScope, GitHubSignInConfig, InProcessBackend, RemoteServer,
    RemoteServerConfig, RunningRemoteServer,
};

/// Ensures the state database at `path` can be opened by this build. An
/// incompatible database is wiped only when the operator asked for it
/// (`reset_requested`) or explicitly confirms the interactive prompt; otherwise
/// the error is returned.
pub fn ensure_state_database(path: &Path, reset_requested: bool) -> Result<(), LoomError> {
    ensure_state_database_with(path, reset_requested, confirm_state_wipe)
}

/// The decision logic for [`ensure_state_database`] with the interactive
/// confirmation injected, so a caller can express its own reset policy — the
/// standalone server passes a policy that never wipes without `--reset-state` —
/// and both can be exercised without a terminal.
pub fn ensure_state_database_with(
    path: &Path,
    reset_requested: bool,
    confirm: impl FnOnce(&Path, SchemaStatus) -> Result<bool, LoomError>,
) -> Result<(), LoomError> {
    let status = loom_persistence::FilePersistence::schema_status(path)?;
    if status.is_compatible() {
        return Ok(());
    }
    let wipe = reset_requested || confirm(path, status)?;
    if !wipe {
        return Err(loom_persistence::incompatible_database_error(path, status));
    }
    loom_persistence::FilePersistence::reset_database(path)
}

/// The interactive confirmation of [`ensure_state_database`]: an incompatible
/// database is wiped only when the operator answers yes, and never when stdin is
/// not a terminal, so a service manager is not left waiting for an answer.
fn confirm_state_wipe(path: &Path, status: SchemaStatus) -> Result<bool, LoomError> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    eprintln!(
        "Loom state database '{}' uses {} and cannot be opened by this build.",
        path.display(),
        status.description()
    );
    eprint!("Wipe it and start with an empty database? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|error| LoomError::new(ErrorCode::Internal, format!("{error}"), true))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| LoomError::new(ErrorCode::Internal, format!("{error}"), true))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Which provider registry a started backend serves: the configured registry an
/// operator sets up from the Providers dialog or the environment, or the demo
/// registry that needs no credential at all.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProviderRegistryChoice {
    /// OpenAI, DeepSeek, GitHub Copilot, the optional Ollama runtime, and the
    /// optional OpenAI-compatible gateway the environment registers.
    #[default]
    Configured,
    /// `deterministic/demo` with the local OpenAI-compatible demo model, for
    /// tests and local automation rather than an exposed production worker.
    Demo,
}

impl ProviderRegistryChoice {
    /// The word `--providers` accepts for the configured registry.
    pub const CONFIGURED: &'static str = "configured";
    /// The word `--providers` accepts for the demo registry.
    pub const DEMO: &'static str = "demo";

    /// Parses one `--providers` value. Anything but the two accepted words is
    /// refused, and the message names the flag and both accepted values so an
    /// operator can correct the command line without reading the help text.
    pub fn parse(value: &str) -> Result<Self, LoomError> {
        match value {
            Self::CONFIGURED => Ok(Self::Configured),
            Self::DEMO => Ok(Self::Demo),
            other => Err(LoomError::invalid_request(format!(
                "--providers must be '{}' or '{}', not '{other}'",
                Self::CONFIGURED,
                Self::DEMO
            ))),
        }
    }

    /// The word this choice is written as, so the startup log line carries the
    /// profile name and nothing else.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Configured => Self::CONFIGURED,
            Self::Demo => Self::DEMO,
        }
    }
}

/// Opens the backend the operator asked for: the durable database at
/// `persistence`, or the in-memory backend when this instance has no durable
/// database. Both binaries serve the backend this returns.
pub fn open_backend(persistence: Option<&Path>) -> Result<Arc<InProcessBackend>, LoomError> {
    open_backend_named(persistence, None)
}

/// [`open_backend`] with the operator-set node name clients should show.
///
/// `node_name` is a value [`crate::resolve_node_name`] accepted; `None` keeps the
/// derived `hostname · short-id` display name. The name is display-only, so it
/// changes no persisted state and no protocol field besides
/// `WorkerNodeStatus.name`.
pub fn open_backend_named(
    persistence: Option<&Path>,
    node_name: Option<&str>,
) -> Result<Arc<InProcessBackend>, LoomError> {
    open_backend_named_with(ProviderRegistryChoice::Configured, persistence, node_name)
}

/// [`open_backend_named`] with the provider registry profile `--providers`
/// selects. The selected profile is logged once here, as just its word, so an
/// operator can tell which registry a worker serves without any endpoint or key
/// reaching a log line.
pub fn open_backend_named_with(
    choice: ProviderRegistryChoice,
    persistence: Option<&Path>,
    node_name: Option<&str>,
) -> Result<Arc<InProcessBackend>, LoomError> {
    log::info!("Provider registry: {}", choice.as_str());
    match (choice, persistence) {
        (ProviderRegistryChoice::Configured, Some(path)) => {
            InProcessBackend::new_persistent_with_github_copilot_named(path, node_name)
        }
        (ProviderRegistryChoice::Configured, None) => {
            InProcessBackend::new_with_github_copilot_named(node_name)
        }
        (ProviderRegistryChoice::Demo, Some(path)) => {
            InProcessBackend::new_persistent_with_demo_named(path, node_name)
        }
        (ProviderRegistryChoice::Demo, None) => InProcessBackend::new_with_demo_named(node_name),
    }
}

/// A bound remote listener together with the backend it serves. It is owned by
/// the caller so it can be stopped without raising a process signal.
pub struct ServingBackend {
    remote: RunningRemoteServer,
    backend: Arc<InProcessBackend>,
}

impl ServingBackend {
    /// The WebSocket URL clients connect to, using the scheme the listener
    /// serves (`ws://` or `wss://`).
    pub fn websocket_url(&self) -> &str {
        self.remote.websocket_url()
    }

    /// The health endpoint URL of the listener.
    pub fn health_url(&self) -> String {
        self.remote.health_url()
    }

    /// The address the listener is bound to, which carries the OS-assigned port
    /// when the configured one was 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.remote.local_addr()
    }

    /// Whether the listener serves TLS.
    pub fn tls_enabled(&self) -> bool {
        self.remote.tls_enabled()
    }

    /// Stops accepting connections and shuts the backend down.
    pub async fn stop(self) -> Result<(), LoomError> {
        self.remote.stop().await?;
        self.backend.shutdown()
    }
}

/// Binds `backend` as a remote WebSocket server that accepts only `token`, warns
/// when `config` publishes it beyond the local machine without TLS, and logs the
/// URLs clients use.
///
/// `config.github_sign_in` decides whether a WebSocket upgrade without a
/// credential is accepted as a pre-login connection that may only sign in with
/// GitHub. Operator bearer tokens keep working either way; they are not bound to
/// a person unless the connection signs in.
pub async fn start_remote_server(
    backend: Arc<InProcessBackend>,
    token: &str,
    config: RemoteServerConfig,
) -> Result<ServingBackend, LoomError> {
    let tls_configured = config.tls.is_some();
    let allow_insecure_remote = config.allow_insecure_remote;
    let bind_addr = config.bind_addr;
    let auth = Arc::new(AuthTokenStore::new());
    let _issued = auth.insert(token, AuthorizationScope::all())?;
    report_github_sign_in(&config.github_sign_in);
    if let Some(warning) = bind_exposure_warning(bind_addr, tls_configured, allow_insecure_remote) {
        log::warn!("{warning}");
    }
    let remote = RemoteServer::new(backend.clone(), auth, config)
        .bind()
        .await?;
    let server = ServingBackend { remote, backend };
    log::info!(
        "Loom remote backend '{}' listening at {}",
        server.backend.node_name(),
        server.websocket_url()
    );
    log::info!("Health endpoint: {}", server.health_url());
    Ok(server)
}

/// Reports how people sign in, so an operator can tell whether a token-less
/// upgrade will be accepted, how many logins are allowed, and which OAuth app
/// the device flow uses.
pub fn report_github_sign_in(config: &GitHubSignInConfig) {
    if !config.is_enabled() {
        log::info!(
            "GitHub sign-in is disabled; a WebSocket upgrade must carry an operator bearer token"
        );
        return;
    }
    let client_id_source = if config.client_id.is_some() {
        "--github-oauth-client-id"
    } else {
        "the embedded GitHub CLI OAuth app"
    };
    let scope = if config.allowlist_size() == 0 {
        "any GitHub account".to_owned()
    } else {
        format!("{} allowlisted login(s)", config.allowlist_size())
    };
    log::info!(
        "GitHub sign-in is enabled for {scope}; the device flow uses the OAuth client id from \
         {client_id_source}"
    );
    if config.client_id.is_none() {
        log::warn!(
            "using the embedded GitHub CLI public OAuth client id for sign-in; pass \
             --github-oauth-client-id to use your own GitHub OAuth app instead"
        );
    }
}

/// A warning when the bind address publishes the backend beyond the local
/// machine without TLS. The listener then speaks plain `ws://` with a single
/// shared bearer token, which the library only allows because the operator
/// passed `--allow-insecure-remote`; this warns loudly so it cannot happen
/// quietly. Without that opt-in the bind is refused, so there is nothing to warn
/// about and the message must not claim a flag that was never given.
pub fn bind_exposure_warning(
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use loom_core::ErrorCode;
    use loom_model::{GITHUB_COPILOT_PROVIDER_ID, OPENAI_PROVIDER_ID};

    use super::{ProviderRegistryChoice, open_backend_named_with};

    fn temporary_database() -> PathBuf {
        std::env::temp_dir().join(format!(
            "loom-providers-demo-{}.db",
            loom_core::RunId::new()
        ))
    }

    /// Whether the process exports a gateway configuration. The configured
    /// registry reads the process environment, so the gateway assertion only
    /// holds when nothing is exported; no test mutates it.
    fn gateway_exported() -> bool {
        ["LOOM_OPENAI_ENDPOINT", "LOOM_API_KEY"]
            .iter()
            .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
    }

    #[test]
    fn the_providers_choice_defaults_to_configured_and_parses_the_two_words() {
        assert_eq!(
            ProviderRegistryChoice::default(),
            ProviderRegistryChoice::Configured
        );
        assert_eq!(
            ProviderRegistryChoice::parse("configured").unwrap(),
            ProviderRegistryChoice::Configured
        );
        assert_eq!(
            ProviderRegistryChoice::parse("demo").unwrap(),
            ProviderRegistryChoice::Demo
        );
        // The accessor returns the words the parser accepts.
        for choice in [
            ProviderRegistryChoice::Configured,
            ProviderRegistryChoice::Demo,
        ] {
            assert_eq!(
                ProviderRegistryChoice::parse(choice.as_str()).unwrap(),
                choice
            );
        }

        for rejected in ["", "Demo", "demo ", "everything provider"] {
            let error = ProviderRegistryChoice::parse(rejected).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert!(!error.retryable);
            assert!(error.message.contains("--providers"), "{error}");
            assert!(error.message.contains("configured"), "{error}");
            assert!(error.message.contains("demo"), "{error}");
        }
    }

    #[test]
    fn the_demo_choice_serves_the_deterministic_and_local_demo_models() {
        let backend =
            open_backend_named_with(ProviderRegistryChoice::Demo, None, Some("demo-worker"))
                .unwrap();
        assert_eq!(backend.node_name(), "demo-worker");
        let registry = backend.provider_registry();
        let models = registry.list_models().unwrap();
        assert!(
            models
                .iter()
                .any(|model| model.id.as_str() == "deterministic/demo"),
            "the demo profile must serve the deterministic model: {models:?}"
        );
        assert!(
            models
                .iter()
                .any(|model| model.id.as_str() == "openai-compatible/demo"),
            "the demo profile must serve the local OpenAI-compatible model: {models:?}"
        );
        // `ProviderRegistry::demo` gains no GitHub Copilot entry.
        let providers = registry.list_providers().unwrap();
        assert!(
            !providers
                .iter()
                .any(|provider| provider.id.as_str() == GITHUB_COPILOT_PROVIDER_ID),
            "{providers:?}"
        );
        backend.shutdown().unwrap();
    }

    #[test]
    fn the_configured_default_registers_no_gateway_without_its_variables() {
        assert_eq!(
            ProviderRegistryChoice::default(),
            ProviderRegistryChoice::Configured,
            "the default registry the worker serves is the configured one"
        );
        if gateway_exported() {
            // An exported gateway is registered instead, and an endpoint without
            // a key refuses the start; neither is this test's subject.
            return;
        }
        let backend =
            open_backend_named_with(ProviderRegistryChoice::Configured, None, None).unwrap();
        let registry = backend.provider_registry();
        let providers = registry.list_providers().unwrap();
        assert!(
            !providers
                .iter()
                .any(|provider| provider.id.as_str() == "openai-compatible"),
            "nothing exported must register no gateway: {providers:?}"
        );
        assert!(
            providers
                .iter()
                .any(|provider| provider.id.as_str() == OPENAI_PROVIDER_ID),
            "the configured registry keeps its OpenAI entry: {providers:?}"
        );
        backend.shutdown().unwrap();
    }

    #[test]
    fn the_demo_choice_opens_a_temporary_database_and_shuts_down() {
        let path = temporary_database();
        let backend =
            open_backend_named_with(ProviderRegistryChoice::Demo, Some(&path), None).unwrap();
        assert!(path.exists(), "the demo choice must open {path:?}");
        let models = backend.provider_registry().list_models().unwrap();
        assert!(
            models
                .iter()
                .any(|model| model.id.as_str() == "deterministic/demo")
        );
        backend.shutdown().unwrap();
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(path.with_extension("credentials.json")).ok();
        std::fs::remove_dir_all(path.with_extension("session-roots")).ok();
    }
}
