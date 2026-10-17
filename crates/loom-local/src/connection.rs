//! Opaque native transport handles.
//!
//! The UI holds these instead of backend types, so it depends only on the
//! native launcher (`loom-local`) and never links the backend implementation.

use std::{
    env,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use loom_core::{ErrorCode, LoomError, Result};
use loom_protocol::{RequestEnvelope, ResponseEnvelope};

use crate::transport::{
    MAX_FRAME_BYTES_ENV, WebSocketConnection, WebSocketTransport, resolve_max_frame_bytes,
};

/// Settings the launcher passes to the native remote transport.
///
/// `ca_certificate` is added to the OS trust roots rather than replacing them,
/// and `allow_insecure_remote` is the only way to reach a plaintext `ws://`
/// worker that is not on the local machine. Both are operator decisions, so
/// neither has a silent default.
///
/// The frame-size limit is not a field here: it is an argument of
/// [`RemoteConnection::connect_with_max_frame_bytes`], which
/// [`RemoteConnection::connect`] defaults from
/// [`crate::MAX_FRAME_BYTES_ENV`] and
/// [`crate::DEFAULT_MAX_FRAME_BYTES`].
#[derive(Clone, Debug, Default)]
pub struct RemoteConnectionOptions {
    /// PEM file whose certificates are trusted in addition to the OS roots.
    pub ca_certificate: Option<PathBuf>,
    /// Explicit opt-in that permits plaintext `ws://` to a non-loopback host.
    pub allow_insecure_remote: bool,
}

impl RemoteConnectionOptions {
    /// Builds the transport for one endpoint with `max_frame_bytes` applied.
    ///
    /// [`WebSocketTransport::with_max_frame_bytes`] validates the
    /// limit, so an unusable value fails before a connection is attempted.
    fn transport(
        &self,
        url: &str,
        token: &str,
        max_frame_bytes: usize,
    ) -> Result<WebSocketTransport> {
        let transport = WebSocketTransport::new(url, token)
            .with_max_frame_bytes(max_frame_bytes)?
            .with_insecure_remote_allowed(self.allow_insecure_remote);
        match self.ca_certificate.as_deref() {
            Some(certificate) => transport.with_ca_certificate_file(certificate),
            None => Ok(transport),
        }
    }
}

/// A blocking WebSocket connection to a remote worker.
#[derive(Clone)]
pub struct RemoteConnection {
    runtime: Arc<tokio::runtime::Runtime>,
    connection: Arc<Mutex<WebSocketConnection>>,
    secure_for_secrets: bool,
}

/// Deadline applied to a remote request that does not name its own.
///
/// A remote transport can stall on a half-open socket, and the native client
/// runs its requests on a single worker thread, so an unanswered request would
/// block every later request forever. This default is larger than the server's
/// own default request timeout, so a healthy but slow request is still answered
/// before the client gives up; a caller with a different bound uses
/// [`RemoteConnection::request_with_timeout`].
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl RemoteConnection {
    /// Connects to `url`, limiting every WebSocket frame to the limit this
    /// process resolves from [`crate::MAX_FRAME_BYTES_ENV`], or to the
    /// documented [`crate::DEFAULT_MAX_FRAME_BYTES`] (4 MiB) when the
    /// variable is unset.
    ///
    /// The variable is the surface a launcher and a deployment already have for
    /// the remote settings it does not carry as options; a caller that wants an
    /// explicit bound uses [`RemoteConnection::connect_with_max_frame_bytes`].
    pub fn connect(
        url: &str,
        token: &str,
        secure_for_secrets: bool,
        options: &RemoteConnectionOptions,
    ) -> Result<Self> {
        let max_frame_bytes =
            resolve_max_frame_bytes(None, env::var(MAX_FRAME_BYTES_ENV).ok().as_deref())?;
        Self::connect_with_max_frame_bytes(url, token, secure_for_secrets, options, max_frame_bytes)
    }

    /// Connects to `url` with an explicit frame-size limit.
    ///
    /// A request or response above `max_frame_bytes` fails as
    /// `payload_too_large`, naming the request, the received size, and the
    /// limit, instead of reading as a malformed payload. The limit must be
    /// greater than zero, and an over-limit frame is still rejected.
    pub fn connect_with_max_frame_bytes(
        url: &str,
        token: &str,
        secure_for_secrets: bool,
        options: &RemoteConnectionOptions,
        max_frame_bytes: usize,
    ) -> Result<Self> {
        let transport = options.transport(url, token, max_frame_bytes)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not create remote client runtime: {error}"),
                    false,
                )
            })?;
        let connection = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), transport.connect()).await
        });
        let connection = match connection {
            Ok(result) => result?,
            Err(_) => {
                return Err(LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    "worker connection timed out",
                    true,
                ));
            }
        };
        Ok(Self {
            runtime: Arc::new(runtime),
            connection: Arc::new(Mutex::new(connection)),
            secure_for_secrets,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, WebSocketConnection>> {
        self.connection.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "remote connection lock was poisoned",
                true,
            )
        })
    }

    pub fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        self.request_with_timeout(request, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn request_with_timeout(
        &self,
        request: RequestEnvelope,
        timeout: Duration,
    ) -> ResponseEnvelope {
        let request_id = request.request_id;
        let result = self.lock().and_then(|mut connection| {
            self.runtime.block_on(async {
                match tokio::time::timeout(timeout, connection.request(request)).await {
                    Ok(result) => result,
                    Err(_) => Err(LoomError::new(
                        ErrorCode::DeadlineExceeded,
                        format!(
                            "worker did not answer within {}s; it may be stalled or unreachable",
                            timeout.as_secs()
                        ),
                        true,
                    )),
                }
            })
        });
        result.unwrap_or_else(|error| ResponseEnvelope::failure(request_id, error))
    }

    pub fn close(&self) -> Result<()> {
        let mut connection = self.lock()?;
        self.runtime.block_on(connection.close())
    }

    pub fn secure_for_secrets(&self) -> bool {
        self.secure_for_secrets
    }
}
