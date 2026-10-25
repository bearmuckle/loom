//! `run`: start one agent run in a workspace session, without waiting for it.
//!
//! The verb is the client's only mutation, and it is built so a repeated
//! invocation is safe:
//!
//! - The session is resolved by name from
//!   `Workspace/ListWorkspaceSessions { include_archived: false }`. A session
//!   with the resolved name is reused, and only when none exists is
//!   `Workspace/CreateAgentSessionInWorkspace` sent.
//! - The inputs (`--repo`, `--as-directory`) are attached only when *this*
//!   invocation created the session. A replay therefore neither duplicates an
//!   attachment nor fails on one that already exists.
//! - The approval policy is pinned with `Session/SetSessionApprovalPolicy`
//!   before the run starts, because a run resolves its policy once, when it is
//!   created.
//! - The run start (`Run/StartSessionAgentRunWithOptions`) carries
//!   `--request-id` as its request id, so the same id is answered from the
//!   worker's durable idempotency cache with the same run instead of starting a
//!   second one. The idempotency key is the run start alone: session creation,
//!   the attachments, and the policy change use fresh ids, which is why they are
//!   safe to repeat.
//!
//! The verb returns as soon as `AgentRunStarted` arrives. It never polls and
//! never waits for the run: `loom-cli events --follow` is the verb that watches
//! one.

use std::io::Write;

use loom_core::{AgentSessionId, ApprovalPolicy, ContextAssemblyOptions, RequestId, SessionLimits};
use loom_protocol::{
    ClientRequest, FilesystemRequest, FilesystemResponse, RepositoryRequest, RepositoryResponse,
    RunRequest, RunResponse, ServerResponse, SessionRequest, SessionResponse, WorkspaceRequest,
};
use serde_json::{Value, json};

use crate::cli::args::{ApprovalPreset, Invocation, RunArgs};
use crate::cli::client::Client;
use crate::cli::text::run_state_name;

use super::{Caller, Failure, emit, handshake};

/// `run`: resolve the session, attach the inputs to a session this invocation
/// created, pin the approval policy, and start the run.
pub(super) async fn run<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &RunArgs,
    out: &mut dyn Write,
) -> Result<(Value, Option<RequestId>), Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;

    let (session_id, session_name, created) = resolve_session(&mut caller, args).await?;

    // Attach only what a session this invocation created needs, so a replay of
    // the same command neither attaches a second time nor collides with what is
    // already there.
    if created {
        if let Some(repository) = &args.repository {
            let (request_id, response) = caller
                .call(ClientRequest::Repository(
                    RepositoryRequest::AttachSessionRepository {
                        session_id,
                        source: repository.source.clone(),
                        path: repository.path.clone(),
                        revision: repository.revision.clone(),
                        reuse_local: false,
                    },
                ))
                .await?;
            match response {
                ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(_)) => {}
                _ => {
                    return Err(Failure::unexpected(request_id, "session repository attach"));
                }
            }
        }
        if let Some(directory) = &args.directory {
            let (request_id, response) = caller
                .call(ClientRequest::Filesystem(
                    FilesystemRequest::AttachSessionDirectory {
                        session_id,
                        source: directory.source.clone(),
                        path: directory.path.clone(),
                    },
                ))
                .await?;
            match response {
                ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached {
                    ..
                }) => {}
                _ => {
                    return Err(Failure::unexpected(request_id, "session directory attach"));
                }
            }
        }
    }

    // A run resolves its approval policy when it is created, so the policy has
    // to be stored before the run start rather than after it.
    let (policy, auto_approve_actions) = match args.approval {
        ApprovalPreset::Default => (ApprovalPolicy::default(), false),
        ApprovalPreset::Auto => (ApprovalPolicy::auto_approve(), true),
    };
    let (request_id, response) = caller
        .call(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                auto_approve_actions: Some(auto_approve_actions),
            },
        ))
        .await?;
    match response {
        ServerResponse::Session(SessionResponse::ApprovalPolicy(_)) => {}
        _ => return Err(Failure::unexpected(request_id, "session approval policy")),
    }

    // The caller's id, when it named one, is the idempotency key of this run
    // start: the worker answers a repeated id with the run it already started.
    let request_id = args.request_id.unwrap_or_default();
    let (_, response) = caller
        .call_with_id(
            request_id,
            ClientRequest::Run(RunRequest::StartSessionAgentRunWithOptions {
                session_id,
                task: args.task.clone(),
                model: args.model.clone(),
                system_instructions: None,
                repository_instructions: None,
                limits: SessionLimits::default(),
                context: ContextAssemblyOptions::default(),
            }),
        )
        .await?;
    let started = match response {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot,
        _ => return Err(Failure::unexpected(request_id, "run start")),
    };

    let state = run_state_name(started.state);
    if !invocation.globals.json {
        emit(out, &format!("session {session_id} {session_name}\n"))?;
        emit(out, &format!("run {} {state}\n", started.id))?;
    }
    let data = json!({
        "session": { "id": session_id.to_string(), "name": session_name },
        "run": { "id": started.id.to_string(), "state": state },
        "created": created,
        "request_id": request_id.to_string(),
    });
    Ok((data, Some(request_id)))
}

/// The session `run` starts in: the first of the workspace's live sessions with
/// the resolved name, or the one this invocation creates.
async fn resolve_session<C: Client>(
    caller: &mut Caller<'_, C>,
    args: &RunArgs,
) -> Result<(AgentSessionId, String, bool), Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id: args.workspace,
                include_archived: false,
            },
        ))
        .await?;
    let sessions = match response {
        ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => sessions,
        _ => return Err(Failure::unexpected(request_id, "workspace sessions")),
    };
    if let Some(session) = sessions
        .iter()
        .find(|session| session.name == args.name)
        .cloned()
    {
        return Ok((session.id, session.name, false));
    }
    let (request_id, response) = caller
        .call(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: args.workspace,
                name: args.name.clone(),
            },
        ))
        .await?;
    match response {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => {
            Ok((session.id, session.name, true))
        }
        _ => Err(Failure::unexpected(request_id, "session creation")),
    }
}
