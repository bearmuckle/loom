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

/// An owned in-process backend embedded in a native client.
pub struct OwnedBackend {
    inner: Arc<loom_server::InProcessBackend>,
}

impl OwnedBackend {
    pub fn new() -> Self {
        Self {
            inner: loom_server::InProcessBackend::new(),
        }
    }

    pub fn demo_with_github_copilot() -> Result<Self> {
        Ok(Self {
            inner: loom_server::InProcessBackend::demo_with_github_copilot()?,
        })
    }

    pub fn with_openai_compatible_persistent_with_github_copilot(
        endpoint: &str,
        api_key: &str,
        model: ModelId,
        path: PathBuf,
    ) -> Result<Self> {
        Ok(Self {
            inner: loom_server::InProcessBackend::with_openai_compatible_persistent_with_github_copilot(
                endpoint, api_key, model, path,
            )?,
        })
    }

    pub fn new_persistent_with_github_copilot(path: PathBuf) -> Result<Self> {
        Ok(Self {
            inner: loom_server::InProcessBackend::new_persistent_with_github_copilot(path)?,
        })
    }

    pub fn connect(&self) -> LocalConnection {
        LocalConnection(self.inner.connect())
    }

    pub fn shutdown(&self) -> Result<()> {
        self.inner.shutdown()
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

/// A blocking WebSocket connection to a remote worker.
#[derive(Clone)]
pub struct RemoteConnection {
    runtime: Arc<tokio::runtime::Runtime>,
    connection: Arc<Mutex<loom_server::WebSocketConnection>>,
    secure_for_secrets: bool,
}

impl RemoteConnection {
    pub fn connect(url: &str, token: &str, secure_for_secrets: bool) -> Result<Self> {
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
            tokio::time::timeout(
                Duration::from_secs(15),
                loom_server::WebSocketTransport::new(url, token).connect(),
            )
            .await
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
        let request_id = request.request_id;
        let result = self
            .lock()
            .and_then(|mut connection| self.runtime.block_on(connection.request(request)));
        match result {
            Ok(response) => response,
            Err(error) => ResponseEnvelope::failure(request_id, error),
        }
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
                        "worker request timed out",
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
