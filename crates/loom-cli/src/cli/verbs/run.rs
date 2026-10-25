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
//!   created. A session this invocation creates gets the requested preset (or
//!   the default one), while a reused session is only changed when the flag was
//!   given, so a plain run does not overwrite a policy a person set.
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
use serde_json::json;

use crate::cli::args::{ApprovalPreset, Invocation, RunArgs};
use crate::cli::client::Client;
use crate::cli::text::run_state_name;

use super::{Caller, Failure, Outcome, emit, handshake};

/// `run`: resolve the session, attach the inputs to a session this invocation
/// created, pin the approval policy, and start the run.
pub(super) async fn run<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &RunArgs,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;

    let resolved = resolve_session(&mut caller, args).await?;
    let session_id = resolved.session_id;
    let session_name = resolved.session_name;
    let created = resolved.created;

    // Attach only what a session this invocation created needs, so a replay of
    // the same command neither attaches a second time nor collides with what is
    // already there. A `--new` command with a request id derives these ids from
    // that base too, so its replay is idempotent even though the replayed
    // creation response still says the session was created.
    if created {
        if let Some(repository) = &args.repository {
            let request_id = auxiliary_id(args, "repository");
            let (_, response) = caller
                .call_with_id(
                    request_id,
                    ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
                        session_id,
                        source: repository.source.clone(),
                        path: repository.path.clone(),
                        revision: repository.revision.clone(),
                        reuse_local: false,
                    }),
                )
                .await?;
            match response {
                ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(_)) => {}
                _ => {
                    return Err(Failure::unexpected(request_id, "session repository attach"));
                }
            }
        }
        if let Some(directory) = &args.directory {
            let request_id = auxiliary_id(args, "directory");
            let (_, response) = caller
                .call_with_id(
                    request_id,
                    ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory {
                        session_id,
                        source: directory.source.clone(),
                        path: directory.path.clone(),
                    }),
                )
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

    // A run resolves its approval policy when it is created, so a policy change
    // has to be stored before the run start. A session this invocation creates
    // starts with the requested preset or the default one; a reused session
    // keeps whatever policy a person set unless `--approval-policy` named one,
    // so a plain `run` against an existing session never overwrites it.
    let preset = if created {
        Some(args.approval.unwrap_or(ApprovalPreset::Default))
    } else {
        args.approval
    };
    if let Some(preset) = preset {
        let (policy, auto_approve_actions) = match preset {
            ApprovalPreset::Default => (ApprovalPolicy::default(), false),
            ApprovalPreset::Auto => (ApprovalPolicy::auto_approve(), true),
        };
        let (request_id, response) = caller
            .call_with_id(
                auxiliary_id(args, "policy"),
                ClientRequest::Session(SessionRequest::SetSessionApprovalPolicy {
                    session_id,
                    policy,
                    auto_approve_actions: Some(auto_approve_actions),
                }),
            )
            .await?;
        match response {
            ServerResponse::Session(SessionResponse::ApprovalPolicy(_)) => {}
            _ => return Err(Failure::unexpected(request_id, "session approval policy")),
        }
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
        "request_ids": {
            "session": resolved.session_request_id.map(|id| id.to_string()),
            "run": request_id.to_string(),
        },
    });
    Ok(Outcome::new(data, Some(request_id)))
}

/// The session `run` starts in: the first of the workspace's live sessions with
/// the resolved name, or the one this invocation creates. `--new` creates one
/// unconditionally and never lists the workspace.
async fn resolve_session<C: Client>(
    caller: &mut Caller<'_, C>,
    args: &RunArgs,
) -> Result<Resolved, Failure> {
    if args.new {
        return create_session(caller, args, derived_id(args.request_id, "session")).await;
    }
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
        return Ok(Resolved {
            session_id: session.id,
            session_name: session.name,
            created: false,
            session_request_id: None,
        });
    }
    create_session(caller, args, RequestId::new()).await
}

/// Creates the session unconditionally and reports the id the creation used.
async fn create_session<C: Client>(
    caller: &mut Caller<'_, C>,
    args: &RunArgs,
    request_id: RequestId,
) -> Result<Resolved, Failure> {
    let (_, response) = caller
        .call_with_id(
            request_id,
            ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: args.workspace,
                name: args.name.clone(),
            }),
        )
        .await?;
    match response {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => Ok(Resolved {
            session_id: session.id,
            session_name: session.name,
            created: true,
            session_request_id: Some(request_id),
        }),
        _ => Err(Failure::unexpected(request_id, "session creation")),
    }
}

/// What `run` resolved: the session, whether this invocation created it, and
/// the id a creation used.
struct Resolved {
    session_id: AgentSessionId,
    session_name: String,
    created: bool,
    session_request_id: Option<RequestId>,
}

/// The id of one auxiliary mutation. A plain command uses a fresh id, as slice
/// one did; a `--new` command with `--request-id` derives every mutation from
/// that base, so repeating it replays instead of attaching a second input.
fn auxiliary_id(args: &RunArgs, purpose: &str) -> RequestId {
    if args.new {
        derived_id(args.request_id, purpose)
    } else {
        RequestId::new()
    }
}

/// A deterministic id derived from `base` and `purpose`.
///
/// The protocol's `RequestId` is a UUID, so the pair is hashed into a valid
/// version-4-shaped UUID. The derivation only has to be stable for the retry
/// horizon of the worker's idempotency cache, which is what makes `run --new`
/// repeatable with one base: the session creation and the run start never
/// share an id.
fn derived_id(base: Option<RequestId>, purpose: &str) -> RequestId {
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
