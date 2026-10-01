//! Persistence tests: feed.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn run_checkpoint_is_scoped_and_rolls_back_session_and_run_with_feed_failure() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut sessions = SessionManager::default();
    let (session, _) = sessions
        .create_in_workspace(WorkspaceId::new(), "worker session")
        .unwrap();
    let run_id = RunId::new();
    let other_run_id = RunId::new();
    let make_summary = |id, task: &str| DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id,
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: task.to_owned(),
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Planning,
            started_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(1),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let initial_summary = make_summary(run_id, "before");
    let other_summary = make_summary(other_run_id, "unrelated run");
    let initial_runs = BTreeMap::from([
        (run_id, initial_summary.clone()),
        (other_run_id, other_summary.clone()),
    ]);
    let activity_call = loom_model::ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "web_search".to_owned(),
        arguments: serde_json::json!({"query": "needle"}),
    };
    let initial_activity = AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        timeline_ordinal: 1,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ToolCall,
        status: AgentActivityStatus::AwaitingApproval,
        started_at: Timestamp::from_unix_millis(2),
        completed_at: None,
        elapsed_ms: None,
        data: AgentActivityData::ToolCall {
            call: activity_call.clone(),
            result: None,
        },
    };
    let initial_activities = BTreeMap::from([(run_id, vec![initial_activity.clone()])]);
    let initial_transcript = BTreeMap::from([(
        run_id,
        vec![
            DurableRunMessage {
                timeline_ordinal: 0,
                role: loom_model::MessageRole::System,
                content: "old system".to_owned(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            DurableRunMessage {
                timeline_ordinal: 2,
                role: loom_model::MessageRole::Assistant,
                content: "stale streamed answer".to_owned(),
                name: Some("old header".to_owned()),
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
        ],
    )]);
    persistence
        .save_state(DurableStateWrite {
            sessions: &sessions.export_state(),
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&initial_runs),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: Some(&initial_transcript),
            run_activities: Some(&initial_activities),
            filesystem_records: None,
            feed: None,
        })
        .unwrap();

    let mut updated_summary = initial_summary.clone();
    updated_summary.snapshot.task = "after checkpoint".to_owned();
    let runtime_config = DurableRunRuntimeConfig {
        system_instructions: None,
        repository_instructions: None,
        approval_policy: ApprovalPolicy::default(),
        limits: loom_core::SessionLimits::default(),
        context_options: ContextAssemblyOptions::default(),
        checkpoint_id: None,
        input_cost_micros_per_1k: 0,
        output_cost_micros_per_1k: 0,
        context_inspection: None,
        project_delegation_enabled: false,
        project_messaging_enabled: false,
        project_inspection_enabled: false,
        project_child_control_enabled: false,
        project_worktree_enabled: false,
        project_review_enabled: false,
        project_integration_enabled: false,
        project_branch_messaging_enabled: false,
    };
    let mut changed_session = session.clone();
    changed_session.state = AgentSessionState::Planning;
    changed_session.updated_at = Timestamp::now();
    let invalid_session_id = AgentSessionId::new();
    let invalid_feed = DurableFeedState {
        next_sequence: EventSequence::new(1),
        retention_limit: 250,
        events: vec![ServerEventEnvelope {
            protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(1),
            session_id: invalid_session_id,
            event: loom_model::ServerEvent::AgentSessionCreated {
                snapshot: changed_session.clone(),
            },
        }],
        workspace_events: Vec::new(),
    };
    let plan = loom_model::AgentPlan { steps: Vec::new() };
    let mut updated_activity = initial_activity.clone();
    updated_activity.status = AgentActivityStatus::Completed;
    updated_activity.completed_at = Some(Timestamp::from_unix_millis(3));
    updated_activity.elapsed_ms = Some(1);
    let activity_result = ToolResult {
        tool_call_id: activity_call.id,
        name: activity_call.name.clone(),
        success: true,
        output: "first attempt result".to_owned(),
    };
    updated_activity.data = AgentActivityData::ToolCall {
        call: activity_call.clone(),
        result: Some(activity_result.clone()),
    };
    let second_activity = AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        timeline_ordinal: 4,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ToolCall,
        status: AgentActivityStatus::Started,
        started_at: Timestamp::from_unix_millis(4),
        completed_at: None,
        elapsed_ms: None,
        data: AgentActivityData::ToolCall {
            call: activity_call.clone(),
            result: None,
        },
    };
    let third_activity = AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        timeline_ordinal: 5,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ModelTurn,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(5),
        completed_at: Some(Timestamp::from_unix_millis(6)),
        elapsed_ms: Some(1),
        data: AgentActivityData::ModelTurn {
            model: ModelId::new("deterministic-model"),
        },
    };
    let activity_delta = vec![
        updated_activity.clone(),
        second_activity.clone(),
        third_activity.clone(),
    ];
    let retry_transcript = DurableRunMessageDelta {
        start_ordinal: 0,
        reset: true,
        messages: vec![
            DurableRunMessage {
                timeline_ordinal: 3,
                role: loom_model::MessageRole::User,
                content: "retry prompt".to_owned(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            DurableRunMessage {
                timeline_ordinal: 6,
                role: loom_model::MessageRole::Assistant,
                content: "partial".to_owned(),
                name: Some("streaming".to_owned()),
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
        ],
    };
    let invalid_checkpoint = DurableRunCheckpointWrite {
        session: &changed_session,
        session_next_sequence: EventSequence::new(1),
        prune_feed: false,
        summary: &updated_summary,
        runtime_config: &runtime_config,
        context_checkpoint: None,
        plan: &plan,
        messages: &[],
        message_delta: Some(&retry_transcript),
        activities: &[],
        activity_deltas: Some(&activity_delta),
        filesystem: None,
        feed: &invalid_feed,
    };
    assert!(persistence.save_run_checkpoint(invalid_checkpoint).is_err());
    assert_eq!(
        persistence.load_run_summary(run_id).unwrap().unwrap(),
        initial_summary,
        "feed failure must roll back the run summary"
    );
    assert_eq!(
        persistence
            .load_sessions()
            .unwrap()
            .unwrap()
            .sessions
            .get(&session.id)
            .unwrap()
            .state,
        AgentSessionState::Idle,
        "feed failure must roll back the owning session projection"
    );
    assert_eq!(
        persistence.load_run_summary(other_run_id).unwrap().unwrap(),
        other_summary,
        "a worker checkpoint must leave unrelated runs untouched"
    );
    assert_eq!(
        persistence.load_run_activities(run_id).unwrap(),
        vec![initial_activity.clone()],
        "feed failure must roll back activity updates and appends"
    );
    assert_eq!(
        persistence.load_run_tool_attempts(run_id).unwrap()[0].attempt_number,
        1,
        "feed failure must roll back tool attempt updates"
    );
    assert_eq!(
        persistence.load_run_messages(run_id).unwrap(),
        initial_transcript[&run_id],
        "a failed retry checkpoint must retain the previous transcript generation"
    );

    let valid_feed = DurableFeedState {
        next_sequence: EventSequence::new(1),
        retention_limit: 250,
        events: vec![ServerEventEnvelope {
            protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(1),
            session_id: session.id,
            event: loom_model::ServerEvent::AgentSessionCreated {
                snapshot: changed_session.clone(),
            },
        }],
        workspace_events: Vec::new(),
    };
    let valid_checkpoint = DurableRunCheckpointWrite {
        session: &changed_session,
        session_next_sequence: EventSequence::new(1),
        prune_feed: false,
        summary: &updated_summary,
        runtime_config: &runtime_config,
        context_checkpoint: None,
        plan: &plan,
        messages: &[],
        message_delta: Some(&retry_transcript),
        activities: &[],
        activity_deltas: Some(&activity_delta),
        filesystem: None,
        feed: &valid_feed,
    };
    persistence.save_run_checkpoint(valid_checkpoint).unwrap();
    persistence
        .append_run_message_fragment(run_id, session.id, 1, 0, 7, b" final")
        .unwrap();
    let tail_delta = DurableRunMessageDelta {
        start_ordinal: 1,
        reset: false,
        messages: vec![DurableRunMessage {
            timeline_ordinal: 6,
            role: loom_model::MessageRole::Assistant,
            content: "partial final".to_owned(),
            name: Some("new header".to_owned()),
            tool_call_id: None,
            tool_calls: Vec::new(),
            reasoning_content: None,
        }],
    };
    let empty_feed = DurableFeedState {
        next_sequence: EventSequence::new(1),
        retention_limit: 250,
        events: Vec::new(),
        workspace_events: Vec::new(),
    };
    persistence
        .save_run_checkpoint(DurableRunCheckpointWrite {
            session: &changed_session,
            session_next_sequence: EventSequence::new(1),
            prune_feed: false,
            summary: &updated_summary,
            runtime_config: &runtime_config,
            context_checkpoint: None,
            plan: &plan,
            messages: &[],
            message_delta: Some(&tail_delta),
            activities: &[],
            activity_deltas: Some(&[]),
            filesystem: None,
            feed: &empty_feed,
        })
        .unwrap();
    drop(persistence);
    let reopened = FilePersistence::open(&path).unwrap();
    assert_eq!(
        reopened.load_run_activities(run_id).unwrap(),
        vec![updated_activity, second_activity, third_activity],
        "activity update and append order must survive restart"
    );
    assert_eq!(
        reopened.load_run_messages(run_id).unwrap(),
        vec![
            retry_transcript.messages[0].clone(),
            tail_delta.messages[0].clone()
        ],
        "retry reset and active assistant header update must survive restart"
    );
    assert_eq!(
        reopened
            .load_run_tool_attempts(run_id)
            .unwrap()
            .iter()
            .map(|attempt| attempt.attempt_number)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "per-activity writes must retain logical tool attempt order"
    );
    let attempts = reopened.load_run_tool_attempts(run_id).unwrap();
    assert_eq!(attempts[0].result.as_ref(), Some(&activity_result));
    assert_eq!(
        reopened.load_run_tool_calls(run_id).unwrap().len(),
        1,
        "adding a non-tool activity must not prune logical tool-call rows"
    );
    assert_eq!(
        reopened
            .load_sessions()
            .unwrap()
            .unwrap()
            .sessions
            .get(&session.id)
            .unwrap()
            .state,
        AgentSessionState::Planning
    );
    assert_eq!(
        reopened
            .load_run_summary(run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .task,
        "after checkpoint"
    );
    assert_eq!(
        reopened.load_run_summary(other_run_id).unwrap().unwrap(),
        other_summary
    );
    assert_eq!(
        reopened
            .load_feed_events_since(Some(session.id), None)
            .unwrap()
            .len(),
        1,
        "feed rows and the session/run projections commit together"
    );
    drop(reopened);
    fs::remove_file(path).unwrap();
}

#[test]
fn run_interactions_round_trip_and_commit_atomically_with_the_feed() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (session, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Run interactions")
        .unwrap();
    let run_id = RunId::new();
    let attempt_id = RunAttemptId::new();
    let interaction_id = InteractionId::new();
    let prompt = "Which branch should I use?".to_owned();
    let created_at = Timestamp::from_unix_millis(1_000);
    let attempt = AgentRunAttemptRecord {
        run_id,
        session_id: session.id,
        id: attempt_id,
        number: 1,
        state: AgentRunState::NeedsInput,
        checkpoint_id: None,
        started_at: created_at,
        completed_at: None,
    };
    let mut execution = AgentExecutionStateRecord {
        run_id,
        session_id: session.id,
        attempt_id,
        control_revision: 1,
        state: AgentRunState::NeedsInput,
        step_id: None,
        step_index: 0,
        provider_cursor: 0,
        next_message_id: 2,
        active_message_id: None,
        last_project_message_sequence: 7,
        last_queued_direction_sequence: 0,
        pending_tool_execution: None,
        pending_project_join: None,
        pending_approval: None,
        pending_input: Some(prompt.clone()),
        last_failed_call: None,
    };
    let mut interaction = AgentInteractionRecord {
        id: interaction_id,
        run_id,
        session_id: session.id,
        attempt_id,
        control_revision: 1,
        kind: AgentInteractionKind::UserInput,
        status: AgentInteractionStatus::Pending,
        tool_call_id: None,
        prompt: prompt.clone(),
        decision: None,
        created_at,
        resolved_at: None,
    };
    let mut summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id,
            control_revision: 1,
            session_id: session.id,
            task: "choose a branch".to_owned(),
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::NeedsInput,
            started_at: created_at,
            updated_at: created_at,
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
        },
        usage: UsageSnapshot::default(),
        attempts: Some(vec![attempt.clone()]),
        execution_state: Some(execution.clone()),
        interactions: Some(vec![interaction.clone()]),
    };
    let run_summaries = BTreeMap::from([(run_id, summary.clone())]);
    let input_event = loom_model::ServerEventEnvelope {
        protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(1),
        session_id: session.id,
        event: loom_model::ServerEvent::Agent {
            event: loom_model::AgentEvent::NeedsInput {
                run_id,
                attempt_id,
                control_revision: 1,
                interaction_id,
                prompt: prompt.clone(),
            },
        },
    };
    let mut feed = DurableFeedState {
        next_sequence: EventSequence::new(1),
        retention_limit: 16,
        events: vec![input_event.clone()],
        workspace_events: Vec::new(),
    };
    let initial_sessions = manager.export_state();
    let save = |sessions: &SessionManagerState,
                summary: &BTreeMap<RunId, DurableRunSummary>,
                feed: &DurableFeedState|
     -> Result<()> {
        persistence.save_state(DurableStateWrite {
            sessions,
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(summary),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            feed: Some(feed),
        })
    };
    save(&initial_sessions, &run_summaries, &feed).unwrap();
    assert_eq!(
        persistence.load_run_attempts(run_id).unwrap(),
        vec![attempt.clone()]
    );
    assert_eq!(
        persistence.load_run_execution_state(run_id).unwrap(),
        Some(execution.clone())
    );
    assert_eq!(
        persistence.load_run_interactions(run_id).unwrap(),
        vec![interaction.clone()]
    );

    interaction.status = AgentInteractionStatus::Answered;
    interaction.resolved_at = Some(Timestamp::from_unix_millis(2_000));
    summary.snapshot.state = AgentRunState::Executing;
    summary.snapshot.control_revision = 2;
    summary.snapshot.updated_at = Timestamp::from_unix_millis(2_000);
    summary.attempts = Some(vec![AgentRunAttemptRecord {
        state: AgentRunState::Executing,
        ..attempt.clone()
    }]);
    execution.control_revision = 2;
    execution.state = AgentRunState::Executing;
    execution.next_message_id = 3;
    execution.pending_input = None;
    execution.pending_tool_execution = Some(loom_model::ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "write_file".to_owned(),
        arguments: serde_json::json!({"path": "src/main.rs", "content": "new"}),
    });
    summary.execution_state = Some(execution.clone());
    summary.interactions = Some(vec![interaction.clone()]);
    let resolved_event = loom_model::ServerEventEnvelope {
        protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(2),
        session_id: session.id,
        event: loom_model::ServerEvent::Agent {
            event: loom_model::AgentEvent::UserMessage {
                run_id,
                attempt_id,
                control_revision: 2,
                interaction_id: Some(interaction_id),
                text: "Use the release branch.".to_owned(),
            },
        },
    };
    feed.events.push(resolved_event);
    feed.next_sequence = EventSequence::new(0);
    let resolved_summaries = BTreeMap::from([(run_id, summary.clone())]);
    let mut resolving_sessions = initial_sessions.clone();
    resolving_sessions
        .sessions
        .get_mut(&session.id)
        .unwrap()
        .state = loom_core::AgentSessionState::Executing;
    assert_eq!(
        save(&resolving_sessions, &resolved_summaries, &feed)
            .unwrap_err()
            .code,
        ErrorCode::MalformedPayload
    );
    assert_eq!(
        persistence.load_run_interactions(run_id).unwrap(),
        vec![AgentInteractionRecord {
            status: AgentInteractionStatus::Pending,
            resolved_at: None,
            ..interaction.clone()
        }]
    );
    assert_eq!(
        persistence.load_run_attempts(run_id).unwrap(),
        vec![attempt.clone()]
    );
    assert_eq!(
        persistence.load_run_execution_state(run_id).unwrap(),
        Some(AgentExecutionStateRecord {
            control_revision: 1,
            state: AgentRunState::NeedsInput,
            next_message_id: 2,
            pending_input: Some(prompt.clone()),
            pending_tool_execution: None,
            ..execution.clone()
        })
    );
    assert_eq!(
        persistence
            .load_run_summary(run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        AgentRunState::NeedsInput
    );
    assert_eq!(
        persistence.load_sessions().unwrap().unwrap(),
        initial_sessions
    );
    let persisted_feed = persistence.load_feed_state().unwrap().unwrap();
    assert_eq!(persisted_feed.next_sequence, EventSequence::new(1));
    assert_eq!(persisted_feed.events, vec![input_event.clone()]);

    drop(persistence);
    let reopened = FilePersistence::open(&path).unwrap();
    assert_eq!(
        reopened
            .load_run_summary(run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        AgentRunState::NeedsInput
    );
    assert_eq!(
        reopened.load_run_execution_state(run_id).unwrap(),
        Some(AgentExecutionStateRecord {
            control_revision: 1,
            state: AgentRunState::NeedsInput,
            next_message_id: 2,
            pending_input: Some(prompt),
            pending_tool_execution: None,
            ..execution.clone()
        })
    );
    assert_eq!(
        reopened.load_run_interactions(run_id).unwrap()[0].status,
        AgentInteractionStatus::Pending
    );
    assert_eq!(reopened.load_sessions().unwrap().unwrap(), initial_sessions);
    let reopened_feed = reopened.load_feed_state().unwrap().unwrap();
    assert_eq!(reopened_feed.next_sequence, EventSequence::new(1));
    assert_eq!(reopened_feed.events, vec![input_event]);

    feed.next_sequence = EventSequence::new(2);
    reopened
        .save_state(DurableStateWrite {
            sessions: &resolving_sessions,
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&resolved_summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            feed: Some(&feed),
        })
        .unwrap();
    assert_eq!(
        reopened
            .load_run_summary(run_id)
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        AgentRunState::Executing
    );
    assert_eq!(
        reopened.load_run_execution_state(run_id).unwrap(),
        Some(execution.clone())
    );
    assert_eq!(
        reopened.load_run_interactions(run_id).unwrap()[0].status,
        AgentInteractionStatus::Answered
    );
    assert_eq!(
        reopened.load_feed_state().unwrap().unwrap().next_sequence,
        EventSequence::new(2)
    );
    summary.snapshot.state = AgentRunState::Evaluating;
    summary.snapshot.updated_at = Timestamp::from_unix_millis(3_000);
    summary.attempts = Some(vec![AgentRunAttemptRecord {
        state: AgentRunState::Evaluating,
        ..attempt
    }]);
    execution.state = AgentRunState::Evaluating;
    summary.execution_state = Some(execution.clone());
    let evaluating_summaries = BTreeMap::from([(run_id, summary)]);
    reopened
        .save_state(DurableStateWrite {
            sessions: &resolving_sessions,
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&evaluating_summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            feed: Some(&feed),
        })
        .unwrap();
    assert_eq!(
        reopened.load_run_execution_state(run_id).unwrap(),
        Some(execution)
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn reconnect_feed_is_indexed_bounded_and_atomic_with_catalog_writes() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    let session_id = AgentSessionId::new();
    let workspace_id = WorkspaceId::new();
    let snapshot = AgentSessionSnapshot {
        id: session_id,
        workspace_id,
        name: "Feed test".to_owned(),
        state: AgentSessionState::Idle,
        created_at: Timestamp::from_unix_millis(10),
        updated_at: Timestamp::from_unix_millis(10),
    };
    let mut manager = SessionManager::default();
    manager
        .create_in_workspace_with_id(workspace_id, session_id, "Feed test")
        .unwrap();
    let second_session_id = AgentSessionId::new();
    manager
        .create_in_workspace_with_id(workspace_id, second_session_id, "Quiet feed")
        .unwrap();
    let second_snapshot = AgentSessionSnapshot {
        id: second_session_id,
        workspace_id,
        name: "Quiet feed".to_owned(),
        state: AgentSessionState::Idle,
        created_at: Timestamp::from_unix_millis(11),
        updated_at: Timestamp::from_unix_millis(11),
    };
    let sessions = manager.export_state();
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(4),
        retention_limit: 2,
        events: [
            (1, session_id, snapshot.clone()),
            (2, second_session_id, second_snapshot),
            (3, session_id, snapshot.clone()),
            (4, session_id, snapshot.clone()),
        ]
        .into_iter()
        .map(
            |(sequence, session_id, event_snapshot)| ServerEventEnvelope {
                protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(sequence),
                session_id,
                event: loom_model::ServerEvent::AgentSessionCreated {
                    snapshot: event_snapshot,
                },
            },
        )
        .collect(),
        workspace_events: Vec::new(),
    };
    store
        .save_state_with_sessions_and_feed(&sessions, Some(&feed))
        .unwrap();
    let header = store.load_feed_header().unwrap().unwrap();
    assert_eq!(header.next_sequence, EventSequence::new(4));
    assert_eq!(header.retention_limit, 2);
    let first_cursor = store.load_feed_session_cursor(session_id).unwrap().unwrap();
    assert_eq!(first_cursor.first_sequence, EventSequence::new(1));
    assert_eq!(first_cursor.latest_sequence, EventSequence::new(4));
    assert_eq!(first_cursor.pruned_through, EventSequence::new(1));
    assert_eq!(
        first_cursor.oldest_retained_sequence,
        Some(EventSequence::new(3))
    );
    assert_eq!(
        store
            .load_feed_events_since(Some(session_id), Some(EventSequence::new(2)))
            .unwrap()
            .iter()
            .map(|event| event.sequence.value())
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    let loaded = store.load_feed_state().unwrap().unwrap();
    assert_eq!(loaded.next_sequence, EventSequence::new(4));
    assert_eq!(loaded.retention_limit, 2);
    assert_eq!(
        loaded
            .events
            .iter()
            .map(|event| event.sequence.value())
            .collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    assert_eq!(
        loaded
            .events
            .iter()
            .find(|event| event.session_id == session_id)
            .map(|event| event.sequence),
        Some(EventSequence::new(3))
    );

    // A small test-only aggregate budget exercises the same global policy
    // used in production without allocating a multi-megabyte fixture.
    let additional_events = DurableFeedState {
        next_sequence: EventSequence::new(6),
        retention_limit: 100,
        events: vec![
            ServerEventEnvelope {
                protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(5),
                session_id,
                event: loom_model::ServerEvent::AgentSessionCreated {
                    snapshot: snapshot.clone(),
                },
            },
            ServerEventEnvelope {
                protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(6),
                session_id: second_session_id,
                event: loom_model::ServerEvent::AgentSessionCreated {
                    snapshot: AgentSessionSnapshot {
                        id: second_session_id,
                        workspace_id,
                        name: "Quiet feed".to_owned(),
                        state: AgentSessionState::Idle,
                        created_at: Timestamp::from_unix_millis(11),
                        updated_at: Timestamp::from_unix_millis(11),
                    },
                },
            },
        ],
        workspace_events: Vec::new(),
    };
    let mut connection = Connection::open(&path).unwrap();
    let transaction = connection.transaction().unwrap();
    save_feed_rows_with_limits(&transaction, &additional_events, 1_000_000, 250).unwrap();
    transaction.commit().unwrap();
    let retained_sequences: Vec<i64> = {
        let mut statement = connection
            .prepare("SELECT sequence FROM feed_events ORDER BY sequence")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    };
    assert!(retained_sequences.contains(&6));
    assert!(retained_sequences.iter().all(|sequence| *sequence >= 5));
    let retained_bytes: i64 = connection
        .query_row(
            "SELECT COALESCE(SUM(length(payload)), 0) FROM feed_events",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(retained_bytes <= 250);
    let quiet_cursor = store
        .load_feed_session_cursor(second_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(quiet_cursor.latest_sequence, EventSequence::new(6));
    assert_eq!(quiet_cursor.pruned_through, EventSequence::new(2));
    drop(connection);

    let connection = Connection::open(&path).unwrap();
    let plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT sequence FROM feed_events
             WHERE session_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 20",
            params![session_id.as_uuid().as_bytes().as_slice(), 0_i64],
            |row| row.get(3),
        )
        .unwrap();
    assert!(plan.contains("feed_events_by_session_sequence"), "{plan}");
    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn workspace_reconnect_feed_is_isolated_indexed_and_tracks_pruning() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    let workspace_a = WorkspaceId::new();
    let workspace_b = WorkspaceId::new();
    let session_a = AgentSessionId::new();
    let session_b = AgentSessionId::new();
    let mut manager = SessionManager::default();
    manager
        .create_in_workspace_with_id(workspace_a, session_a, "Workspace A")
        .unwrap();
    manager
        .create_in_workspace_with_id(workspace_b, session_b, "Workspace B")
        .unwrap();
    let snapshot = |id, workspace_id| AgentSessionSnapshot {
        id,
        workspace_id,
        name: "session".to_owned(),
        state: AgentSessionState::Idle,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(1),
    };
    let event = |sequence, id, workspace_id| ServerEventEnvelope {
        protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(sequence),
        session_id: id,
        event: loom_model::ServerEvent::AgentSessionCreated {
            snapshot: snapshot(id, workspace_id),
        },
    };
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(4),
        retention_limit: 1,
        events: vec![
            event(1, session_a, workspace_a),
            event(2, session_b, workspace_b),
            event(3, session_a, workspace_a),
            event(4, session_b, workspace_b),
        ],
        workspace_events: Vec::new(),
    };
    store
        .save_state_with_sessions_and_feed(&manager.export_state(), Some(&feed))
        .unwrap();

    let events_a = store
        .load_feed_workspace_events_since(workspace_a, None)
        .unwrap();
    let events_b = store
        .load_feed_workspace_events_since(workspace_b, None)
        .unwrap();
    assert_eq!(
        events_a
            .iter()
            .map(workspace_feed_event_sequence)
            .collect::<Vec<_>>(),
        vec![3]
    );
    assert_eq!(
        events_b
            .iter()
            .map(workspace_feed_event_sequence)
            .collect::<Vec<_>>(),
        vec![4]
    );
    let cursor_a = store
        .load_feed_workspace_cursor(workspace_a)
        .unwrap()
        .unwrap();
    let cursor_b = store
        .load_feed_workspace_cursor(workspace_b)
        .unwrap()
        .unwrap();
    assert_eq!(cursor_a.first_sequence, EventSequence::new(1));
    assert_eq!(cursor_a.latest_sequence, EventSequence::new(3));
    assert_eq!(cursor_a.pruned_through, EventSequence::new(1));
    assert_eq!(
        cursor_a.oldest_retained_sequence,
        Some(EventSequence::new(3))
    );
    assert_eq!(cursor_b.first_sequence, EventSequence::new(2));
    assert_eq!(cursor_b.latest_sequence, EventSequence::new(4));
    assert_eq!(cursor_b.pruned_through, EventSequence::new(2));
    assert_eq!(
        cursor_b.oldest_retained_sequence,
        Some(EventSequence::new(4))
    );

    let connection = Connection::open(&path).unwrap();
    let plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT sequence FROM feed_events
         WHERE workspace_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 20",
            params![workspace_a.as_uuid().as_bytes().as_slice(), 0_i64],
            |row| row.get(3),
        )
        .unwrap();
    assert!(plan.contains("feed_events_by_workspace_sequence"), "{plan}");
    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn session_and_workspace_feed_share_the_global_payload_budget() {
    let path = std::env::temp_dir().join(format!("loom-mixed-feed-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    let workspace_id = WorkspaceId::new();
    let session_id = AgentSessionId::new();
    let mut manager = SessionManager::default();
    let (snapshot, _) = manager
        .create_in_workspace_with_id(workspace_id, session_id, "Feed budget")
        .unwrap();
    let empty_feed = DurableFeedState {
        next_sequence: EventSequence::default(),
        retention_limit: 100,
        events: Vec::new(),
        workspace_events: Vec::new(),
    };
    store
        .save_state_with_sessions_and_feed(&manager.export_state(), Some(&empty_feed))
        .unwrap();
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(3),
        retention_limit: 100,
        events: vec![ServerEventEnvelope {
            protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(1),
            session_id,
            event: loom_model::ServerEvent::AgentSessionCreated { snapshot },
        }],
        workspace_events: vec![
            WorkspaceEventEnvelope {
                protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(2),
                workspace_id,
                event: loom_model::WorkspaceEvent::Renamed {
                    name: "workspace mutation one".to_owned(),
                },
            },
            WorkspaceEventEnvelope {
                protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(3),
                workspace_id,
                event: loom_model::WorkspaceEvent::Renamed {
                    name: "workspace mutation two".to_owned(),
                },
            },
        ],
    };
    let mut connection = Connection::open(&path).unwrap();
    let transaction = connection.transaction().unwrap();
    save_feed_rows_with_limits(&transaction, &feed, 10_000, 250).unwrap();
    transaction.commit().unwrap();
    let retained_bytes: i64 = connection
        .query_row(
            "SELECT COALESCE((SELECT SUM(length(payload)) FROM feed_events), 0)
                  + COALESCE((SELECT SUM(length(payload)) FROM workspace_feed_events), 0)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        retained_bytes <= 250,
        "retained payload bytes: {retained_bytes}"
    );
    drop(connection);
    fs::remove_file(path).unwrap();
}
