//! The backend a native client owns: a managed `loom-server` child process.
//!
//! Tests substitute an in-process backend so the view can be driven without a
//! child process; that variant, and the `loom-server` dev-dependency it needs,
//! exist only in test builds.

use std::sync::Arc;

use loom_core::LoomError;
use loom_local::ManagedWorker;

pub(crate) struct OwnedBackend {
    kind: Kind,
}

enum Kind {
    Managed(Arc<ManagedWorker>),
    #[cfg(test)]
    InProcess(Arc<loom_server::InProcessBackend>),
}

impl OwnedBackend {
    pub(crate) fn managed(worker: ManagedWorker) -> Self {
        Self {
            kind: Kind::Managed(Arc::new(worker)),
        }
    }

    /// The managed worker process, so the application can stop it on quit even
    /// when the view is never dropped.
    pub(crate) fn managed_worker(&self) -> Option<Arc<ManagedWorker>> {
        match &self.kind {
            Kind::Managed(worker) => Some(Arc::clone(worker)),
            #[cfg(test)]
            Kind::InProcess(_) => None,
        }
    }

    pub(crate) fn shutdown(&self) -> Result<(), LoomError> {
        match &self.kind {
            Kind::Managed(worker) => {
                worker.stop();
                Ok(())
            }
            #[cfg(test)]
            Kind::InProcess(backend) => backend.shutdown(),
        }
    }
}

#[cfg(test)]
impl OwnedBackend {
    /// An in-memory backend with the deterministic demo provider.
    pub(crate) fn new() -> Self {
        Self {
            kind: Kind::InProcess(loom_server::InProcessBackend::new()),
        }
    }

    pub(crate) fn new_persistent_with_github_copilot(
        path: std::path::PathBuf,
    ) -> Result<Self, LoomError> {
        Ok(Self {
            kind: Kind::InProcess(
                loom_server::InProcessBackend::new_persistent_with_github_copilot(path)?,
            ),
        })
    }

    pub(crate) fn connect(&self) -> loom_server::InProcessConnection {
        match &self.kind {
            Kind::InProcess(backend) => backend.connect(),
            Kind::Managed(_) => panic!("a managed worker is reached over WebSocket"),
        }
    }
}
