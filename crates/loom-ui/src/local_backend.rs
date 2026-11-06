//! The in-process backend a view test drives.
//!
//! The native client no longer owns a backend process: it connects to a worker
//! the setup screen installed or reached. Tests still need a backend without a
//! socket, so this module is compiled for tests only, along with the
//! `loom-server` dev-dependency it needs.

use loom_core::LoomError;

pub(crate) struct OwnedBackend {
    backend: std::sync::Arc<loom_server::InProcessBackend>,
}

impl OwnedBackend {
    /// An in-memory backend with the deterministic demo provider.
    pub(crate) fn new() -> Self {
        Self {
            backend: loom_server::InProcessBackend::new(),
        }
    }

    pub(crate) fn new_persistent_with_github_copilot(
        path: std::path::PathBuf,
    ) -> Result<Self, LoomError> {
        Ok(Self {
            backend: loom_server::InProcessBackend::new_persistent_with_github_copilot(path)?,
        })
    }

    pub(crate) fn connect(&self) -> loom_server::InProcessConnection {
        self.backend.connect()
    }

    pub(crate) fn shutdown(&self) -> Result<(), LoomError> {
        self.backend.shutdown()
    }
}
