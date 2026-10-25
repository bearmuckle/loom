//! The client seam every verb talks to.
//!
//! [`Client`] is object safe and asynchronous, so a verb is written once against
//! it and runs both over a real WebSocket (`RemoteClient`) and over the
//! in-process backend the tests drive. `request` reports a transport failure as
//! an `Err`: a protocol error from the worker arrives inside the envelope's
//! `result`, which is what keeps a refused request distinguishable from a
//! dropped connection.

use std::{future::Future, pin::Pin};

use loom_core::LoomError;
use loom_protocol::{RequestEnvelope, ResponseEnvelope};
use loom_server::WebSocketConnection;

/// One request/response exchange with a worker.
pub trait Client {
    /// Sends `envelope` and returns the answer for it.
    fn request<'a>(
        &'a mut self,
        envelope: RequestEnvelope,
    ) -> Pin<Box<dyn Future<Output = Result<ResponseEnvelope, LoomError>> + 'a>>;
}

/// A client backed by a WebSocket connection to a worker.
pub struct RemoteClient {
    connection: WebSocketConnection,
}

impl RemoteClient {
    pub fn new(connection: WebSocketConnection) -> Self {
        Self { connection }
    }
}

impl Client for RemoteClient {
    fn request<'a>(
        &'a mut self,
        envelope: RequestEnvelope,
    ) -> Pin<Box<dyn Future<Output = Result<ResponseEnvelope, LoomError>> + 'a>> {
        Box::pin(async move { self.connection.request(envelope).await })
    }
}

/// Clients that only the tests use: the in-process backend, and decorators that
/// rewrite a request or its answer.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use loom_protocol::ClientRequest;
    use loom_server::{InProcessBackend, InProcessConnection};
    use std::sync::Arc;

    /// A client that answers from the in-process backend.
    pub(crate) struct InProcessClient {
        connection: InProcessConnection,
    }

    impl InProcessClient {
        pub(crate) fn new(backend: &Arc<InProcessBackend>) -> Self {
            Self {
                connection: backend.connect(),
            }
        }
    }

    impl Client for InProcessClient {
        fn request<'a>(
            &'a mut self,
            envelope: RequestEnvelope,
        ) -> Pin<Box<dyn Future<Output = Result<ResponseEnvelope, LoomError>> + 'a>> {
            Box::pin(async move { Ok(self.connection.request(envelope)) })
        }
    }

    /// A test hook that rewrites a request before it is sent.
    pub(crate) type Before = Box<dyn FnMut(&mut RequestEnvelope)>;
    /// A test hook that rewrites an answer after it arrives.
    pub(crate) type After = Box<dyn FnMut(&ClientRequest, &mut ResponseEnvelope)>;
    /// A test hook that fails a request before it reaches the inner client.
    pub(crate) type Fail = Box<dyn FnMut(&ClientRequest) -> Option<LoomError>>;

    /// A client that lets a test rewrite the request and the answer around an
    /// inner client, which is how a protocol mismatch and a stale-cursor
    /// resynchronisation are produced without a second worker.
    pub(crate) struct Wrapped<C> {
        inner: C,
        before: Option<Before>,
        after: Option<After>,
        fail: Option<Fail>,
    }

    impl<C> Wrapped<C> {
        pub(crate) fn new(inner: C) -> Self {
            Self {
                inner,
                before: None,
                after: None,
                fail: None,
            }
        }

        pub(crate) fn before<F>(mut self, hook: F) -> Self
        where
            F: FnMut(&mut RequestEnvelope) + 'static,
        {
            self.before = Some(Box::new(hook));
            self
        }

        pub(crate) fn after<F>(mut self, hook: F) -> Self
        where
            F: FnMut(&ClientRequest, &mut ResponseEnvelope) + 'static,
        {
            self.after = Some(Box::new(hook));
            self
        }

        /// Fails the requests `hook` names, which is how a transport failure
        /// is produced without a second worker.
        pub(crate) fn fail_on<F>(mut self, hook: F) -> Self
        where
            F: FnMut(&ClientRequest) -> Option<LoomError> + 'static,
        {
            self.fail = Some(Box::new(hook));
            self
        }
    }

    impl<C: Client> Client for Wrapped<C> {
        fn request<'a>(
            &'a mut self,
            mut envelope: RequestEnvelope,
        ) -> Pin<Box<dyn Future<Output = Result<ResponseEnvelope, LoomError>> + 'a>> {
            if let Some(before) = self.before.as_mut() {
                before(&mut envelope);
            }
            let request = envelope.request.clone();
            let failure = self.fail.as_mut().and_then(|fail| fail(&request));
            let inner = &mut self.inner;
            let after = self.after.as_mut();
            Box::pin(async move {
                if let Some(error) = failure {
                    return Err(error);
                }
                let mut response = inner.request(envelope).await?;
                if let Some(after) = after {
                    after(&request, &mut response);
                }
                Ok(response)
            })
        }
    }
}
