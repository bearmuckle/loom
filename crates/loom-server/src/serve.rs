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
    match persistence {
        Some(path) => InProcessBackend::new_persistent_with_github_copilot_named(path, node_name),
        None => InProcessBackend::new_with_github_copilot_named(node_name),
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
