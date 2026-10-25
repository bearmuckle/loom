//! The verb implementations.
//!
//! Every verb shares one shape: negotiate, send only the requests its row in the
//! module contract names, and either print text lines or return the payload a
//! `--json` envelope carries. A verb therefore never writes output when `--json`
//! is set, and never writes a line before the request it prints succeeded.

mod events;
mod query;
mod run;
mod supervise;

use std::io::Write;

use loom_core::{Capability, CapabilitySet, ErrorCode, LoomError, RequestId};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest, NegotiationResult, RequestEnvelope,
    ServerResponse,
};
use serde_json::Value;

use crate::cli::args::{Invocation, Verb};
use crate::cli::{client::Client, exit};

/// Why a verb produced no success payload.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The command line itself was wrong.
    Usage(String),
    /// Local input or output failed, such as an unreadable cursor file.
    Internal(String),
    /// The reader closed stdout, as in `loom-cli events | head`. That is not an
    /// error: the verb stops quietly and exits 0.
    BrokenPipe,
    /// The request never reached the worker, or its answer never arrived.
    Transport {
        request_id: Option<RequestId>,
        error: LoomError,
    },
    /// A bounded poll reached its deadline without the state it waited for.
    Timeout {
        request_id: Option<RequestId>,
        message: String,
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
            Self::BrokenPipe => exit::OK,
            Self::Transport { .. } => exit::UNREACHABLE,
            Self::Timeout { .. } => exit::TIMEOUT,
            Self::Server { error, .. } => exit::for_code(error.code),
        }
    }

    /// The protocol error code, which a usage or local failure reports as
    /// `invalid_request` or `internal`.
    pub(crate) fn code(&self) -> String {
        match self {
            Self::Usage(_) => ErrorCode::InvalidRequest.to_string(),
            Self::Internal(_) => ErrorCode::Internal.to_string(),
            Self::BrokenPipe => "broken_pipe".to_owned(),
            Self::Timeout { .. } => ErrorCode::DeadlineExceeded.to_string(),
            Self::Transport { error, .. } | Self::Server { error, .. } => error.code.to_string(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Usage(message) | Self::Internal(message) => message,
            Self::BrokenPipe => "the output stream was closed",
            Self::Timeout { message, .. } => message,
            Self::Transport { error, .. } | Self::Server { error, .. } => &error.message,
        }
    }

    fn retryable(&self) -> bool {
        match self {
            Self::Usage(_) | Self::Internal(_) | Self::BrokenPipe | Self::Timeout { .. } => false,
            Self::Transport { error, .. } | Self::Server { error, .. } => error.retryable,
        }
    }

    fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Usage(_) | Self::Internal(_) | Self::BrokenPipe => None,
            Self::Transport { request_id, .. } | Self::Timeout { request_id, .. } => *request_id,
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

    /// A conflict that survived the retry the caller allowed.
    fn conflict(request_id: RequestId, message: String) -> Self {
        Self::Server {
            request_id,
            error: LoomError::new(ErrorCode::Conflict, message, true),
        }
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

/// The successful result of one verb: the payload a `--json` envelope carries,
/// the id of its primary request, and the exit code it reports. A blocked
/// `wait` and a `liveness` verdict that is `gone` or `unknown` are successful
/// determinations that still report the documented non-zero code.
pub(crate) struct Outcome {
    pub data: Value,
    pub request_id: Option<RequestId>,
    pub exit_code: i32,
}

impl Outcome {
    pub(crate) fn new(data: Value, request_id: Option<RequestId>) -> Self {
        Self {
            data,
            request_id,
            exit_code: exit::OK,
        }
    }

    pub(crate) fn with_code(mut self, exit_code: i32) -> Self {
        self.exit_code = exit_code;
        self
    }
}

/// The capabilities a verb negotiates, which are exactly the ones its requests
/// need, plus `JsonProtocol` because every response is JSON.
///
/// A read-only verb therefore never asks for a mutating capability: `ls` asks
/// for the session-read capability its two listing requests need, `get` for the
/// same read, and `events`/`peek` for event subscription. Only `run` requests
/// what its mutations require, so a worker that does not offer session creation
/// can still be inspected and a scoped token sees the smallest negotiable set.
pub(crate) fn used_capabilities(verb: &Verb) -> CapabilitySet {
    let mut capabilities = vec![Capability::JsonProtocol];
    match verb {
        Verb::Doctor => capabilities.push(Capability::ReadWorkerNodeStatus),
        Verb::WhoAmI => {}
        Verb::Ls { .. } => capabilities.push(Capability::ReadAgentSession),
        Verb::Get { .. } => capabilities.push(Capability::ReadAgentSession),
        Verb::Events(_) | Verb::Peek(_) => {
            capabilities.push(Capability::SubscribeSessionEvents);
        }
        Verb::Run(args) => {
            capabilities.extend([
                Capability::CreateAgentSession,
                Capability::ManageSessionRepositories,
                Capability::WriteSessionFilesystem,
                Capability::ConfigureApprovalPolicy,
                Capability::StartAgentRun,
            ]);
            // `--new` never lists sessions, so it also never negotiates the
            // read that listing would need.
            if !args.new {
                capabilities.push(Capability::ReadAgentSession);
            }
        }
        Verb::Send { .. } | Verb::Answer { .. } => {
            capabilities.extend([Capability::ControlAgentRun, Capability::ReadAgentRun]);
        }
        Verb::Approve { .. } | Verb::Deny { .. } => {
            capabilities.extend([Capability::ApproveAgentAction, Capability::ReadAgentRun]);
        }
        Verb::Stop { archive, .. } => {
            capabilities.extend([Capability::ControlAgentRun, Capability::ReadAgentRun]);
            if archive.is_some() {
                capabilities.push(Capability::ControlAgentSession);
            }
        }
        Verb::Wait { .. } => {
            capabilities.extend([Capability::ReadAgentSession, Capability::ReadAgentRun]);
        }
        Verb::Status { .. } | Verb::Liveness { .. } => {
            capabilities.push(Capability::ReadAgentSession);
        }
        Verb::Result { .. } => {
            capabilities.extend([
                Capability::ReadSessionFilesystem,
                Capability::ReadVcsStatus,
                Capability::ReadVcsDiff,
                Capability::ReadSessionTaskEvidence,
            ]);
        }
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
    err: &mut dyn Write,
) -> Result<Outcome, Failure> {
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
        Verb::Events(args) => events::events(client, invocation, args, out, err).await,
        Verb::Peek(args) => events::peek(client, invocation, args, out, err).await,
        Verb::Run(args) => run::run(client, invocation, args, out).await,
        Verb::Send { run, text } => supervise::send(client, invocation, *run, text, out).await,
        Verb::Answer { run, text } => supervise::answer(client, invocation, *run, text, out).await,
        Verb::Approve { run, tool_call } => {
            supervise::approve(client, invocation, *run, *tool_call, out).await
        }
        Verb::Deny {
            run,
            tool_call,
            reason,
        } => supervise::deny(client, invocation, *run, *tool_call, reason.as_deref(), out).await,
        Verb::Stop {
            run,
            archive,
            retry_on_conflict,
        } => supervise::stop(client, invocation, *run, *archive, *retry_on_conflict, out).await,
        Verb::Wait {
            session,
            until,
            timeout_secs,
        } => supervise::wait(client, invocation, *session, *until, *timeout_secs, out).await,
        Verb::Status { session, format } => {
            supervise::status(client, invocation, *session, *format, out).await
        }
        Verb::Liveness { session } => supervise::liveness(client, invocation, *session, out).await,
        Verb::Result { session, task } => {
            supervise::result(client, invocation, *session, *task, out).await
        }
    }
}

/// Writes one preformatted line. A reader that closed the pipe is a clean stop,
/// and any other write failure is reported as a local failure instead of a
/// panic.
fn emit(out: &mut dyn Write, line: &str) -> Result<(), Failure> {
    out.write_all(line.as_bytes()).map_err(|error| {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            Failure::BrokenPipe
        } else {
            Failure::Internal(format!("could not write output: {error}"))
        }
    })
}
