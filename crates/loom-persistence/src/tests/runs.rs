//! Persistence tests: runs.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn queued_run_directions_validate_and_read_lazily() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    let run_id = RunId::new();
    let session_id = AgentSessionId::new();
    assert!(store.list_run_directions(run_id, 0, 8).unwrap().is_empty());
    assert_eq!(
        store
            .enqueue_run_direction(run_id, session_id, "   ", Timestamp::now())
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        store
            .enqueue_run_direction(run_id, session_id, &"x".repeat(65_537), Timestamp::now())
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    drop(store);
    let _ = fs::remove_file(path);
}

#[test]
fn streamed_messages_are_append_only_and_paged_by_keyset() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (session, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Streamed messages")
        .unwrap();
    let run_id = RunId::new();
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "stream fragments".to_owned(),
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at: Timestamp::from_unix_millis(1000),
            updated_at: Timestamp::from_unix_millis(1000),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let run_summaries = BTreeMap::from([(run_id, summary)]);
    let large_content = "0123456789".repeat(60_000);
    let run_messages = BTreeMap::from([(
        run_id,
        vec![
            DurableRunMessage {
                timeline_ordinal: 0,
                role: loom_model::MessageRole::User,
                content: "question".to_owned(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            DurableRunMessage {
                timeline_ordinal: 0,
                role: loom_model::MessageRole::Assistant,
                content: "seed".to_owned(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            DurableRunMessage {
                timeline_ordinal: 0,
                role: loom_model::MessageRole::Assistant,
                content: String::new(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            DurableRunMessage {
                timeline_ordinal: 0,
                role: loom_model::MessageRole::User,
                content: large_content.clone(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
        ],
    )]);
    let oversized_fragment = vec![b'x'; MAX_MESSAGE_FRAGMENT_BYTES + 1];
    assert_eq!(
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 0, 4, &oversized_fragment,)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    persistence
        .save_state(DurableStateWrite {
            sessions: &manager.export_state(),
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&run_summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: Some(&run_messages),
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
        .unwrap();

    for (message_ordinal, fragment_ordinal, byte_offset) in [
        (u64::MAX, 0, 0),
        (1, u64::MAX, 4),
        (1, 0, u64::MAX),
        (1, 0, i64::MAX as u64),
    ] {
        assert_eq!(
            persistence
                .append_run_message_fragment(
                    run_id,
                    session.id,
                    message_ordinal,
                    fragment_ordinal,
                    byte_offset,
                    b"x",
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    assert_eq!(
        persistence
            .append_run_message_fragment(run_id, AgentSessionId::new(), 1, 0, 4, b"x")
            .unwrap_err()
            .code,
        ErrorCode::WorkspaceAccessDenied
    );
    assert_eq!(
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"")
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 0, 4, &[0xff])
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        persistence
            .append_run_message_fragment(run_id, session.id, 0, 0, 0, b"not assistant")
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    persistence
        .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"hello ")
        .unwrap();
    persistence
        .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"hello ")
        .unwrap();
    persistence
        .append_run_message_fragment(run_id, session.id, 1, 1, 10, b"world")
        .unwrap();
    assert_eq!(
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"conflict")
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        persistence
            .load_run_message_content_range(run_id, 999, 0, 1)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        persistence
            .load_run_message_content_range(
                run_id,
                1,
                0,
                MAX_CONTENT_RANGE_BYTES.saturating_add(1),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    let newest_page = persistence
        .load_run_message_page(run_id, Some(2), 1)
        .unwrap();
    assert_eq!(newest_page.len(), 1);
    assert_eq!(newest_page[0].ordinal, 1);
    assert_eq!(newest_page[0].content_bytes, 15);
    assert_eq!(
        persistence
            .load_run_message_content_range(run_id, 1, u64::MAX, 1)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        persistence
            .load_run_message_content_range(run_id, 1, 7, 5)
            .unwrap(),
        b"lo wo"
    );
    assert_eq!(
        persistence
            .load_run_message_content_range(run_id, 3, (CONTENT_PART_BYTES - 3) as u64, 10,)
            .unwrap(),
        b"1234567890"
    );
    assert!(
        persistence
            .load_run_message_content_range(run_id, 3, 0, 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        persistence.load_run_messages(run_id).unwrap()[1].content,
        "seedhello world"
    );
    assert_eq!(
        persistence
            .load_run_message_page(run_id, Some(1), 1)
            .unwrap()[0]
            .ordinal,
        0
    );
    assert!(
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 2, 12, b"!")
            .is_err()
    );
    assert_eq!(
        persistence
            .next_run_message_fragment_position(run_id, 1)
            .unwrap(),
        (2, 15)
    );
    persistence
        .append_run_message_fragment(run_id, session.id, 1, 2, 15, b"!")
        .unwrap();
    persistence
        .append_run_message_fragment(run_id, session.id, 2, 0, 0, b"part")
        .unwrap();
    persistence
        .append_run_message_fragment(run_id, session.id, 2, 1, 4, b"ial")
        .unwrap();
    drop(persistence);
    let persistence = FilePersistence::open(&path).unwrap();
    let newest_page = persistence
        .load_run_message_page(run_id, Some(3), 1)
        .unwrap();
    assert_eq!(newest_page[0].ordinal, 2);
    assert_eq!(newest_page[0].content_bytes, 7);
    assert_eq!(
        persistence
            .load_run_message_content_range(run_id, 2, 2, 4)
            .unwrap(),
        b"rtia"
    );
    assert_eq!(
        persistence.load_run_messages(run_id).unwrap()[2].content,
        "partial"
    );
    assert_eq!(
        persistence.load_run_messages(run_id).unwrap()[3].content,
        large_content
    );
    assert_eq!(
        persistence
            .load_run_message_page(run_id, Some(2), 1)
            .unwrap()[0]
            .ordinal,
        1
    );
    assert_eq!(
        persistence
            .load_run_message_page(run_id, Some(1), 1)
            .unwrap()[0]
            .ordinal,
        0
    );
    assert_eq!(
        persistence.load_run_messages(run_id).unwrap()[1].content,
        "seedhello world!"
    );
    persistence
        .save_state(DurableStateWrite {
            sessions: &manager.export_state(),
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&run_summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: Some(&run_messages),
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
        .unwrap();
    let after_stale_snapshot = persistence.load_run_messages(run_id).unwrap();
    assert_eq!(after_stale_snapshot[1].content, "seedhello world!");
    assert_eq!(after_stale_snapshot[2].content, "partial");

    let mut mismatched_messages = run_messages.clone();
    mismatched_messages.get_mut(&run_id).unwrap()[1].content = "replacement base".to_owned();
    mismatched_messages.get_mut(&run_id).unwrap()[2].content = "replacement tail".to_owned();
    persistence
        .save_state(DurableStateWrite {
            sessions: &manager.export_state(),
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&run_summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: Some(&mismatched_messages),
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
        .unwrap();
    let after_mismatch = persistence.load_run_messages(run_id).unwrap();
    assert_eq!(after_mismatch[1].content, "seedhello world!");
    assert_eq!(after_mismatch[2].content, "partial");
    assert_eq!(stored_fragment_count(&path, run_id), 5);

    let mut assembled_messages = run_messages.clone();
    assembled_messages.get_mut(&run_id).unwrap()[1].content = "seedhello world!".to_owned();
    assembled_messages.get_mut(&run_id).unwrap()[2].content = "partial".to_owned();
    let invalid_feed = invalid_feed_for_rollback();
    assert!(
        persistence
            .save_state(DurableStateWrite {
                sessions: &manager.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&run_summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: Some(&assembled_messages),
                run_activities: None,
                filesystem_records: None,
                feed: Some(&invalid_feed),
            })
            .is_err()
    );
    let after_rollback = persistence.load_run_messages(run_id).unwrap();
    assert_eq!(after_rollback[1].content, "seedhello world!");
    assert_eq!(after_rollback[2].content, "partial");
    assert_eq!(stored_fragment_count(&path, run_id), 5);
    persistence
        .save_state(DurableStateWrite {
            sessions: &manager.export_state(),
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&run_summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: Some(&assembled_messages),
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
        .unwrap();
    let after_consolidation = persistence.load_run_messages(run_id).unwrap();
    assert_eq!(after_consolidation[1].content, "seedhello world!");
    assert_eq!(after_consolidation[2].content, "partial");
    assert_eq!(stored_fragment_count(&path, run_id), 0);
    assert!(
        persistence
            .load_run_message_page(run_id, None, 101)
            .is_err()
    );
    assert!(
        persistence
            .load_run_message_content_range(run_id, 1, 0, MAX_CONTENT_RANGE_BYTES.saturating_add(1))
            .is_err()
    );
    let connection = Connection::open(&path).unwrap();
    let page_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT ordinal FROM run_messages
             WHERE run_id=?1 AND ordinal<?2 ORDER BY ordinal DESC LIMIT 20",
            params![run_id.as_uuid().as_bytes().as_slice(), 2_i64],
            |row| row.get(3),
        )
        .unwrap();
    assert!(page_plan.contains("PRIMARY KEY"), "{page_plan}");
    let content_range_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT byte_offset, byte_length, blob_hash
             FROM content_parts
             WHERE content_hash=?1 AND byte_offset<?2
               AND byte_offset + byte_length > ?3
             ORDER BY byte_offset",
            params![vec![0_u8; 32], CONTENT_PART_BYTES as i64, 0_i64],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        content_range_plan.contains("content_hash=? AND byte_offset<?"),
        "{content_range_plan}"
    );
    let large_content_hash: Vec<u8> = connection
        .query_row(
            "SELECT content_hash FROM run_messages WHERE run_id=?1 AND ordinal=3",
            [run_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM content_parts WHERE content_hash=?1 AND ordinal=1",
            [large_content_hash],
        )
        .unwrap();
    drop(connection);
    assert_eq!(
        persistence
            .load_run_message_content_range(run_id, 3, CONTENT_PART_BYTES as u64 + 1, 8)
            .unwrap_err()
            .code,
        ErrorCode::MalformedPayload
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn run_message_pages_reject_invalid_limits_and_cursors() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    store
        .save_state_with_sessions(&SessionManager::default().export_state())
        .unwrap();
    let run_id = RunId::new();
    assert!(
        store
            .load_run_message_page(run_id, None, 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .load_run_message_page(run_id, None, 0)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        store
            .load_run_message_page(run_id, None, MAX_RUN_MESSAGE_PAGE_SIZE + 1)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        store
            .load_run_message_page(run_id, Some(u64::MAX), 1)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn tool_attempt_state_storage_tracks_activity_outcomes_and_intents() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut sessions = SessionManager::default();
    let (session, _) = sessions
        .create_in_workspace(WorkspaceId::new(), "Tool attempt owner")
        .unwrap();
    let run_id = RunId::new();
    let attempt_id = RunAttemptId::new();
    let started_at = Timestamp::from_unix_millis(1000);
    let queued_call = loom_model::ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "read_file".to_owned(),
        arguments: serde_json::json!({"path": "queued"}),
    };
    let unknown_call = loom_model::ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "write_file".to_owned(),
        arguments: serde_json::json!({"path": "unknown"}),
    };
    let statuses = [
        (queued_call.clone(), AgentActivityStatus::Started),
        (unknown_call.clone(), AgentActivityStatus::Started),
        (
            loom_model::ToolCall {
                id: loom_core::ToolCallId::new(),
                name: "inspect".to_owned(),
                arguments: serde_json::json!({}),
            },
            AgentActivityStatus::Started,
        ),
        (
            loom_model::ToolCall {
                id: loom_core::ToolCallId::new(),
                name: "failed".to_owned(),
                arguments: serde_json::json!({}),
            },
            AgentActivityStatus::Failed,
        ),
        (
            loom_model::ToolCall {
                id: loom_core::ToolCallId::new(),
                name: "ask_user".to_owned(),
                arguments: serde_json::json!({}),
            },
            AgentActivityStatus::AwaitingInput,
        ),
        (
            loom_model::ToolCall {
                id: loom_core::ToolCallId::new(),
                name: "cancelled".to_owned(),
                arguments: serde_json::json!({}),
            },
            AgentActivityStatus::Cancelled,
        ),
    ];
    let mut activities = statuses
        .into_iter()
        .enumerate()
        .map(|(ordinal, (call, status))| AgentActivityRecord {
            id: ActivityId::new(),
            run_id,
            timeline_ordinal: 0,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ToolCall,
            status,
            started_at: Timestamp::from_unix_millis(1000 + ordinal as u64),
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::ToolCall { call, result: None },
        })
        .collect::<Vec<_>>();

    let completed_call = loom_model::ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "completed".to_owned(),
        arguments: serde_json::json!({}),
    };
    let completed_call_id = completed_call.id;
    activities.push(AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        timeline_ordinal: 0,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ToolCall,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(1100),
        completed_at: Some(Timestamp::from_unix_millis(1200)),
        elapsed_ms: Some(100),
        data: AgentActivityData::ToolCall {
            call: completed_call,
            result: Some(ToolResult {
                tool_call_id: completed_call_id,
                name: "completed".to_owned(),
                success: true,
                output: "done".to_owned(),
            }),
        },
    });

    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id,
            control_revision: 3,
            session_id: session.id,
            task: "Exercise tool-attempt states".to_owned(),
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at,
            updated_at: Timestamp::from_unix_millis(1200),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
        },
        usage: UsageSnapshot::default(),
        attempts: Some(vec![AgentRunAttemptRecord {
            run_id,
            session_id: session.id,
            id: attempt_id,
            number: 1,
            state: AgentRunState::Executing,
            checkpoint_id: None,
            started_at,
            completed_at: None,
        }]),
        execution_state: Some(AgentExecutionStateRecord {
            run_id,
            session_id: session.id,
            attempt_id,
            control_revision: 3,
            state: AgentRunState::Executing,
            step_id: None,
            step_index: 0,
            provider_cursor: 0,
            next_message_id: 0,
            active_message_id: None,
            last_project_message_sequence: 0,
            last_queued_direction_sequence: 0,
            pending_tool_execution: Some(queued_call.clone()),
            pending_project_join: None,
            pending_approval: None,
            pending_input: None,
            last_failed_call: Some(unknown_call.clone()),
        }),
        interactions: None,
    };
    let summaries = BTreeMap::from([(run_id, summary)]);
    let activities = BTreeMap::from([(run_id, activities)]);
    persistence
        .save_state(DurableStateWrite {
            sessions: &sessions.export_state(),
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: Some(&activities),
            filesystem_records: None,
            feed: None,
        })
        .unwrap();

    let attempts = persistence.load_run_tool_attempts(run_id).unwrap();
    assert_eq!(attempts.len(), 7);
    assert_eq!(
        attempts
            .iter()
            .find(|attempt| attempt.call_id == queued_call.id)
            .unwrap()
            .state,
        AgentToolAttemptState::Queued
    );
    assert_eq!(
        attempts
            .iter()
            .find(|attempt| attempt.call_id == unknown_call.id)
            .unwrap()
            .state,
        AgentToolAttemptState::OutcomeUnknown
    );
    assert_eq!(
        attempts
            .iter()
            .map(|attempt| attempt.state)
            .collect::<Vec<_>>(),
        vec![
            AgentToolAttemptState::Queued,
            AgentToolAttemptState::OutcomeUnknown,
            AgentToolAttemptState::Running,
            AgentToolAttemptState::Failed,
            AgentToolAttemptState::AwaitingInput,
            AgentToolAttemptState::Cancelled,
            AgentToolAttemptState::Completed,
        ]
    );
    fs::remove_file(path).unwrap();
}
