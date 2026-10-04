//! The backend host service that owns the protocol connection worker.

#[cfg(not(target_family = "wasm"))]
use std::sync::mpsc::{self, Sender};

use futures_channel::oneshot;
use loom_core::{ErrorCode, LoomError, RequestId};
use loom_protocol::{RequestEnvelope, ResponseEnvelope};

use crate::connection::ClientConnection;

/// A submitted request whose response has not arrived yet.
pub(crate) struct PendingResponse {
    request_id: RequestId,
    reply: oneshot::Receiver<ResponseEnvelope>,
}

impl PendingResponse {
    /// Awaits the worker's answer.
    ///
    /// Natively this runs on a background OS thread and the underlying
    /// worker call is a genuine blocking wait; in the browser it awaits a
    /// channel fed by the WebSocket's `onmessage` callback. Either way,
    /// callers just do `pending.wait().await`.
    pub(crate) async fn wait(self) -> ResponseEnvelope {
        self.reply.await.unwrap_or_else(|_| {
            ResponseEnvelope::failure(
                self.request_id,
                LoomError::new(
                    ErrorCode::Internal,
                    "the client connection worker stopped before answering",
                    false,
                ),
            )
        })
    }
}

/// Owns the protocol connection.
///
/// Natively, a dedicated worker thread executes requests in submission
/// order, which also keeps the remote transport's single in-flight request
/// contract; UI handlers submit a request and await a [`PendingResponse`] on
/// a background task, so a handler never blocks the GPUI thread on backend
/// latency. In the browser there is only one JS thread, so each submitted
/// request is instead driven forward as its own cooperative task on that
/// same thread; the browser transport already supports overlapping in-flight
/// requests (it correlates responses by request id), so no additional
/// serialization is needed there.
#[derive(Clone)]
pub(crate) struct BackendWorker {
    #[cfg(not(target_family = "wasm"))]
    jobs: Sender<Job>,
    #[cfg(all(not(target_family = "wasm"), test))]
    test_connection: Option<ClientConnection>,
    #[cfg(target_family = "wasm")]
    connection: ClientConnection,
    secure_for_secrets: bool,
}

#[cfg(not(target_family = "wasm"))]
#[allow(dead_code)]
struct Job {
    request: RequestEnvelope,
    reply: oneshot::Sender<ResponseEnvelope>,
}

impl BackendWorker {
    #[cfg(all(not(target_family = "wasm"), test))]
    pub(crate) fn spawn(connection: ClientConnection) -> Self {
        let secure_for_secrets = connection.secure_for_secrets();
        let (jobs, _incoming) = mpsc::channel::<Job>();
        Self {
            jobs,
            test_connection: Some(connection),
            secure_for_secrets,
        }
    }

    #[cfg(all(not(target_family = "wasm"), not(test)))]
    pub(crate) fn spawn(connection: ClientConnection) -> Self {
        let secure_for_secrets = connection.secure_for_secrets();
        let (jobs, incoming) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            while let Ok(job) = incoming.recv() {
                let response = connection.request(job.request);
                // A dropped receiver means the view stopped caring about this
                // request; the backend has already applied it either way.
                let _ = job.reply.send(response);
            }
        });
        Self {
            jobs,
            secure_for_secrets,
        }
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn spawn(connection: ClientConnection) -> Self {
        let secure_for_secrets = connection.secure_for_secrets();
        Self {
            connection,
            secure_for_secrets,
        }
    }

    pub(crate) fn secure_for_secrets(&self) -> bool {
        self.secure_for_secrets
    }

    /// The reason the underlying transport is closed, if known.
    ///
    /// The browser returns the socket's close reason so the view can show a
    /// reconnect screen. Native transports manage their own lifecycle and
    /// report availability through worker-node status instead.
    #[cfg(target_family = "wasm")]
    pub(crate) fn connection_closed_reason(&self) -> Option<String> {
        self.connection.closed_reason()
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn submit(&self, request: RequestEnvelope) -> PendingResponse {
        let request_id = request.request_id;
        let (reply, receiver) = oneshot::channel();
        #[cfg(test)]
        if let Some(connection) = &self.test_connection {
            let _ = reply.send(connection.request(request));
            return PendingResponse {
                request_id,
                reply: receiver,
            };
        }
        if self.jobs.send(Job { request, reply }).is_err() {
            // The worker thread is gone; return a receiver that will
            // immediately resolve to the "stopped before answering" error.
            let (_, receiver) = oneshot::channel();
            return PendingResponse {
                request_id,
                reply: receiver,
            };
        }
        PendingResponse {
            request_id,
            reply: receiver,
        }
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn submit(&self, request: RequestEnvelope) -> PendingResponse {
        let request_id = request.request_id;
        let (reply, receiver) = oneshot::channel();
        let connection = self.connection.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let response = connection.request(request).await;
            let _ = reply.send(response);
        });
        PendingResponse {
            request_id,
            reply: receiver,
        }
    }
}
