use loom_agent::{AgentEvent, AgentPlanStep, AgentRunSnapshot, AgentRunState};
use loom_context::ContextAssemblyOptions;
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, Capability, CapabilitySet,
    EventSequence, ProjectId, ProtocolVersion, RunId, SessionLimits, Timestamp,
};
use loom_model::ModelId;
use loom_process::{TaskKind, TaskSpec};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientFrame, ClientRequest, RequestEnvelope, ResponseEnvelope,
    ServerEvent, ServerEventEnvelope, ServerFrame, ServerResponse, decode_client_frame,
    decode_event, decode_request, decode_response, decode_server_frame, encode_client_frame,
    encode_event, encode_request, encode_response, encode_server_frame,
};
use loom_workspace::{WorkspaceEdit, WorkspaceSnapshot};

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
        evidence: Vec::new(),
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

#[test]
fn m2_workspace_and_task_requests_round_trip_without_untyped_envelopes() {
    let request = RequestEnvelope::new(ClientRequest::ApplyWorkspaceEdit {
        project_id: ProjectId::new(),
        edit: WorkspaceEdit {
            path: "src/lib.rs".to_owned(),
            old_text: "old".to_owned(),
            new_text: "new".to_owned(),
            expected_revision: Some("revision".to_owned()),
        },
    });
    let decoded = decode_request(&encode_request(&request).unwrap()).unwrap();
    assert_eq!(decoded, request);

    let task = TaskSpec {
        kind: TaskKind::Test,
        label: "contract task".to_owned(),
        command: "printf".to_owned(),
        args: vec!["ok".to_owned()],
        cwd: None,
        output_limit_bytes: Some(128),
        artifact_paths: vec!["target/result.txt".to_owned()],
    };
    let response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::WorkspaceSnapshot(WorkspaceSnapshot {
            project_id: ProjectId::new(),
            root: "/workspace".to_owned(),
            captured_at: Timestamp::from_unix_millis(4),
            entries: Vec::new(),
        }),
    );
    let decoded_response = decode_response(&encode_response(&response).unwrap()).unwrap();
    assert_eq!(decoded_response, response);
    assert_eq!(task.kind, TaskKind::Test);
}

#[test]
fn m3_run_options_provider_and_context_contracts_round_trip() {
    let request = RequestEnvelope::new(ClientRequest::StartAgentRunWithOptions {
        session_id: AgentSessionId::new(),
        task: "durable task".to_owned(),
        model: ModelId::new("deterministic/demo"),
        workspace_root: "/workspace".to_owned(),
        system_instructions: Some("system".to_owned()),
        repository_instructions: Some("repository".to_owned()),
        limits: SessionLimits {
            max_tool_calls: Some(3),
            max_cost_micros: Some(10_000),
            ..Default::default()
        },
        context: ContextAssemblyOptions {
            context_window: Some(4_096),
            max_input_tokens: Some(2_048),
            reserved_output_tokens: Some(512),
        },
    });
    assert_eq!(
        decode_request(&encode_request(&request).unwrap()).unwrap(),
        request
    );

    let response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::Providers {
            providers: Vec::new(),
        },
    );
    assert_eq!(
        decode_response(&encode_response(&response).unwrap()).unwrap(),
        response
    );
}

#[test]
fn m4_capability_discovery_and_version_migration_are_additive() {
    let request = RequestEnvelope::with_version(
        ProtocolVersion::new(1, 7),
        ClientRequest::DiscoverCapabilities,
    );
    assert_eq!(
        decode_request(&encode_request(&request).unwrap()).unwrap(),
        request
    );
    assert!(
        request
            .protocol_version
            .is_compatible_with(CURRENT_PROTOCOL_VERSION)
    );
    assert!(!ProtocolVersion::new(2, 0).is_compatible_with(CURRENT_PROTOCOL_VERSION));
}

#[test]
fn m4_transport_frames_preserve_typed_envelopes() {
    let request = ClientFrame::Request(Box::new(RequestEnvelope::new(
        ClientRequest::DiscoverCapabilities,
    )));
    assert_eq!(
        decode_client_frame(&encode_client_frame(&request).unwrap()).unwrap(),
        request
    );

    let response = ServerFrame::Response(Box::new(ResponseEnvelope::failure(
        loom_core::RequestId::new(),
        loom_core::LoomError::malformed_payload("fixture"),
    )));
    assert_eq!(
        decode_server_frame(&encode_server_frame(&response).unwrap()).unwrap(),
        response
    );
}

#[test]
fn m5_session_run_review_and_evidence_contracts_round_trip() {
    let project_id = ProjectId::new();
    let sessions = RequestEnvelope::new(ClientRequest::ListAgentSessions {
        project_id: Some(project_id),
        include_archived: false,
    });
    assert_eq!(
        decode_request(&encode_request(&sessions).unwrap()).unwrap(),
        sessions
    );
    let message = RequestEnvelope::new(ClientRequest::SendAgentMessage {
        run_id: RunId::new(),
        message: "continue with validation".to_owned(),
    });
    assert_eq!(
        decode_request(&encode_request(&message).unwrap()).unwrap(),
        message
    );
    let changes = RequestEnvelope::new(ClientRequest::GetWorkspaceChanges {
        project_id,
        after_sequence: Some(EventSequence::new(4)),
    });
    assert_eq!(
        decode_request(&encode_request(&changes).unwrap()).unwrap(),
        changes
    );
    let tasks = RequestEnvelope::new(ClientRequest::ListTasks { project_id });
    assert_eq!(
        decode_request(&encode_request(&tasks).unwrap()).unwrap(),
        tasks
    );
    let response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::Projects {
            projects: vec![loom_protocol::ProjectSnapshot {
                id: project_id,
                name: "workspace".to_owned(),
                root: Some("/workspace".to_owned()),
                session_count: 1,
                updated_at: Some(Timestamp::from_unix_millis(5)),
            }],
        },
    );
    assert_eq!(
        decode_response(&encode_response(&response).unwrap()).unwrap(),
        response
    );
    let run = AgentRunSnapshot {
        id: RunId::new(),
        session_id: AgentSessionId::new(),
        task: "review".to_owned(),
        model: ModelId::new("deterministic/demo"),
        state: AgentRunState::Completed,
        started_at: Timestamp::from_unix_millis(6),
        updated_at: Timestamp::from_unix_millis(7),
        completed_at: Some(Timestamp::from_unix_millis(8)),
        summary: Some("done".to_owned()),
        evidence: Vec::new(),
    };
    let run_snapshot = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::AgentRunSnapshot(loom_protocol::AgentRunSnapshotProjection {
            run,
            plan: vec![AgentPlanStep {
                id: "validate".to_owned(),
                description: "Validate the change".to_owned(),
            }],
            messages: Vec::new(),
            pending_approval: None,
            pending_input: None,
            usage: Default::default(),
        }),
    );
    assert_eq!(
        decode_response(&encode_response(&run_snapshot).unwrap()).unwrap(),
        run_snapshot
    );
    let evidence = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::TaskEvidence {
            evidence: Vec::new(),
        },
    );
    assert_eq!(
        decode_response(&encode_response(&evidence).unwrap()).unwrap(),
        evidence
    );
}
