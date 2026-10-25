//! The operator verbs: worker node health, archive retention, and deletion.
//!
//! `node` and `retention get` are reads. `retention set` and `delete` are
//! operator-level mutations that the token's capabilities already gate. The
//! policy is worker-wide, so `retention set` reports it as stored and says the
//! next sweep applies it rather than claiming an immediate deletion.

use std::io::Write;

use loom_core::{AgentSessionId, ProjectSnapshot};
use loom_protocol::{
    ArchiveRetentionPolicy, ClientRequest, ControlRequest, ControlResponse, ProjectRequest,
    ProjectResponse, ServerResponse, SessionRequest, SessionResponse,
};
use serde_json::{Value, json};

use crate::cli::args::{Invocation, RetentionSetArgs};
use crate::cli::client::Client;

use super::{Caller, Failure, Outcome, capability_names, emit, handshake};

/// `node`: the worker's id, name, and the health its status carries.
pub(super) async fn node<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Control(ControlRequest::GetWorkerNodeStatus))
        .await?;
    let node = match response {
        ServerResponse::Control(ControlResponse::WorkerNodeStatus(node)) => node,
        _ => return Err(Failure::unexpected(request_id, "worker node status")),
    };
    if !invocation.globals.json {
        let capabilities = capability_names(&node.capabilities);
        emit(out, &format!("node_id {}\n", node.node_id))?;
        emit(out, &format!("name {}\n", node.name))?;
        emit(out, &format!("online {}\n", node.online))?;
        emit(
            out,
            &format!(
                "capabilities {} {}\n",
                capabilities.len(),
                capabilities.join(",")
            ),
        )?;
        emit(out, &format!("cpu_count {}\n", node.resources.cpu_count))?;
        emit(
            out,
            &format!(
                "cpu_usage_percent {}\n",
                optional(node.resources.cpu_usage_percent)
            ),
        )?;
        emit(
            out,
            &format!(
                "memory_usage_percent {}\n",
                optional(node.resources.memory_usage_percent)
            ),
        )?;
        emit(
            out,
            &format!(
                "memory_total_bytes {}\n",
                optional(node.resources.memory_total_bytes)
            ),
        )?;
        emit(
            out,
            &format!(
                "memory_available_bytes {}\n",
                optional(node.resources.memory_available_bytes)
            ),
        )?;
        emit(
            out,
            &format!(
                "disk_total_bytes {}\n",
                optional(node.resources.disk_total_bytes)
            ),
        )?;
        emit(
            out,
            &format!(
                "disk_available_bytes {}\n",
                optional(node.resources.disk_available_bytes)
            ),
        )?;
    }
    Ok(Outcome::new(
        serde_json::to_value(&node).unwrap_or(Value::Null),
        caller.primary,
    ))
}

/// `retention get`: the policy the worker stored.
pub(super) async fn retention_get<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Control(
            ControlRequest::GetArchiveRetentionPolicy,
        ))
        .await?;
    let policy = match response {
        ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(policy)) => policy,
        _ => return Err(Failure::unexpected(request_id, "archive retention policy")),
    };
    if !invocation.globals.json {
        emit_policy(out, &policy)?;
    }
    Ok(Outcome::new(policy_value(&policy), caller.primary))
}

/// `retention set`: merge the given fields into the stored policy and report
/// what the worker now holds.
pub(super) async fn retention_set<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &RetentionSetArgs,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Control(
            ControlRequest::GetArchiveRetentionPolicy,
        ))
        .await?;
    let mut policy = match response {
        ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(policy)) => policy,
        _ => return Err(Failure::unexpected(request_id, "archive retention policy")),
    };
    if let Some(window_ms) = args.window_ms {
        policy.retention_ms = window_ms;
    }
    if let Some(force_discard_worktrees) = args.force_discard_worktrees {
        policy.force_discard_worktrees = force_discard_worktrees;
    }
    if let Some(sub_agent_ms) = args.sub_agent_ms {
        policy.sub_agent_retention_ms = sub_agent_ms;
    }
    let (request_id, response) = caller
        .call(ClientRequest::Control(
            ControlRequest::SetArchiveRetentionPolicy { policy },
        ))
        .await?;
    let stored = match response {
        ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(policy)) => policy,
        _ => return Err(Failure::unexpected(request_id, "archive retention policy")),
    };
    if !invocation.globals.json {
        emit_policy(out, &stored)?;
        emit(out, "stored for this worker; the next sweep applies it\n")?;
    }
    let mut data = policy_value(&stored);
    data["applies_from"] = json!("next_sweep");
    Ok(Outcome::new(data, caller.primary))
}

/// `delete`: remove one archived session, or a project root with its tree.
///
/// The project snapshot is read first so the command can require `--yes` for a
/// cascade and report the worktrees a force deletion discards before it sends
/// the irreversible request. A non-archived session is still refused by the
/// worker, which is the authority on which sessions may be deleted.
pub(super) async fn delete<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session_id: AgentSessionId,
    force: bool,
    yes: bool,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession { session_id },
        ))
        .await?;
    let project: ProjectSnapshot = match response {
        ServerResponse::Project(ProjectResponse::ProjectSnapshot(project)) => project,
        _ => return Err(Failure::unexpected(request_id, "project snapshot")),
    };
    // Only a project root cascades, and the worker applies the same rule: a
    // root owns another member or at least one delegated task.
    let cascades = project.root_session_id == session_id
        && (project.agents.len() > 1 || !project.tasks.is_empty());
    let mut targets = vec![session_id];
    if cascades {
        for agent in &project.agents {
            if agent.session_id != session_id {
                targets.push(agent.session_id);
            }
        }
    }
    if targets.len() > 1 && !yes {
        return Err(Failure::Usage(format!(
            "delete would remove {} sessions because this project root cascades; pass --yes to \
             confirm",
            targets.len()
        )));
    }
    let worktrees = if force {
        project
            .worktrees
            .iter()
            .filter(|worktree| {
                targets.contains(&worktree.parent_session_id)
                    || targets.contains(&worktree.child_session_id)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let (request_id, response) = caller
        .call(ClientRequest::Session(SessionRequest::DeleteAgentSession {
            session_id,
            force,
        }))
        .await?;
    match response {
        ServerResponse::Session(SessionResponse::AgentSessionDeleted {
            session_id: deleted,
        }) if deleted == session_id => {}
        _ => return Err(Failure::unexpected(request_id, "session deletion")),
    }
    if !invocation.globals.json {
        for target in &targets {
            emit(out, &format!("deleted {target}\n"))?;
        }
        if force {
            if worktrees.is_empty() {
                emit(out, "discarded 0 worktrees\n")?;
            } else {
                for worktree in &worktrees {
                    emit(
                        out,
                        &format!(
                            "discarded worktree {} {}\n",
                            worktree.worktree_name, worktree.relative_path
                        ),
                    )?;
                }
            }
        }
    }
    let data = json!({
        "session_id": session_id.to_string(),
        "deleted_sessions": targets.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "force": force,
        "discarded_worktrees": worktrees
            .iter()
            .map(|worktree| json!({
                "name": worktree.worktree_name,
                "path": worktree.relative_path,
            }))
            .collect::<Vec<_>>(),
    });
    Ok(Outcome::new(data, caller.primary))
}

/// A compact duration for the text projection: `off` when the window is
/// disabled, otherwise the largest whole unit the parser accepts, falling back
/// to milliseconds.
fn window_name(window_ms: Option<u64>) -> String {
    let Some(window_ms) = window_ms.filter(|window_ms| *window_ms > 0) else {
        return "off".to_owned();
    };
    for (unit, size) in [
        ("d", 86_400_000_u64),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1_000),
    ] {
        if window_ms % size == 0 {
            return format!("{}{unit}", window_ms / size);
        }
    }
    format!("{window_ms}ms")
}

fn policy_value(policy: &ArchiveRetentionPolicy) -> Value {
    json!({
        "retention_ms": policy.retention_ms,
        "force_discard_worktrees": policy.force_discard_worktrees,
        "sub_agent_retention_ms": policy.sub_agent_retention_ms,
        "window": window_name(policy.retention_ms),
        "sub_agent": window_name(policy.sub_agent_retention_ms),
    })
}

fn emit_policy(out: &mut dyn Write, policy: &ArchiveRetentionPolicy) -> Result<(), Failure> {
    emit(
        out,
        &format!("window {}\n", window_name(policy.retention_ms)),
    )?;
    emit(
        out,
        &format!("force-discard {}\n", policy.force_discard_worktrees),
    )?;
    emit(
        out,
        &format!("sub-agent {}\n", window_name(policy.sub_agent_retention_ms)),
    )?;
    Ok(())
}

fn optional<T: std::fmt::Display>(value: Option<T>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::window_name;

    #[test]
    fn window_names_use_the_largest_whole_unit_and_off_for_disabled() {
        assert_eq!(window_name(None), "off");
        assert_eq!(window_name(Some(0)), "off");
        assert_eq!(window_name(Some(604_800_000)), "7d");
        assert_eq!(window_name(Some(1_209_600_000)), "14d");
        assert_eq!(window_name(Some(7_200_000)), "2h");
        assert_eq!(window_name(Some(1_500)), "1500ms");
        assert_eq!(window_name(Some(600_000)), "10m");
    }
}
