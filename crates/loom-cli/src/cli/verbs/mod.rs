//! The verb implementations.
//!
//! Every verb shares one shape: negotiate, send only the requests its row in the
//! module contract names, and either print text lines or return the payload a
//! `--json` envelope carries. A verb therefore never writes output when `--json`
//! is set, and never writes a line before the request it prints succeeded.
//!
//! `status --tree` is the one verb that projects a second authority: it reads
//! `Project/GetProjectSnapshotForSession` in addition to the session metadata
//! and reports the project, its agents, delegated tasks, worktrees, and the
//! pending decisions of the agents that block. The worker stays authoritative
//! for the records; this client only derives the verdict words and reads one
//! more session metadata projection per blocked agent, because the project
//! record says that an agent waits but not on which approval or question.
//!
//! Every session input is a full id or a unique prefix of one. [`run`] resolves
//! a prefix against the worker's own session listing before it dispatches, so a
//! verb below only ever sees the full id; a command line of full ids sends no
//! listing at all.

mod child_models;
mod events;
mod jev;
mod operator;
mod project;
mod providers;
mod query;
mod run;
mod supervise;

use std::io::Write;

use loom_core::{AgentSessionId, Capability, CapabilitySet, ErrorCode, LoomError, RequestId};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest, ControlResponse, NegotiationResult,
    RequestEnvelope, ServerResponse, SessionResponse, WorkspaceRequest, WorkspaceResponse,
};
use serde_json::Value;

use crate::cli::args::{Invocation, SessionRef, Verb, session_hex};
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
    /// A call outside the worker connection failed, such as a decision-model
    /// request this client made itself. Its code and exit code are the
    /// provider's, not the worker's.
    Provider(LoomError),
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
            Self::Provider(error) => exit::for_code(error.code),
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
            Self::Provider(error) => error.code.to_string(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Usage(message) | Self::Internal(message) => message,
            Self::BrokenPipe => "the output stream was closed",
            Self::Timeout { message, .. } => message,
            Self::Transport { error, .. } | Self::Server { error, .. } => &error.message,
            Self::Provider(error) => &error.message,
        }
    }

    fn retryable(&self) -> bool {
        match self {
            Self::Usage(_) | Self::Internal(_) | Self::BrokenPipe => false,
            // A bounded poll that reached its deadline can be resumed: the
            // state it waited for is still there, so running the command again
            // continues from where the bound stopped it.
            Self::Timeout { .. } => true,
            Self::Transport { error, .. } | Self::Server { error, .. } => error.retryable,
            Self::Provider(error) => error.retryable,
        }
    }

    fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Usage(_) | Self::Internal(_) | Self::BrokenPipe => None,
            Self::Provider(_) => None,
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
    /// Whether the worker answered a mutating request of this invocation from
    /// its idempotency cache instead of acting again.
    pub replayed: bool,
}

impl Outcome {
    pub(crate) fn new(data: Value, request_id: Option<RequestId>) -> Self {
        Self {
            data,
            request_id,
            exit_code: exit::OK,
            replayed: false,
        }
    }

    pub(crate) fn with_code(mut self, exit_code: i32) -> Self {
        self.exit_code = exit_code;
        self
    }

    pub(crate) fn with_replayed(mut self, replayed: bool) -> Self {
        self.replayed = replayed;
        self
    }
}

/// The capabilities a verb negotiates, which are exactly the ones its requests
/// need, plus `JsonProtocol` because every response is JSON.
///
/// A read-only verb therefore never asks for a mutating capability: `node` and
/// `retention get` ask for the worker-status read, `providers list` and
/// `models list` for the provider catalog, `ls` and `get` for the session read,
/// and `events`/`peek` for event subscription. Only `run`, `retention set`,
/// `providers set-key`, and `delete` request what their mutations require, so a
/// worker that does not offer session creation can still be inspected and a
/// scoped token sees the smallest negotiable set.
pub(crate) fn used_capabilities(verb: &Verb) -> CapabilitySet {
    let mut capabilities = vec![Capability::JsonProtocol];
    match verb {
        Verb::Doctor => capabilities.push(Capability::ReadWorkerNodeStatus),
        Verb::Node => capabilities.push(Capability::ReadWorkerNodeStatus),
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
            // Without `--model` the verb asks the worker for its providers to
            // derive the model, which is the same read `providers list` sends.
            // An explicit `--model` never asks for it.
            if args.model.is_none() {
                capabilities.push(Capability::ListProviders);
            }
            // `--project` lists the workspace and reads project snapshots to
            // check the project and find its sub projects, and a sub project
            // it creates without a named input lists the project's sources to
            // inherit them.
            if args.project.is_some() {
                capabilities.extend([Capability::ReadAgentSession, Capability::ReadProject]);
                if args.repository.is_none() && args.directory.is_none() {
                    capabilities.push(Capability::ReadSessionFilesystem);
                }
            }
            // Child model flags read the stored policy and replace it.
            if !args.child_models.is_empty() {
                capabilities.extend([Capability::ReadProject, Capability::ControlAgentSession]);
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
        Verb::Liveness { .. } => capabilities.push(Capability::ReadAgentSession),
        Verb::Status { tree, .. } => {
            capabilities.push(Capability::ReadAgentSession);
            // The tree is projected from the project snapshot, so it also
            // negotiates that read; a plain `status` never asks for it.
            if *tree {
                capabilities.push(Capability::ReadProject);
            }
        }
        Verb::Result { .. } => {
            capabilities.extend([
                Capability::ReadSessionFilesystem,
                Capability::ReadVcsStatus,
                Capability::ReadVcsDiff,
                Capability::ReadSessionTaskEvidence,
            ]);
        }
        Verb::RetentionGet => capabilities.push(Capability::ReadWorkerNodeStatus),
        Verb::RetentionSet(_) => {
            capabilities.extend([
                Capability::ReadWorkerNodeStatus,
                Capability::ConfigureArchiveRetention,
            ]);
        }
        Verb::ProvidersList | Verb::ModelsList { .. } => {
            capabilities.push(Capability::ListProviders);
        }
        Verb::ProvidersSetKey(_) | Verb::ProvidersAdd(_) | Verb::ProvidersRemove { .. } => {
            capabilities.push(Capability::ConfigureProviders);
        }
        Verb::ProvidersDiscover { .. } => capabilities.push(Capability::ListProviders),
        // `jev status` and `jev ask` are local and send nothing at all; `jev
        // triage` reads the session, its pending approval, and the session
        // root, and approves the call only when `--apply` allows it.
        Verb::JevStatus(_) | Verb::JevAsk(_) => {}
        Verb::JevTriage(args) => {
            capabilities.extend([
                Capability::ReadAgentSession,
                Capability::ReadAgentRun,
                Capability::ReadSessionFilesystem,
            ]);
            if args.apply {
                capabilities.push(Capability::ApproveAgentAction);
            }
        }
        Verb::SessionArchive { .. } | Verb::ProjectArchive { .. } => {
            capabilities.extend([Capability::ReadProject, Capability::ControlAgentSession]);
        }
        Verb::SessionDelete { .. } | Verb::Delete { .. } | Verb::ProjectDelete { .. } => {
            capabilities.extend([Capability::ReadProject, Capability::DeleteAgentSession]);
        }
        Verb::ProjectWorktreeCleanup { .. } => {
            capabilities.extend([
                Capability::ReadProject,
                Capability::CleanupProjectChildWorktree,
            ]);
        }
        Verb::ProjectWorktrees { .. } | Verb::ProjectWorktreeGet { .. } => {
            capabilities.push(Capability::ReadProject);
        }
        Verb::ProjectWorktreeDiff { .. } => {
            capabilities.extend([Capability::ReadProject, Capability::ReadProjectChildReview]);
        }
        Verb::ProjectWorktreeIntegrate { .. } => {
            capabilities.extend([Capability::ReadProject, Capability::IntegrateProjectChild]);
        }
        Verb::ProjectGet { .. }
        | Verb::ProjectTree { .. }
        | Verb::ProjectTasks { .. }
        | Verb::ProjectModelsGet { .. } => {
            capabilities.push(Capability::ReadProject);
        }
        Verb::ProjectModelsSet { .. } => {
            capabilities.extend([Capability::ReadProject, Capability::ControlAgentSession]);
        }
        Verb::ProjectChildren { .. } => {
            capabilities.extend([Capability::ReadProject, Capability::ReadAgentSession]);
        }
        Verb::ProjectMessages { .. } => {
            capabilities.extend([
                Capability::ReadProject,
                Capability::ReadProjectAgentMessages,
            ]);
        }
        // A direction to a delegated child's run reads the project snapshot for
        // the task, the child's session metadata for its active run, and the run
        // revision, so all three reads are part of the verb.
        Verb::ProjectSend { .. } => {
            capabilities.extend([
                Capability::ReadProject,
                Capability::ReadAgentSession,
                Capability::ReadAgentRun,
                Capability::ControlAgentRun,
            ]);
        }
        // The manager session id of the control comes from the project
        // snapshot, so the ability to read that snapshot is part of the verb.
        Verb::ProjectChild { .. } => {
            capabilities.extend([Capability::ReadProject, Capability::ControlProjectChild]);
        }
    }
    CapabilitySet::new(capabilities)
}

/// One connection's requests, remembering the id of the first one and whether
/// any answer was an idempotent replay.
struct Caller<'a, C: Client> {
    client: &'a mut C,
    primary: Option<RequestId>,
    replayed: bool,
}

impl<'a, C: Client> Caller<'a, C> {
    fn new(client: &'a mut C) -> Self {
        Self {
            client,
            primary: None,
            replayed: false,
        }
    }

    /// Whether the worker served any request of this invocation from its
    /// idempotency cache, which happens only when a mutating request repeated a
    /// request id it had already answered.
    fn replayed(&self) -> bool {
        self.replayed
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
        if response.replayed {
            self.replayed = true;
        }
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

/// How many of the sessions an ambiguous prefix matches are listed before the
/// message counts the rest.
const AMBIGUOUS_MATCHES: usize = 5;

/// One short session id the command line spelled, with the form a listing is
/// compared in.
struct Prefix {
    /// The argument as the caller typed it, which an error reports.
    raw: String,
    /// The hex digits of `raw`, lowercased and without hyphens.
    hex: String,
}

/// One session the worker's listing reported.
struct ListedSession {
    id: AgentSessionId,
    name: String,
}

/// Resolves every short session id of `verb` to the full id it names.
///
/// A command line that spells only full ids returns without sending a request,
/// which is the path every existing caller keeps taking.
async fn resolve_sessions<C: Client>(client: &mut C, verb: &mut Verb) -> Result<(), Failure> {
    let mut wanted: Vec<Prefix> = Vec::new();
    verb.for_each_session_mut(&mut |session| {
        if !session.is_prefix() {
            return;
        }
        let hex = session.hex();
        if wanted.iter().any(|prefix| prefix.hex == hex) {
            return;
        }
        wanted.push(Prefix {
            raw: session.raw().to_owned(),
            hex,
        });
    });
    if wanted.is_empty() {
        return Ok(());
    }
    let mut caller = Caller::new(client);
    negotiate_for_resolution(&mut caller, verb).await?;
    let sessions = list_sessions(&mut caller).await?;
    let request_id = caller.primary;
    let mut matched: Vec<(String, AgentSessionId)> = Vec::new();
    for prefix in &wanted {
        match matching(&prefix.hex, &sessions).as_slice() {
            [] => return Err(no_session_matches(request_id, &prefix.raw)),
            [one] => matched.push((prefix.hex.clone(), one.id)),
            several => return Err(ambiguous_prefix(prefix, several)),
        }
    }
    verb.for_each_session_mut(&mut |session| {
        if !session.is_prefix() {
            return;
        }
        let hex = session.hex();
        if let Some((_, id)) = matched.iter().find(|(known, _)| *known == hex) {
            session.resolve(*id);
        }
    });
    Ok(())
}

/// Negotiates the capabilities the listing needs on top of the verb's own, so a
/// verb that never reads sessions itself, such as `session archive`, can still
/// resolve a prefix.
async fn negotiate_for_resolution<C: Client>(
    caller: &mut Caller<'_, C>,
    verb: &Verb,
) -> Result<(), Failure> {
    let capabilities = CapabilitySet::new(
        used_capabilities(verb)
            .iter()
            .copied()
            .chain(std::iter::once(Capability::ReadAgentSession)),
    );
    let (request_id, response) = caller
        .call(ClientRequest::Control(ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities,
        }))
        .await?;
    match response {
        ServerResponse::Control(ControlResponse::Negotiated(_)) => Ok(()),
        _ => Err(Failure::unexpected(request_id, "negotiate")),
    }
}

/// Every session the worker's workspaces hold, archived sessions included: a
/// prefix may name an archived session, which is what `session delete` takes.
async fn list_sessions<C: Client>(
    caller: &mut Caller<'_, C>,
) -> Result<Vec<ListedSession>, Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces))
        .await?;
    let workspaces = match response {
        ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces }) => workspaces,
        _ => return Err(Failure::unexpected(request_id, "workspace listing")),
    };
    let mut sessions = Vec::new();
    for workspace in workspaces {
        let (request_id, response) = caller
            .call(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id: workspace.id,
                    include_archived: true,
                },
            ))
            .await?;
        match response {
            ServerResponse::Session(SessionResponse::AgentSessions { sessions: listed }) => {
                sessions.extend(listed.into_iter().map(|session| ListedSession {
                    id: session.id,
                    name: session.name,
                }));
            }
            _ => return Err(Failure::unexpected(request_id, "workspace sessions")),
        }
    }
    Ok(sessions)
}

/// The sessions whose id starts with `prefix`, which is already hex.
fn matching<'a>(prefix: &str, sessions: &'a [ListedSession]) -> Vec<&'a ListedSession> {
    sessions
        .iter()
        .filter(|session| session_hex(&session.id.to_string()).starts_with(prefix))
        .collect()
}

/// The failure of a prefix no session starts with. It reports the code and the
/// exit code of an unknown full id, so a caller handles both the same way.
fn no_session_matches(request_id: Option<RequestId>, raw: &str) -> Failure {
    Failure::Server {
        request_id: request_id.unwrap_or_default(),
        error: LoomError::new(
            ErrorCode::NotFound,
            format!(
                "no session starts with '{raw}'; 'loom-cli ls --workspace <workspace-id>' lists a \
                 workspace's sessions"
            ),
            false,
        ),
    }
}

/// The failure of a prefix several sessions start with: a usage error that
/// names them, because only the caller can say which one was meant.
fn ambiguous_prefix(prefix: &Prefix, sessions: &[&ListedSession]) -> Failure {
    let mut listed: Vec<&ListedSession> = sessions.to_vec();
    listed.sort_by_key(|session| session.id);
    let names: Vec<String> = listed
        .iter()
        .take(AMBIGUOUS_MATCHES)
        .map(|session| {
            if session.name.is_empty() {
                session.id.to_string()
            } else {
                format!("{} ({})", session.id, session.name)
            }
        })
        .collect();
    let rest = listed.len().saturating_sub(AMBIGUOUS_MATCHES);
    let rest = match rest {
        0 => String::new(),
        count => format!(", and {count} more"),
    };
    Failure::Usage(format!(
        "session id prefix '{}' is ambiguous: {} sessions match: {}{rest}; pass more characters",
        prefix.raw,
        listed.len(),
        names.join(", "),
    ))
}

/// The resolved id of one session input. Resolution runs before the dispatch
/// below, so this only reports an input the resolver does not visit.
fn session_id(session: &SessionRef) -> Result<AgentSessionId, Failure> {
    session.full().ok_or_else(|| {
        Failure::Internal(format!(
            "the session id '{}' was not resolved before the verb ran",
            session.raw()
        ))
    })
}

/// Runs one verb and returns the payload a `--json` envelope reports, plus the
/// id of its primary request.
pub(crate) async fn run<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Outcome, Failure> {
    // A short session id is resolved against the worker's sessions before the
    // verb runs, so every session input accepts a unique prefix. The resolved
    // copy is what the dispatch below reads, and a command line of full ids
    // dispatches unchanged without a listing request.
    let mut resolved = invocation.clone();
    resolve_sessions(client, &mut resolved.verb).await?;
    let invocation = &resolved;
    match &invocation.verb {
        Verb::Doctor => query::doctor(client, invocation, out).await,
        Verb::Node => operator::node(client, invocation, out).await,
        Verb::WhoAmI => query::whoami(client, invocation, out).await,
        Verb::Ls {
            workspace,
            archived,
        } => query::ls(client, invocation, *workspace, *archived, out).await,
        Verb::Get { session, metadata } => {
            query::get(client, invocation, session_id(session)?, *metadata, out).await
        }
        Verb::Events(args) => events::events(client, invocation, args, out, err).await,
        Verb::Peek(args) => events::peek(client, invocation, args, out, err).await,
        Verb::Run(args) => run::run(client, invocation, args, out, err).await,
        Verb::Send {
            run,
            text,
            request_id,
        } => supervise::send(client, invocation, *run, text, *request_id, out).await,
        Verb::Answer {
            run,
            text,
            request_id,
        } => supervise::answer(client, invocation, *run, text, *request_id, out).await,
        Verb::Approve {
            run,
            tool_call,
            request_id,
        } => supervise::approve(client, invocation, *run, *tool_call, *request_id, out).await,
        Verb::Deny {
            run,
            tool_call,
            reason,
            request_id,
        } => {
            supervise::deny(
                client,
                invocation,
                *run,
                *tool_call,
                reason.as_deref(),
                *request_id,
                out,
            )
            .await
        }
        Verb::Stop {
            run,
            archive,
            retry_on_conflict,
            request_id,
        } => {
            let archive = archive.as_ref().map(session_id).transpose()?;
            supervise::stop(
                client,
                invocation,
                *run,
                archive,
                *retry_on_conflict,
                *request_id,
                out,
            )
            .await
        }
        Verb::Wait {
            session,
            until,
            timeout_secs,
        } => {
            supervise::wait(
                client,
                invocation,
                session_id(session)?,
                *until,
                *timeout_secs,
                out,
            )
            .await
        }
        Verb::Status {
            session,
            format,
            tree,
        } => {
            supervise::status(
                client,
                invocation,
                session_id(session)?,
                *format,
                *tree,
                out,
            )
            .await
        }
        Verb::Liveness { session } => {
            supervise::liveness(client, invocation, session_id(session)?, out).await
        }
        Verb::Result { session, task } => {
            supervise::result(client, invocation, session_id(session)?, *task, out).await
        }
        Verb::RetentionGet => operator::retention_get(client, invocation, out).await,
        Verb::RetentionSet(args) => operator::retention_set(client, invocation, args, out).await,
        Verb::ProvidersList => providers::list(client, invocation, out).await,
        Verb::ProvidersSetKey(args) => providers::set_key(client, invocation, args, out).await,
        Verb::ProvidersAdd(args) => providers::add(client, invocation, args, out).await,
        Verb::ProvidersRemove { provider } => {
            providers::remove(client, invocation, provider, out).await
        }
        Verb::ProvidersDiscover { provider } => {
            providers::discover(client, invocation, provider, out).await
        }
        Verb::JevStatus(flags) => jev::status(client, invocation, flags, out).await,
        Verb::JevAsk(args) => jev::ask(client, invocation, args, out).await,
        Verb::JevTriage(args) => jev::triage(client, invocation, args, out).await,
        Verb::ModelsList { provider } => {
            providers::models(client, invocation, provider.as_deref(), out).await
        }
        Verb::SessionArchive {
            session,
            yes,
            request_id,
        } => {
            operator::archive(
                client,
                invocation,
                session_id(session)?,
                false,
                *yes,
                *request_id,
                out,
            )
            .await
        }
        Verb::SessionDelete {
            session,
            force,
            yes,
            request_id,
        } => {
            operator::delete(
                client,
                invocation,
                session_id(session)?,
                *force,
                *yes,
                false,
                *request_id,
                out,
            )
            .await
        }
        Verb::ProjectArchive {
            session,
            request_id,
        } => {
            operator::archive(
                client,
                invocation,
                session_id(session)?,
                true,
                false,
                *request_id,
                out,
            )
            .await
        }
        Verb::ProjectDelete {
            session,
            force,
            yes,
            request_id,
        } => {
            operator::delete(
                client,
                invocation,
                session_id(session)?,
                *force,
                *yes,
                true,
                *request_id,
                out,
            )
            .await
        }
        Verb::ProjectWorktrees { session } => {
            operator::worktrees(client, invocation, session_id(session)?, out).await
        }
        Verb::ProjectWorktreeGet { session, task } => {
            operator::worktree_get(client, invocation, session_id(session)?, *task, out).await
        }
        Verb::ProjectWorktreeDiff { session, task } => {
            operator::worktree_diff(client, invocation, session_id(session)?, *task, out).await
        }
        Verb::ProjectWorktreeIntegrate {
            session,
            task,
            expected_parent_revision,
            request_id,
        } => {
            operator::worktree_integrate(
                client,
                invocation,
                session_id(session)?,
                *task,
                expected_parent_revision.as_deref(),
                *request_id,
                out,
            )
            .await
        }
        Verb::ProjectWorktreeCleanup {
            session,
            task,
            disposition,
            request_id,
        } => {
            operator::worktree_cleanup(
                client,
                invocation,
                session_id(session)?,
                *task,
                *disposition,
                *request_id,
                out,
            )
            .await
        }
        Verb::Delete {
            session,
            force,
            yes,
            request_id,
        } => {
            operator::delete(
                client,
                invocation,
                session_id(session)?,
                *force,
                *yes,
                false,
                *request_id,
                out,
            )
            .await
        }
        Verb::ProjectGet { session } => {
            project::get(client, invocation, session_id(session)?, out).await
        }
        Verb::ProjectTree { session } => {
            project::tree(client, invocation, session_id(session)?, out).await
        }
        Verb::ProjectTasks { session } => {
            project::tasks(client, invocation, session_id(session)?, out).await
        }
        Verb::ProjectChildren { session } => {
            project::children(client, invocation, session_id(session)?, out).await
        }
        Verb::ProjectMessages {
            session,
            after,
            limit,
        } => {
            project::messages(
                client,
                invocation,
                session_id(session)?,
                *after,
                *limit,
                out,
            )
            .await
        }
        Verb::ProjectSend {
            session,
            text,
            request_id,
        } => {
            project::send(
                client,
                invocation,
                session_id(session)?,
                text,
                *request_id,
                out,
            )
            .await
        }
        Verb::ProjectModelsGet { session } => {
            child_models::get(client, invocation, session_id(session)?, out).await
        }
        Verb::ProjectModelsSet {
            session,
            changes,
            request_id,
        } => {
            child_models::set(
                client,
                invocation,
                session_id(session)?,
                changes,
                *request_id,
                out,
            )
            .await
        }
        Verb::ProjectChild {
            session,
            task,
            action,
            request_id,
        } => {
            project::child(
                client,
                invocation,
                session_id(session)?,
                *task,
                *action,
                *request_id,
                out,
            )
            .await
        }
    }
}

/// The negotiated capabilities as their snake_case protocol names, in the
/// order the protocol's own set orders them.
pub(crate) fn capability_names(capabilities: &CapabilitySet) -> Vec<String> {
    capabilities
        .iter()
        .map(|capability| {
            serde_json::to_value(capability)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default()
        })
        .collect()
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

/// A deterministic id derived from `base` and `purpose`.
///
/// The protocol's `RequestId` is a UUID, so the pair is hashed into a valid
/// version-4-shaped UUID. The derivation only has to be stable for the retry
/// horizon of the worker's idempotency cache, which is what makes a repeated
/// command with one base idempotent: `run --new` derives its session creation
/// and its input attachments from one base, and `stop --archive` derives the
/// archive from the interrupt's key, so no two mutations of one command share
/// an id and a replay performs neither twice. `None` has no base to derive
/// from and answers a fresh id, which is the behaviour of a command without
/// `--request-id`.
pub(super) fn derived_id(base: Option<RequestId>, purpose: &str) -> RequestId {
    let Some(base) = base else {
        return RequestId::new();
    };
    let label = format!("{base}/{purpose}");
    let first = fnv1a(label.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let second = fnv1a(label.as_bytes(), 0x8422_2325_cbf2_9ce4);
    let mut digits: Vec<char> = format!("{first:016x}{second:016x}").chars().collect();
    // Version 4 and the RFC 4122 variant, so the value is a well-formed UUID
    // that an older worker's request-id handling accepts.
    digits[12] = '4';
    digits[16] = '8';
    let text: String = digits.into_iter().collect();
    format!(
        "{}-{}-{}-{}-{}",
        &text[0..8],
        &text[8..12],
        &text[12..16],
        &text[16..20],
        &text[20..32]
    )
    .parse()
    .expect("a derived id is a well-formed UUID")
}

fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
