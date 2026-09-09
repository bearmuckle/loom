use loom_agent::{AgentEvent, AgentRunSnapshot, AgentRunState};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, Capability, CapabilitySet,
    EventSequence, ProjectId, ProtocolVersion, RunId, Timestamp,
};
use loom_model::ModelId;
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, RequestEnvelope, ResponseEnvelope, ServerEvent,
    ServerEventEnvelope, ServerResponse, decode_event, decode_request, decode_response,
    encode_event, encode_request, encode_response,
};

#[test]
fn request_json_round_trip_preserves_typed_envelope() {
    let request = RequestEnvelope::new(ClientRequest::Negotiate {
        client_version: CURRENT_PROTOCOL_VERSION,
        capabilities: CapabilitySet::new([
            Capability::CreateAgentSession,
            Capability::SubscribeSessionEvents,
        ]),
    });

    let encoded = encode_request(&request).unwrap();
    let decoded = decode_request(&encoded).unwrap();

    assert_eq!(decoded, request);
}

#[test]
fn response_json_round_trip_preserves_structured_errors() {
    let response = ResponseEnvelope::failure(
        loom_core::RequestId::new(),
        loom_core::LoomError::invalid_request("name is required"),
    );

    let encoded = encode_response(&response).unwrap();
    let decoded = decode_response(&encoded).unwrap();

    assert_eq!(decoded, response);
}

#[test]
fn event_json_round_trip_preserves_sequence_and_session() {
    let session_id = AgentSessionId::new();
    let snapshot = AgentSessionSnapshot {
        id: session_id,
        project_id: ProjectId::new(),
        name: "Protocol fixture".to_owned(),
        state: AgentSessionState::Idle,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(1),
    };
    let event = ServerEventEnvelope {
        protocol_version: ProtocolVersion::new(1, 0),
        sequence: EventSequence::new(7),
        session_id,
        event: ServerEvent::AgentSessionCreated { snapshot },
    };

    let encoded = encode_event(&event).unwrap();
    let decoded = decode_event(&encoded).unwrap();

    assert_eq!(decoded, event);
}

#[test]
fn response_can_carry_model_list_without_provider_specific_types() {
    let response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::Models { models: Vec::new() },
    );

    let encoded = encode_response(&response).unwrap();
    let decoded = decode_response(&encoded).unwrap();

    assert_eq!(decoded, response);
}

#[test]
fn agent_event_json_round_trip_preserves_run_identity() {
    let run = AgentRunSnapshot {
        id: RunId::new(),
        session_id: AgentSessionId::new(),
        task: "inspect the workspace".to_owned(),
        model: ModelId::new("deterministic/demo"),
        state: AgentRunState::Executing,
        started_at: Timestamp::from_unix_millis(2),
        updated_at: Timestamp::from_unix_millis(3),
        completed_at: None,
        summary: None,
    };
    let event = ServerEventEnvelope {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(8),
        session_id: run.session_id,
        event: ServerEvent::Agent {
            event: AgentEvent::RunStarted {
                snapshot: run.clone(),
            },
        },
    };

    let encoded = encode_event(&event).unwrap();
    let decoded = decode_event(&encoded).unwrap();

    assert_eq!(decoded, event);
    assert_eq!(run.state, AgentRunState::Executing);
}
