//! Opaque native transport handles.
//!
//! The UI holds these instead of `loom-server` types directly, so it depends on
//! the native launcher (`loom-local`) rather than the backend implementation.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use loom_core::{ErrorCode, LoomError, Result};
use loom_model::ModelId;
use loom_protocol::{RequestEnvelope, ResponseEnvelope};
use loom_server::{ARCHIVE_RETENTION_SWEEP_INTERVAL, ArchiveRetentionSweepDriver};

/// An owned in-process backend embedded in a native client.
pub struct OwnedBackend {
    inner: Arc<loom_server::InProcessBackend>,
    /// The background archive-retention sweep, present only for a durable
    /// backend that can store a retention policy. Dropping it stops the sweep
    /// before the backend releases its database.
    retention_sweeps: Mutex<Option<ArchiveRetentionSweepDriver>>,
}

impl OwnedBackend {
    pub fn new() -> Self {
        Self {
            inner: loom_server::InProcessBackend::new(),
            retention_sweeps: Mutex::new(None),
        }
    }

    pub fn demo_with_github_copilot() -> Result<Self> {
        Ok(Self {
            inner: loom_server::InProcessBackend::demo_with_github_copilot()?,
            retention_sweeps: Mutex::new(None),
        })
    }

    pub fn new_persistent(path: PathBuf) -> Result<Self> {
        Ok(Self::with_retention_sweeps(
            loom_server::InProcessBackend::new_persistent(path)?,
        ))
    }

    pub fn with_openai_compatible_persistent_with_github_copilot(
        endpoint: &str,
        api_key: &str,
        model: ModelId,
        path: PathBuf,
    ) -> Result<Self> {
        Ok(Self::with_retention_sweeps(
            loom_server::InProcessBackend::with_openai_compatible_persistent_with_github_copilot(
                endpoint, api_key, model, path,
            )?,
        ))
    }

    pub fn new_persistent_with_github_copilot(path: PathBuf) -> Result<Self> {
        Ok(Self::with_retention_sweeps(
            loom_server::InProcessBackend::new_persistent_with_github_copilot(path)?,
        ))
    }

    /// Wraps a durable backend and starts the archive-retention sweep.
    ///
    /// A native client that embeds its backend has no server process to run the
    /// sweep, so without this an archived session would stay forever however the
    /// retention policy is set. Starting is best-effort: a driver that cannot
    /// start its thread logs the reason there instead of failing the client.
    fn with_retention_sweeps(inner: Arc<loom_server::InProcessBackend>) -> Self {
        let driver = ArchiveRetentionSweepDriver::start(
            Arc::clone(&inner),
            ARCHIVE_RETENTION_SWEEP_INTERVAL,
        );
        Self {
            inner,
            retention_sweeps: Mutex::new(Some(driver)),
        }
    }

    pub fn connect(&self) -> LocalConnection {
        LocalConnection(self.inner.connect())
    }

    pub fn shutdown(&self) -> Result<()> {
        // Stop the sweep before the backend releases its database, so no round
        // runs against a shutting-down store.
        if let Ok(mut driver) = self.retention_sweeps.lock() {
            drop(driver.take());
        }
        self.inner.shutdown()
    }

    /// Whether a background archive-retention sweep is running for this backend.
    #[cfg(test)]
    pub(crate) fn runs_archive_retention_sweeps(&self) -> bool {
        self.retention_sweeps
            .lock()
            .map(|driver| driver.is_some())
            .unwrap_or(false)
    }
}

impl Default for OwnedBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// A connected in-process backend transport.
#[derive(Clone)]
pub struct LocalConnection(loom_server::InProcessConnection);

impl LocalConnection {
    pub fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        self.0.request(request)
    }
}

/// Settings the launcher passes to the native remote transport.
///
/// `ca_certificate` is added to the OS trust roots rather than replacing them,
/// and `allow_insecure_remote` is the only way to reach a plaintext `ws://`
/// worker that is not on the local machine. Both are operator decisions, so
/// neither has a silent default.
#[derive(Clone, Debug, Default)]
pub struct RemoteConnectionOptions {
    /// PEM file whose certificates are trusted in addition to the OS roots.
    pub ca_certificate: Option<PathBuf>,
    /// Explicit opt-in that permits plaintext `ws://` to a non-loopback host.
    pub allow_insecure_remote: bool,
}

impl RemoteConnectionOptions {
    /// Builds the transport for one endpoint.
    fn transport(&self, url: &str, token: &str) -> Result<loom_server::WebSocketTransport> {
        let transport = loom_server::WebSocketTransport::new(url, token)
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
    connection: Arc<Mutex<loom_server::WebSocketConnection>>,
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
    pub fn connect(
        url: &str,
        token: &str,
        secure_for_secrets: bool,
        options: &RemoteConnectionOptions,
    ) -> Result<Self> {
        let transport = options.transport(url, token)?;
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

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, loom_server::WebSocketConnection>> {
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
