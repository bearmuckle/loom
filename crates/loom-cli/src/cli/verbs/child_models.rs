//! The project's child model policy: `project models get|set`, and the reads
//! and writes `run`, `project get`, and `status --tree` share.
//!
//! A change is a read-modify-write: the verb reads the stored policy, applies
//! only the entries its flags name, and replaces the policy with the result, so
//! a command never drops a kind it did not mention. The worker validates every
//! model against its configured providers before it stores anything.

use std::io::Write;

use loom_core::{
    AgentSessionId, ErrorCode, LoomError, ProjectChildModelPolicy, ProjectId, RequestId,
};
use loom_protocol::{ClientRequest, ProjectRequest, ProjectResponse, ServerResponse};
use serde_json::{Value, json};

use crate::cli::args::{ChildModelChanges, Invocation};
use crate::cli::client::Client;
use crate::cli::text::escape;

use super::{Caller, Failure, Outcome, derived_id, emit, handshake};

/// `project models get`: the policy of the project any member addresses.
///
/// The policy is stored for the project root, so a member session is resolved
/// to its root first: the read names the root the policy belongs to, and the
/// printed project id is the root's.
pub(super) async fn get<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let root = root_session(&mut caller, session_id).await?;
    let (_, project_id, policy) = read(&mut caller, root).await?;
    if !invocation.globals.json {
        emit(out, &format!("project {project_id}\n"))?;
        emit_policy(out, &policy)?;
    }
    Ok(Outcome::new(value(project_id, &policy), caller.primary))
}

/// The root session of the project `session_id` belongs to.
///
/// A project is addressed by any of its member sessions, but only the root
/// stores its child model policy, so `project models get` resolves the session
/// first. A session the worker does not know names no project root, and the
/// error says so instead of reporting the policy of a made-up project.
async fn root_session<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
) -> Result<AgentSessionId, Failure> {
    match caller
        .call(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession { session_id },
        ))
        .await
    {
        Ok((_, ServerResponse::Project(ProjectResponse::ProjectSnapshot(project)))) => {
            Ok(project.root_session_id)
        }
        Ok((request_id, _)) => Err(Failure::unexpected(request_id, "project snapshot")),
        Err(failure) => Err(failure),
    }
    .map_err(|failure| match failure {
        Failure::Server { request_id, error } if error.code == ErrorCode::NotFound => {
            Failure::Server {
                request_id,
                error: LoomError::new(
                    ErrorCode::NotFound,
                    format!(
                        "'{session_id}' does not name a session, so it names no project root; \
                         'loom-cli ls' lists the sessions"
                    ),
                    false,
                ),
            }
        }
        other => other,
    })
}

/// `project models set`: apply the flags to the stored policy of the project
/// root and print what the worker stored.
pub(super) async fn set<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    changes: &ChildModelChanges,
    request_id: Option<RequestId>,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (project_id, policy) = change(
        &mut caller,
        session_id,
        changes,
        request_id.unwrap_or_default(),
    )
    .await?;
    if !invocation.globals.json {
        emit(out, &format!("project {project_id}\n"))?;
        emit_policy(out, &policy)?;
    }
    Ok(
        Outcome::new(value(project_id, &policy), request_id.or(caller.primary))
            .with_replayed(caller.replayed()),
    )
}

/// Applies `changes` to the policy of the project rooted at `session_id`,
/// sending the replacement under `request_id`.
pub(super) async fn change<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
    changes: &ChildModelChanges,
    request_id: RequestId,
) -> Result<(ProjectId, ProjectChildModelPolicy), Failure> {
    let stored = if changes.clear {
        ProjectChildModelPolicy::default()
    } else {
        read(caller, session_id).await?.2
    };
    let (request_id, response) = caller
        .call_with_id(
            request_id,
            ClientRequest::Project(ProjectRequest::SetProjectChildModels {
                session_id,
                policy: changes.apply(stored),
            }),
        )
        .await?;
    match response {
        ServerResponse::Project(ProjectResponse::ProjectChildModels { project_id, policy }) => {
            Ok((project_id, policy))
        }
        _ => Err(Failure::unexpected(request_id, "project child models")),
    }
}

/// The id a `run` sends its child model change under: derived from the run's
/// own `--request-id` when it has one, so a replayed run replays the change.
pub(super) fn run_change_id(request_id: Option<RequestId>) -> RequestId {
    derived_id(request_id, "child-models")
}

/// Reads the policy of the project `session_id` belongs to.
pub(super) async fn read<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
) -> Result<(RequestId, ProjectId, ProjectChildModelPolicy), Failure> {
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::GetProjectChildModels { session_id },
        ))
        .await?;
    match response {
        ServerResponse::Project(ProjectResponse::ProjectChildModels { project_id, policy }) => {
            Ok((request_id, project_id, policy))
        }
        _ => Err(Failure::unexpected(request_id, "project child models")),
    }
}

/// The policy for a report that also stands without it: a worker that answers
/// the read with an error (an older worker, or one without durable storage)
/// leaves it out instead of failing the whole report.
pub(super) async fn optional<C: Client>(
    caller: &mut Caller<'_, C>,
    session_id: AgentSessionId,
) -> Result<Option<ProjectChildModelPolicy>, Failure> {
    match read(caller, session_id).await {
        Ok((_, _, policy)) => Ok(Some(policy)),
        Err(Failure::Server { .. }) => Ok(None),
        Err(other) => Err(other),
    }
}

/// The text lines of a policy: `child-model default <id|->`, then one
/// `child-model kind <kind> <id>` line per kind in name order.
pub(super) fn emit_policy(
    out: &mut dyn Write,
    policy: &ProjectChildModelPolicy,
) -> Result<(), Failure> {
    let default_model = policy
        .default_model_id
        .as_deref()
        .map(escape)
        .unwrap_or_else(|| "-".to_owned());
    emit(out, &format!("child-model default {default_model}\n"))?;
    for (kind, model) in &policy.by_kind {
        emit(
            out,
            &format!("child-model kind {} {}\n", escape(kind), escape(model)),
        )?;
    }
    Ok(())
}

/// The JSON of a policy: the default and the per-kind map.
pub(super) fn policy_value(policy: &ProjectChildModelPolicy) -> Value {
    json!({
        "default_model_id": policy.default_model_id,
        "by_kind": policy.by_kind,
    })
}

fn value(project_id: ProjectId, policy: &ProjectChildModelPolicy) -> Value {
    json!({
        "project_id": project_id.to_string(),
        "child_models": policy_value(policy),
    })
}
