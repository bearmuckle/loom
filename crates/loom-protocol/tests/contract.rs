use loom_core::{
    ActivityId, AgentSessionId, AgentSessionSnapshot, AgentSessionState, Capability, CapabilitySet,
    EventSequence, InteractionId, ProtocolVersion, RunId, SessionLimits, StepId, Timestamp,
    WorkspaceId,
};
use loom_model::{ModelId, ToolCall};
use loom_protocol::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, AgentEvent,
    AgentPlanStep, AgentRunSnapshot, AgentRunState, CURRENT_PROTOCOL_VERSION, ClientFrame,
    ClientRequest, ContextAssemblyOptions, FileActivityOperation, GitHubCopilotLoginStatus,
    RequestEnvelope, ResponseEnvelope, ServerEvent, ServerEventEnvelope, ServerFrame,
    ServerResponse, SessionFilesystemSnapshot, TaskKind, TaskSpec, ToolResult, WorkerNodeResources,
    WorkerNodeStatus, WorkspaceConfig, WorkspaceEdit, WorkspaceRecord, decode_client_frame,
    decode_event, decode_request, decode_response, decode_server_frame, encode_client_frame,
    encode_event, encode_request, encode_response, encode_server_frame,
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

    let approval_settings = RequestEnvelope::new(ClientRequest::SetSessionApprovalPolicy {
        session_id: AgentSessionId::new(),
        policy: loom_core::ApprovalPolicy::default(),
        auto_approve_actions: Some(false),
    });
    assert_eq!(
        decode_request(&encode_request(&approval_settings).unwrap()).unwrap(),
        approval_settings
    );
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
fn browser_copilot_login_round_trips_without_access_tokens() {
    let request = RequestEnvelope::new(ClientRequest::StartGitHubCopilotLogin);
    assert_eq!(
        request.request.required_capability(),
        Some(Capability::ConfigureProviders)
    );
    assert_eq!(
        decode_request(&encode_request(&request).unwrap()).unwrap(),
        request
    );

    let response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::GitHubCopilotLoginStarted {
            login_id: "login-1".to_owned(),
            user_code: "ABCD-EFGH".to_owned(),
            verification_uri: "https://github.com/login/device".to_owned(),
            expires_in: 900,
            interval: 5,
        },
    );
    assert_eq!(
        decode_response(&encode_response(&response).unwrap()).unwrap(),
        response
    );

    let status_request = RequestEnvelope::new(ClientRequest::GetGitHubCopilotLoginStatus {
        login_id: "login-1".to_owned(),
    });
    assert_eq!(
        status_request.request.required_capability(),
        Some(Capability::ConfigureProviders)
    );
    assert_eq!(
        decode_request(&encode_request(&status_request).unwrap()).unwrap(),
        status_request
    );
    let status_response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::GitHubCopilotLoginStatus {
            status: GitHubCopilotLoginStatus::Configured,
        },
    );
    assert_eq!(
        decode_response(&encode_response(&status_response).unwrap()).unwrap(),
        status_response
    );
}

#[test]
fn workspace_config_defaults_the_pulse_threshold_for_older_saved_configs() {
    let old_config: WorkspaceConfig =
        serde_json::from_str(r#"{"revision":7,"worker_nodes":[]}"#).unwrap();

    assert_eq!(old_config.cpu_pulse_threshold_percent, 5);

    let mut config = old_config;
    config.cpu_pulse_threshold_percent = 23;
    let encoded = serde_json::to_string(&config).unwrap();
    let decoded: WorkspaceConfig = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.cpu_pulse_threshold_percent, 23);
}

#[test]
fn event_json_round_trip_preserves_sequence_and_session() {
    let session_id = AgentSessionId::new();
    let snapshot = AgentSessionSnapshot {
        id: session_id,
        workspace_id: loom_core::WorkspaceId::new(),
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
fn older_provider_summaries_default_api_key_setup_to_unsupported() {
    let health = serde_json::to_value(loom_model::ProviderHealth::default()).unwrap();
    let summary: loom_model::ProviderSummary = serde_json::from_value(serde_json::json!({
        "id": "openai-compatible",
        "kind": "open_ai_compatible",
        "display_name": "OpenAI-compatible",
        "models": [],
        "credential_id": null,
        "health": health
    }))
    .unwrap();
    assert!(!summary.api_key_configurable);
}

#[test]
fn worker_status_response_round_trip_preserves_resource_samples() {
    let response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::WorkerNodeStatus(WorkerNodeStatus {
            node_id: "worker-1".to_owned(),
            name: "Worker one".to_owned(),
            online: true,
            capabilities: CapabilitySet::default(),
            resources: WorkerNodeResources {
                cpu_count: 8,
                cpu_usage_percent: Some(31),
                memory_usage_percent: Some(50),
                memory_total_bytes: Some(16 << 30),
                memory_available_bytes: Some(8 << 30),
                disk_total_bytes: Some(1 << 40),
                disk_available_bytes: Some(1 << 39),
            },
        }),
    );

    let encoded = encode_response(&response).unwrap();
    let decoded = decode_response(&encoded).unwrap();

    assert_eq!(decoded, response);
}

#[test]
fn agent_event_json_round_trip_preserves_run_identity() {
    let run = AgentRunSnapshot {
        id: RunId::new(),
        attempt_id: loom_core::RunAttemptId::new(),
        control_revision: 0,
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
fn approval_interaction_event_round_trip_preserves_revision_identity() {
    let run_id = RunId::new();
    let attempt_id = loom_core::RunAttemptId::new();
    let interaction_id = InteractionId::new();
    let tool_call_id = loom_core::ToolCallId::new();
    let event = ServerEventEnvelope {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(9),
        session_id: AgentSessionId::new(),
        event: ServerEvent::Agent {
            event: AgentEvent::ToolApprovalRequired {
                run_id,
                attempt_id,
                control_revision: 4,
                interaction_id,
                call: ToolCall {
                    id: tool_call_id,
                    name: "write_file".to_owned(),
                    arguments: serde_json::json!({"path": "src/lib.rs"}),
                },
            },
        },
    };

    let encoded = encode_event(&event).unwrap();
    let decoded = decode_event(&encoded).unwrap();

    assert_eq!(decoded, event);
}

#[test]
fn activity_event_round_trip_preserves_typed_work_and_relationships() {
    let session_id = AgentSessionId::new();
    let run_id = RunId::new();
    let step_id = StepId::new();
    let call = ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "read_file".to_owned(),
        arguments: serde_json::json!({ "path": "src/lib.rs" }),
    };
    let activity = AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        parent_id: Some(ActivityId::new()),
        step_id: Some(step_id),
        kind: AgentActivityKind::File,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(10),
        completed_at: Some(Timestamp::from_unix_millis(12)),
        elapsed_ms: Some(2),
        data: AgentActivityData::File {
            call: call.clone(),
            operation: FileActivityOperation::Read,
            path: Some("src/lib.rs".to_owned()),
            result: Some(ToolResult::success(&call, "contents".to_owned())),
        },
    };
    let event = ServerEventEnvelope {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(9),
        session_id,
        event: ServerEvent::Agent {
            event: AgentEvent::ActivityRecorded {
                run_id,
                activity: activity.clone(),
            },
        },
    };

    assert_eq!(decode_event(&encode_event(&event).unwrap()).unwrap(), event);
    assert!(activity.parent_id.is_some());
}

#[test]
fn m2_workspace_and_task_requests_round_trip_without_untyped_envelopes() {
    let request = RequestEnvelope::new(ClientRequest::ApplySessionFilesystemEdit {
        session_id: AgentSessionId::new(),
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
        ServerResponse::SessionFilesystemSnapshot(SessionFilesystemSnapshot {
            session_id: AgentSessionId::new(),
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
    let request = RequestEnvelope::new(ClientRequest::StartSessionAgentRunWithOptions {
        session_id: AgentSessionId::new(),
        task: "durable task".to_owned(),
        model: ModelId::new("deterministic/demo"),
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
fn capability_discovery_accepts_current_major_and_rejects_old_major() {
    let request = RequestEnvelope::with_version(
        ProtocolVersion::new(4, 0),
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
    assert!(!ProtocolVersion::new(3, 9).is_compatible_with(CURRENT_PROTOCOL_VERSION));
    assert!(!ProtocolVersion::new(2, 9).is_compatible_with(CURRENT_PROTOCOL_VERSION));
    assert!(!ProtocolVersion::new(1, 7).is_compatible_with(CURRENT_PROTOCOL_VERSION));
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
    let workspace_id = WorkspaceId::new();
    let sessions = RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
        workspace_id,
        include_archived: false,
    });
    assert_eq!(
        decode_request(&encode_request(&sessions).unwrap()).unwrap(),
        sessions
    );
    let message = RequestEnvelope::new(ClientRequest::SendAgentMessage {
        run_id: RunId::new(),
        attempt_id: loom_core::RunAttemptId::new(),
        expected_control_revision: 3,
        message: "continue with validation".to_owned(),
    });
    assert_eq!(
        decode_request(&encode_request(&message).unwrap()).unwrap(),
        message
    );
    let changes = RequestEnvelope::new(ClientRequest::GetSessionFilesystemChanges {
        session_id: AgentSessionId::new(),
        after_sequence: Some(EventSequence::new(4)),
    });
    assert_eq!(
        decode_request(&encode_request(&changes).unwrap()).unwrap(),
        changes
    );
    let tasks = RequestEnvelope::new(ClientRequest::ListSessionTasks {
        session_id: AgentSessionId::new(),
    });
    assert_eq!(
        decode_request(&encode_request(&tasks).unwrap()).unwrap(),
        tasks
    );
    let response = ResponseEnvelope::success(
        loom_core::RequestId::new(),
        ServerResponse::Workspaces {
            workspaces: vec![WorkspaceRecord {
                id: workspace_id,
                name: "workspace".to_owned(),
                created_at: Timestamp::from_unix_millis(5),
                updated_at: Timestamp::from_unix_millis(5),
            }],
        },
    );
    assert_eq!(
        decode_response(&encode_response(&response).unwrap()).unwrap(),
        response
    );
    let run = AgentRunSnapshot {
        id: RunId::new(),
        attempt_id: loom_core::RunAttemptId::new(),
        control_revision: 0,
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
            activities: Vec::new(),
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
