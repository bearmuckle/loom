//! The verb implementations.
//!
//! Every verb shares one shape: negotiate, send only the requests its row in the
//! module contract names, and either print text lines or return the payload a
//! `--json` envelope carries. A verb therefore never writes output when `--json`
//! is set, and never writes a line before the request it prints succeeded.

mod events;
mod query;
mod run;

use std::io::Write;

use loom_core::{Capability, CapabilitySet, ErrorCode, LoomError, RequestId};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest, NegotiationResult, RequestEnvelope,
    ServerResponse,
};
use serde_json::Value;

use crate::ctl::args::{Invocation, Verb};
use crate::ctl::{client::Client, exit};

/// Why a verb produced no success payload.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The command line itself was wrong.
    Usage(String),
    /// Local input or output failed, such as an unreadable cursor file.
    Internal(String),
    /// The request never reached the worker, or its answer never arrived.
    Transport {
        request_id: Option<RequestId>,
        error: LoomError,
    },
    /// The worker answered with an error.
    Server {
        request_id: RequestId,
        error: LoomError,
    },
}

impl Failure {
    pub(crate) fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => exit::USAGE,
            Self::Internal(_) => exit::INTERNAL,
            Self::Transport { .. } => exit::UNREACHABLE,
            Self::Server { error, .. } => exit::for_code(error.code),
        }
    }

    /// The protocol error code, which a usage or local failure reports as
    /// `invalid_request` or `internal`.
    pub(crate) fn code(&self) -> String {
        match self {
            Self::Usage(_) => ErrorCode::InvalidRequest.to_string(),
            Self::Internal(_) => ErrorCode::Internal.to_string(),
            Self::Transport { error, .. } | Self::Server { error, .. } => error.code.to_string(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Usage(message) | Self::Internal(message) => message,
            Self::Transport { error, .. } | Self::Server { error, .. } => &error.message,
        }
    }

    fn retryable(&self) -> bool {
        match self {
            Self::Usage(_) | Self::Internal(_) => false,
            Self::Transport { error, .. } | Self::Server { error, .. } => error.retryable,
        }
    }

    fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Usage(_) | Self::Internal(_) => None,
            Self::Transport { request_id, .. } => *request_id,
            Self::Server { request_id, .. } => Some(*request_id),
        }
    }

    /// The failure envelope of the `--json` contract.
    pub(crate) fn envelope(&self, verb: &str) -> Value {
        self.envelope_with_code(verb, self.exit_code())
    }

    pub(crate) fn envelope_with_code(&self, verb: &str, exit_code: i32) -> Value {
        serde_json::json!({
            "ok": false,
            "verb": verb,
            "request_id": self.request_id().map(|id| id.to_string()),
            "error": {
                "code": self.code(),
                "message": self.message(),
                "retryable": self.retryable(),
                "exit_code": exit_code,
            },
        })
    }

    /// A worker answered a request with a response of another kind, which means
    /// the two sides do not agree on the protocol after all.
    fn unexpected(request_id: RequestId, family: &str) -> Self {
        Self::Server {
            request_id,
            error: LoomError::new(
                ErrorCode::MalformedPayload,
                format!("the worker answered the {family} request with an unexpected response"),
                false,
            ),
        }
    }
}

/// The capabilities a verb negotiates: the set every verb shares, plus the ones
/// `run` adds for the mutations only it performs.
///
/// A verb negotiates exactly the capabilities its own requests need, so a
/// read-only verb keeps asking for the shared set alone: its handshake stays
/// byte-identical, and a worker that does not offer session creation can still
/// be inspected. Only `run` mutates, so only `run` asks for what its mutations
/// require.
pub(crate) fn used_capabilities(verb: &Verb) -> CapabilitySet {
    let mut capabilities = vec![
        Capability::SubscribeSessionEvents,
        Capability::ReadAgentSession,
        Capability::ManageWorkspaces,
        Capability::ManageSessionRepositories,
        Capability::WriteSessionFilesystem,
        Capability::ReadWorkerNodeStatus,
        Capability::JsonProtocol,
        Capability::StartAgentRun,
    ];
    if matches!(verb, Verb::Run(_)) {
        capabilities.push(Capability::CreateAgentSession);
        capabilities.push(Capability::ConfigureApprovalPolicy);
    }
    CapabilitySet::new(capabilities)
}

/// One connection's requests, remembering the id of the first one.
struct Caller<'a, C: Client> {
    client: &'a mut C,
    primary: Option<RequestId>,
}

impl<'a, C: Client> Caller<'a, C> {
    fn new(client: &'a mut C) -> Self {
        Self {
            client,
            primary: None,
        }
    }

    async fn call(
        &mut self,
        request: ClientRequest,
    ) -> Result<(RequestId, ServerResponse), Failure> {
        self.call_envelope(RequestEnvelope::new(request)).await
    }

    /// Sends `request` under a caller-chosen id, which is what makes a run start
    /// replayable: the worker answers a repeated id from its durable cache.
    async fn call_with_id(
        &mut self,
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<(RequestId, ServerResponse), Failure> {
        self.call_envelope(RequestEnvelope::with_request_id(request_id, request))
            .await
    }

    async fn call_envelope(
        &mut self,
        envelope: RequestEnvelope,
    ) -> Result<(RequestId, ServerResponse), Failure> {
        let request_id = envelope.request_id;
        if self.primary.is_none() {
            self.primary = Some(request_id);
        }
        let response = self
            .client
            .request(envelope)
            .await
            .map_err(|error| Failure::Transport {
                request_id: Some(request_id),
                error,
            })?;
        let result = response
            .result
            .map_err(|error| Failure::Server { request_id, error })?;
        Ok((request_id, result))
    }
}

/// The version handshake every verb performs before its own requests.
async fn handshake<C: Client>(
    caller: &mut Caller<'_, C>,
    verb: &Verb,
) -> Result<NegotiationResult, Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Control(ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: used_capabilities(verb),
        }))
        .await?;
    match response {
        ServerResponse::Control(loom_protocol::ControlResponse::Negotiated(negotiated)) => {
            Ok(negotiated)
        }
        _ => Err(Failure::unexpected(request_id, "negotiate")),
    }
}

/// Runs one verb and returns the payload a `--json` envelope reports, plus the
/// id of its primary request.
pub(crate) async fn run<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
) -> Result<(Value, Option<RequestId>), Failure> {
    match &invocation.verb {
        Verb::Doctor => query::doctor(client, invocation, out).await,
        Verb::WhoAmI => query::whoami(client, invocation, out).await,
        Verb::Ls {
            workspace,
            archived,
        } => query::ls(client, invocation, *workspace, *archived, out).await,
        Verb::Get { session, metadata } => {
            query::get(client, invocation, *session, *metadata, out).await
        }
        Verb::Events(args) => events::events(client, invocation, args, out).await,
        Verb::Peek(args) => events::peek(client, invocation, args, out).await,
        Verb::Run(args) => run::run(client, invocation, args, out).await,
    }
}

/// Writes one preformatted line, reporting a broken output stream as a local
/// failure rather than panicking.
fn emit(out: &mut dyn Write, line: &str) -> Result<(), Failure> {
    out.write_all(line.as_bytes())
        .map_err(|error| Failure::Internal(format!("could not write output: {error}")))
}
