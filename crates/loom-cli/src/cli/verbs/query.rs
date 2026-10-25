//! The verbs that report worker and catalog state: `doctor`, `whoami`, `ls`,
//! and `get`.

use std::io::Write;

use loom_core::{AgentSessionId, AgentSessionSnapshot, WorkspaceId};
use loom_protocol::{
    AccessRequest, AccessResponse, ActivityDetail, AgentSessionSnapshotProjection, ClientRequest,
    ControlRequest, ControlResponse, ParticipantIdentity, ServerResponse, SessionRequest,
    SessionResponse, WorkspaceRequest, WorkspaceResponse,
};
use serde_json::{Value, json};

use crate::cli::args::Invocation;
use crate::cli::client::Client;
use crate::cli::text::{run_state_name, session_state_name};

use super::Caller;
use super::{Failure, Outcome, capability_names, emit, handshake};

/// `doctor`: discovery, negotiation, the participant, and the worker node.
pub(super) async fn doctor<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    // Discovery first, so a worker that does not answer capability discovery at
    // all fails before anything is negotiated against it.
    let (request_id, response) = caller
        .call(ClientRequest::Control(ControlRequest::DiscoverCapabilities))
        .await?;
    let discovered = match response {
        ServerResponse::Control(ControlResponse::Capabilities(discovered)) => discovered,
        _ => return Err(Failure::unexpected(request_id, "capability discovery")),
    };
    let negotiated = handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Access(AccessRequest::GetCurrentParticipant))
        .await?;
    let participant = match response {
        ServerResponse::Access(AccessResponse::CurrentParticipant { participant }) => participant,
        _ => return Err(Failure::unexpected(request_id, "participant")),
    };
    let (request_id, response) = caller
        .call(ClientRequest::Control(ControlRequest::GetWorkerNodeStatus))
        .await?;
    let node = match response {
        ServerResponse::Control(ControlResponse::WorkerNodeStatus(node)) => node,
        _ => return Err(Failure::unexpected(request_id, "worker node status")),
    };

    let capabilities = capability_names(&negotiated.capabilities);
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "protocol {}.{}\n",
                negotiated.protocol_version.major, negotiated.protocol_version.minor
            ),
        )?;
        emit(
            out,
            &format!(
                "capabilities {} {}\n",
                capabilities.len(),
                capabilities.join(",")
            ),
        )?;
        emit(out, &participant_line(participant.as_ref()))?;
        emit(out, &format!("node {} {}\n", node.name, node.node_id))?;
    }

    let data = json!({
        "protocol": {
            "major": negotiated.protocol_version.major,
            "minor": negotiated.protocol_version.minor,
        },
        "capabilities": capabilities,
        "participant": participant_value(participant.as_ref()),
        "node": serde_json::to_value(&node).unwrap_or(Value::Null),
        "discovered": capability_names(&discovered.capabilities),
    });
    Ok(Outcome::new(data, caller.primary))
}

/// `whoami`: the participant this connection acts as.
pub(super) async fn whoami<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Access(AccessRequest::GetCurrentParticipant))
        .await?;
    let participant = match response {
        ServerResponse::Access(AccessResponse::CurrentParticipant { participant }) => participant,
        _ => return Err(Failure::unexpected(request_id, "participant")),
    };
    if !invocation.globals.json {
        emit(out, &participant_line(participant.as_ref()))?;
    }
    Ok(Outcome::new(
        json!({ "participant": participant_value(participant.as_ref()) }),
        caller.primary,
    ))
}

/// `ls`: every workspace, or the sessions of one workspace.
pub(super) async fn ls<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    workspace: Option<WorkspaceId>,
    archived: bool,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let data = match workspace {
        None => {
            let (request_id, response) = caller
                .call(ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces))
                .await?;
            let workspaces = match response {
                ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces }) => {
                    workspaces
                }
                _ => return Err(Failure::unexpected(request_id, "workspace listing")),
            };
            if !invocation.globals.json {
                for record in &workspaces {
                    emit(out, &format!("{} {}\n", record.id, record.name))?;
                }
            }
            json!({ "workspaces": serde_json::to_value(&workspaces).unwrap_or(Value::Null) })
        }
        Some(workspace_id) => {
            let (request_id, response) = caller
                .call(ClientRequest::Workspace(
                    WorkspaceRequest::ListWorkspaceSessions {
                        workspace_id,
                        include_archived: archived,
                    },
                ))
                .await?;
            let sessions = match response {
                ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => sessions,
                _ => return Err(Failure::unexpected(request_id, "workspace sessions")),
            };
            if !invocation.globals.json {
                for session in &sessions {
                    emit(
                        out,
                        &format!(
                            "{} {} {}\n",
                            session.id,
                            session_state_name(session.state),
                            session.name
                        ),
                    )?;
                }
            }
            json!({
                "workspace_id": workspace_id.to_string(),
                "sessions": serde_json::to_value(&sessions).unwrap_or(Value::Null),
            })
        }
    };
    Ok(Outcome::new(data, caller.primary))
}

/// `get`: one session, and with `--metadata` its active run.
pub(super) async fn get<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    session: AgentSessionId,
    metadata: bool,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Session(SessionRequest::GetAgentSession {
            session_id: session,
        }))
        .await?;
    let snapshot: AgentSessionSnapshot = match response {
        ServerResponse::Session(SessionResponse::AgentSession(snapshot)) => snapshot,
        _ => return Err(Failure::unexpected(request_id, "session")),
    };
    if !invocation.globals.json {
        emit(
            out,
            &format!(
                "{} {} {}\n",
                snapshot.id,
                session_state_name(snapshot.state),
                snapshot.name
            ),
        )?;
    }
    if !metadata {
        return Ok(Outcome::new(
            serde_json::to_value(&snapshot).unwrap_or(Value::Null),
            caller.primary,
        ));
    }
    let (request_id, response) = caller
        .call(ClientRequest::Session(
            SessionRequest::GetAgentSessionSnapshotMetadata {
                session_id: session,
                activity_detail: ActivityDetail::Full,
            },
        ))
        .await?;
    let projection: AgentSessionSnapshotProjection = match response {
        ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection)) => *projection,
        _ => return Err(Failure::unexpected(request_id, "session metadata")),
    };
    if !invocation.globals.json
        && let Some(active) = &projection.active_run
    {
        emit(
            out,
            &format!(
                "run {} {}\n",
                active.run.id,
                run_state_name(active.run.state)
            ),
        )?;
    }
    Ok(Outcome::new(
        serde_json::to_value(&projection).unwrap_or(Value::Null),
        caller.primary,
    ))
}

/// `operator` for a connection that is not bound to a person, otherwise the
/// person's display name and stable id.
fn participant_line(participant: Option<&ParticipantIdentity>) -> String {
    match participant {
        None => "participant operator\n".to_owned(),
        Some(participant) => format!(
            "participant {} {}\n",
            participant.display_name, participant.id
        ),
    }
}

fn participant_value(participant: Option<&ParticipantIdentity>) -> Value {
    match participant {
        None => Value::Null,
        Some(participant) => serde_json::to_value(participant).unwrap_or(Value::Null),
    }
}
