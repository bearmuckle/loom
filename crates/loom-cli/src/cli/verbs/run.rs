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
//!   safe to repeat. An answer served from that cache is reported as
//!   `replayed: true` in the `--json` envelope.
//!
//! - `--project <session-id>` makes the session a sub project of that project
//!   root, the same link loom-ui's "New sub project" records: the project is
//!   checked to be a live root of `--workspace`
//!   (`Workspace/ListWorkspaceSessions` and
//!   `Project/GetProjectSnapshotForSession`), a session with the resolved name
//!   is reused only when its snapshot names that project as its parent, and a
//!   new one is created with `Project/CreateSubProject`, which is a retryable
//!   mutation exactly like the workspace creation it replaces. A sub project
//!   this invocation created inherits the project's folders, then the
//!   repositories no inherited folder covers, unless `--repo` or
//!   `--as-directory` names its input; a source that cannot be inherited is a
//!   warning on stderr, because the new project is still usable.
//!
//! The verb returns as soon as `AgentRunStarted` arrives. It never polls and
//! never waits for the run: `loom-cli events --follow` is the verb that watches
//! one.
//!
//! The model is the caller's `--model` when one was given. Otherwise the verb
//! sends `Provider/ListProviders` - the same read `providers list` sends - and
//! starts the first provider, in the order the worker returned them, that has a
//! credential and a known default model, selected as
//! `loom:model:v1:["<provider-id>","<model>"]`. A worker that offers no such
//! provider, or that refuses the catalog read as a missing capability, falls
//! back to `deterministic/demo` with one warning on stderr, because a run with
//! the demo model is better than no run; a transport failure is still a
//! failure, because the worker was never reached.

use std::io::Write;

use loom_core::{
    AgentSessionId, ApprovalPolicy, ContextAssemblyOptions, ErrorCode, RequestId, SessionLimits,
};
use loom_core::{AgentSessionSnapshot, LoomError, ProjectSnapshot};
use loom_model::ModelId;
use loom_protocol::{
    ClientRequest, FilesystemRequest, FilesystemResponse, ProjectRequest, ProjectResponse,
    ProviderRequest, ProviderResponse, RepositoryRequest, RepositoryResponse, RunRequest,
    RunResponse, ServerResponse, SessionRequest, SessionResponse, WorkspaceRequest,
};
use serde_json::json;

use crate::cli::args::{ApprovalPreset, DEFAULT_MODEL, Invocation, RunArgs};
use crate::cli::client::Client;
use crate::cli::text::run_state_name;

use super::{Caller, Failure, Outcome, derived_id, emit, handshake, providers};

/// `run`: resolve the model and the session, attach the inputs to a session this
/// invocation created, pin the approval policy, and start the run.
pub(super) async fn run<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &RunArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;

    let model = resolve_model(&mut caller, args, err).await?;
    let resolved = match args.project {
        Some(project) => resolve_sub_project(&mut caller, args, project).await?,
        None => resolve_session(&mut caller, args).await?,
    };
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
        // A sub project starts where its project is, unless the caller named
        // its input: the same inheritance loom-ui's "New sub project" applies.
        if let Some(project) = args.project
            && args.repository.is_none()
            && args.directory.is_none()
        {
            inherit_project_sources(&mut caller, args, project, session_id, err).await?;
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
                model: model.clone(),
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
        if let Some(project) = args.project {
            emit(out, &format!("parent {project}\n"))?;
        }
    }
    // A session `run` places in a project is a sub project: its own project id
    // is its session id, and `parent_id` is the project it belongs to.
    let project = args.project.map(|parent| {
        json!({
            "id": session_id.to_string(),
            "parent_id": parent.to_string(),
        })
    });
    let data = json!({
        "session": { "id": session_id.to_string(), "name": session_name },
        "run": { "id": started.id.to_string(), "state": state },
        "project": project,
        "created": created,
        "request_id": request_id.to_string(),
        "request_ids": {
            "session": resolved.session_request_id.map(|id| id.to_string()),
            "run": request_id.to_string(),
        },
    });
    Ok(Outcome::new(data, Some(request_id)).with_replayed(caller.replayed()))
}

/// The model the run starts: `--model` when the caller named one, otherwise the
/// first provider the worker returned that has a credential and a known default
/// model.
///
/// `Provider/ListProviders` is the same read `providers list` and `models list`
/// send, and the selection is the same default those verbs mark. The tie-break is
/// the order the worker returned the providers in, so two runs against one
/// worker select the same model.
///
/// A worker that refuses the read as a missing capability, and a worker where no
/// credential-bearing provider has a default model, both fall back to
/// `deterministic/demo` with one warning on stderr: the demo model runs, so
/// failing the whole command would cost more than saying what happened. A
/// transport failure is not caught, because the worker was never reached and
/// the run would fail the same way.
async fn resolve_model<C: Client>(
    caller: &mut Caller<'_, C>,
    args: &RunArgs,
    err: &mut dyn Write,
) -> Result<ModelId, Failure> {
    if let Some(model) = &args.model {
        return Ok(model.clone());
    }
    let (request_id, response) = match caller
        .call(ClientRequest::Provider(ProviderRequest::ListProviders))
        .await
    {
        Ok(answer) => answer,
        Err(Failure::Server { error, .. }) if error.code == ErrorCode::CapabilityDenied => {
            return fallback(err, "the worker does not offer its provider catalog");
        }
        Err(other) => return Err(other),
    };
    let providers = match response {
        ServerResponse::Provider(ProviderResponse::Providers { providers }) => providers,
        _ => return Err(Failure::unexpected(request_id, "provider list")),
    };
    match providers::default_selection(&providers) {
        Some(model) => Ok(model),
        None => fallback(err, "no provider with a credential has a default model"),
    }
}

/// Starts the demo model and says on stderr why no provider was selected.
fn fallback(err: &mut dyn Write, reason: &str) -> Result<ModelId, Failure> {
    writeln!(
        err,
        "warning: {reason}; starting the run with '{DEFAULT_MODEL}'"
    )
    .map_err(|error| Failure::Internal(format!("could not write a warning: {error}")))?;
    Ok(ModelId::new(DEFAULT_MODEL))
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
    let (_, sessions) = workspace_sessions(caller, args).await?;
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

/// The sub project of `project` that `run` starts in: the first live session of
/// the workspace with the resolved name whose project names `project` as its
/// parent, or the sub project this invocation creates. `--new` creates one
/// unconditionally, but still checks the project first, so a mistyped id is a
/// clear refusal rather than a failed creation.
async fn resolve_sub_project<C: Client>(
    caller: &mut Caller<'_, C>,
    args: &RunArgs,
    project: AgentSessionId,
) -> Result<Resolved, Failure> {
    let (list_id, sessions) = workspace_sessions(caller, args).await?;
    if !sessions.iter().any(|session| session.id == project) {
        return Err(Failure::Server {
            request_id: list_id,
            error: LoomError::new(
                ErrorCode::NotFound,
                format!(
                    "--project {project} is not a live session of workspace {}; \
                     'loom-cli ls --workspace {}' lists them",
                    args.workspace, args.workspace
                ),
                false,
            ),
        });
    }
    let (_, snapshot) = project_snapshot(caller, project).await?;
    if snapshot.root_session_id != project {
        return Err(Failure::Usage(format!(
            "--project {project} is an agent of project {}, not a project root; \
             pass --project {}",
            snapshot.project_id, snapshot.root_session_id
        )));
    }
    if !args.new {
        for session in sessions
            .iter()
            .filter(|session| session.name == args.name && session.id != project)
        {
            let (_, candidate) = project_snapshot(caller, session.id).await?;
            let parent = candidate.parent_project_id.map(|id| *id.as_uuid());
            if candidate.root_session_id == session.id && parent == Some(*project.as_uuid()) {
                return Ok(Resolved {
                    session_id: session.id,
                    session_name: session.name.clone(),
                    created: false,
                    session_request_id: None,
                });
            }
        }
    }
    let request_id = if args.new {
        derived_id(args.request_id, "session")
    } else {
        RequestId::new()
    };
    let (_, response) = caller
        .call_with_id(
            request_id,
            ClientRequest::Project(ProjectRequest::CreateSubProject {
                parent_session_id: project,
                name: args.name.clone(),
            }),
        )
        .await?;
    match response {
        ServerResponse::Project(ProjectResponse::SubProjectCreated { session, .. }) => {
            Ok(Resolved {
                session_id: session.id,
                session_name: session.name,
                created: true,
                session_request_id: Some(request_id),
            })
        }
        _ => Err(Failure::unexpected(request_id, "sub project creation")),
    }
}

/// The workspace's live sessions, and the id of the request that listed them.
async fn workspace_sessions<C: Client>(
    caller: &mut Caller<'_, C>,
    args: &RunArgs,
) -> Result<(RequestId, Vec<AgentSessionSnapshot>), Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id: args.workspace,
                include_archived: false,
            },
        ))
        .await?;
    match response {
        ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => {
            Ok((request_id, sessions))
        }
        _ => Err(Failure::unexpected(request_id, "workspace sessions")),
    }
}

/// The project that covers `session_id`, and the id of the request that read it.
async fn project_snapshot<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
) -> Result<(RequestId, ProjectSnapshot), Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession { session_id },
        ))
        .await?;
    match response {
        ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)) => {
            Ok((request_id, snapshot))
        }
        _ => Err(Failure::unexpected(request_id, "project snapshot")),
    }
}

/// Attaches the project's folders, then the repositories no inherited folder
/// covers, to the sub project this invocation created.
///
/// This is the inheritance loom-ui applies to a sub project: a mounted folder
/// already carries the checkouts inside it, so a repository under one is not
/// cloned a second time, and a repository is attached from its own source with
/// the worker's existing clone reused. A source that cannot be attached is a
/// warning, because the new project is still usable and a person can add the
/// source later; a failure to reach the worker is still a failure.
async fn inherit_project_sources<C: Client>(
    caller: &mut Caller<'_, C>,
    args: &RunArgs,
    project: AgentSessionId,
    session_id: AgentSessionId,
    err: &mut dyn Write,
) -> Result<(), Failure> {
    let mut mounted = Vec::new();
    let (request_id, response) = caller
        .call(ClientRequest::Filesystem(
            FilesystemRequest::ListSessionDirectories {
                session_id: project,
            },
        ))
        .await?;
    let directories = match response {
        ServerResponse::Filesystem(FilesystemResponse::SessionDirectories { directories }) => {
            directories
        }
        _ => return Err(Failure::unexpected(request_id, "session directories")),
    };
    for directory in directories {
        let attached = caller
            .call_with_id(
                auxiliary_id(args, &format!("inherit-directory/{}", directory.path)),
                ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory {
                    session_id,
                    source: directory.source.clone(),
                    path: directory.path.clone(),
                }),
            )
            .await;
        match attached {
            Ok((
                _,
                ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached { .. }),
            )) => {
                mounted.push(directory.path);
            }
            Ok((request_id, _)) => {
                return Err(Failure::unexpected(request_id, "session directory attach"));
            }
            Err(Failure::Server { error, .. }) => {
                warn(
                    err,
                    &format!(
                        "could not inherit folder '{}' of project {project}: {}",
                        directory.source, error.message
                    ),
                )?;
            }
            Err(other) => return Err(other),
        }
    }
    let (request_id, response) = caller
        .call(ClientRequest::Repository(
            RepositoryRequest::ListSessionRepositories {
                session_id: project,
            },
        ))
        .await?;
    let repositories = match response {
        ServerResponse::Repository(RepositoryResponse::SessionRepositories { repositories }) => {
            repositories
        }
        _ => return Err(Failure::unexpected(request_id, "session repositories")),
    };
    for repository in repositories {
        if mounted
            .iter()
            .any(|mount| path_is_within(&repository.path, mount))
        {
            continue;
        }
        let attached = caller
            .call_with_id(
                auxiliary_id(args, &format!("inherit-repository/{}", repository.path)),
                ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
                    session_id,
                    source: repository.source.clone(),
                    path: repository.path.clone(),
                    revision: None,
                    reuse_local: true,
                }),
            )
            .await;
        match attached {
            Ok((
                _,
                ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(_)),
            )) => {}
            Ok((request_id, _)) => {
                return Err(Failure::unexpected(request_id, "session repository attach"));
            }
            Err(Failure::Server { error, .. }) => {
                warn(
                    err,
                    &format!(
                        "could not inherit repository '{}' of project {project}: {}",
                        repository.source, error.message
                    ),
                )?;
            }
            Err(other) => return Err(other),
        }
    }
    Ok(())
}

/// Whether a session path is a mounted folder or lies inside one.
fn path_is_within(path: &str, mount: &str) -> bool {
    path == mount || path.starts_with(&format!("{mount}/"))
}

/// Writes one warning line on stderr.
fn warn(err: &mut dyn Write, message: &str) -> Result<(), Failure> {
    writeln!(err, "warning: {message}")
        .map_err(|error| Failure::Internal(format!("could not write a warning: {error}")))
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
