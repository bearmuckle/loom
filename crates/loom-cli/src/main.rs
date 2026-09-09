use std::env;

use loom_core::{AgentSessionState, Capability, CapabilitySet, LoomError, ProjectId};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, RequestEnvelope, ServerEvent, ServerResponse,
};
use loom_server::{InProcessBackend, InProcessConnection};

fn main() -> Result<(), LoomError> {
    let session_name = session_name(env::args().skip(1))?;
    let Some(session_name) = session_name else {
        return Ok(());
    };
    let backend = InProcessBackend::new();
    let connection = backend.connect();

    negotiate(&connection)?;
    let snapshot = create_session(&connection, session_name)?;
    let events = read_events(&connection, snapshot.id)?;

    println!("Loom native shell");
    println!(
        "Connected in-process using protocol {}.{}",
        CURRENT_PROTOCOL_VERSION.major, CURRENT_PROTOCOL_VERSION.minor
    );
    println!(
        "Session {}: {} [{}]",
        snapshot.id,
        snapshot.name,
        state_name(snapshot.state)
    );
    println!("Event stream:");
    for event in events {
        match event.event {
            ServerEvent::AgentSessionCreated { snapshot } => println!(
                "  #{} session_created -> {} [{}]",
                event.sequence,
                snapshot.id,
                state_name(snapshot.state)
            ),
            ServerEvent::AgentSessionStateChanged { current, .. } => println!(
                "  #{} session_state_changed -> [{}]",
                event.sequence,
                state_name(current)
            ),
        }
    }

    Ok(())
}

fn negotiate(connection: &InProcessConnection) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
        client_version: CURRENT_PROTOCOL_VERSION,
        capabilities: CapabilitySet::new([
            Capability::CreateAgentSession,
            Capability::ReadAgentSession,
            Capability::SubscribeSessionEvents,
            Capability::JsonProtocol,
        ]),
    }));
    match response.result? {
        ServerResponse::Negotiated(_) => Ok(()),
        response => Err(LoomError::new(
            loom_core::ErrorCode::Internal,
            format!("backend returned unexpected negotiation response: {response:?}"),
            false,
        )),
    }
}

fn create_session(
    connection: &InProcessConnection,
    name: String,
) -> Result<loom_core::AgentSessionSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
        project_id: ProjectId::new(),
        name,
    }));
    match response.result? {
        ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
        response => Err(LoomError::new(
            loom_core::ErrorCode::Internal,
            format!("backend returned unexpected session response: {response:?}"),
            false,
        )),
    }
}

fn read_events(
    connection: &InProcessConnection,
    session_id: loom_core::AgentSessionId,
) -> Result<Vec<loom_protocol::ServerEventEnvelope>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
        session_id: Some(session_id),
        after_sequence: None,
    }));
    match response.result? {
        ServerResponse::SessionEvents { events } => Ok(events),
        response => Err(LoomError::new(
            loom_core::ErrorCode::Internal,
            format!("backend returned unexpected event response: {response:?}"),
            false,
        )),
    }
}

fn session_name(mut args: impl Iterator<Item = String>) -> Result<Option<String>, LoomError> {
    let mut name = String::from("New session");
    while let Some(argument) = args.next() {
        if argument == "--name" {
            name = args
                .next()
                .ok_or_else(|| LoomError::invalid_request("--name requires a value"))?;
        } else if let Some(value) = argument.strip_prefix("--name=") {
            name = value.to_owned();
        } else if argument == "--help" || argument == "-h" {
            println!("Usage: loom [--name <session-name>]");
            return Ok(None);
        } else {
            return Err(LoomError::invalid_request(format!(
                "unknown argument '{argument}'"
            )));
        }
    }
    Ok(Some(name))
}

const fn state_name(state: AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Idle => "idle",
        AgentSessionState::Queued => "queued",
        AgentSessionState::Planning => "planning",
        AgentSessionState::AwaitingApproval => "awaiting_approval",
        AgentSessionState::Executing => "executing",
        AgentSessionState::Evaluating => "evaluating",
        AgentSessionState::NeedsInput => "needs_input",
        AgentSessionState::Completed => "completed",
        AgentSessionState::Failed => "failed",
        AgentSessionState::Cancelled => "cancelled",
    }
}
