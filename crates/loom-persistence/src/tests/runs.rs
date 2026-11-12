//! Persistence tests: runs.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn qualified_model_selectors_round_trip_with_bounded_storage() {
    let path = std::env::temp_dir().join(format!("loom-model-selector-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (session, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Selectors")
        .unwrap();
    store
        .save_state_with_sessions(&manager.export_state())
        .unwrap();
    let run_id = RunId::new();
    let model = ModelId::for_provider(
        &loom_model::ProviderId::new("provider/\"\\".repeat(20)),
        &ModelId::new("model/\"\\".repeat(32)),
    );
    assert!(model.as_str().len() > 512);
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "persist qualified selector".to_owned(),
            jev: None,
            model: model.clone(),
            state: AgentRunState::Executing,
            started_at: Timestamp::from_unix_millis(1000),
            updated_at: Timestamp::from_unix_millis(1000),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let mut summaries = BTreeMap::from([(run_id, summary)]);
    {
        let mut connection = Connection::open(&path).unwrap();
        let transaction = connection.transaction().unwrap();
        save_run_summary_rows(&transaction, &summaries).unwrap();
        transaction.commit().unwrap();
    }
    drop(store);
    let store = FilePersistence::open(&path).unwrap();
    assert_eq!(
        store.load_run_summaries().unwrap()[&run_id].snapshot.model,
        model
    );
    let mut connection = Connection::open(&path).unwrap();
    for value in [
        "x".repeat(MAX_MODEL_ID_BYTES),
        "é".repeat(MAX_MODEL_ID_BYTES / 2),
    ] {
        summaries.get_mut(&run_id).unwrap().snapshot.model = ModelId::new(value);
        let transaction = connection.transaction().unwrap();
        save_run_summary_rows(&transaction, &summaries).unwrap();
        transaction.commit().unwrap();
    }
    for value in [
        "x".repeat(MAX_MODEL_ID_BYTES + 1),
        "é".repeat(MAX_MODEL_ID_BYTES / 2 + 1),
    ] {
        summaries.get_mut(&run_id).unwrap().snapshot.model = ModelId::new(value.clone());
        let transaction = connection.transaction().unwrap();
        assert!(save_run_summary_rows(&transaction, &summaries).is_err());
        drop(transaction);
        assert!(
            connection
                .execute("UPDATE run_summaries SET model=?1", [value])
                .is_err()
        );
    }
    drop((connection, store));
    fs::remove_file(path).unwrap();
}

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
            jev: None,
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at: Timestamp::from_unix_millis(1000),
            updated_at: Timestamp::from_unix_millis(1000),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
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
    let gap = persistence
        .append_run_message_fragment(run_id, session.id, 1, 3, 15, b"gap")
        .unwrap_err();
    assert_eq!(gap.code, ErrorCode::InvalidRequest);
    assert!(
        gap.message.contains(&format!("run {run_id} message 1")),
        "the error must name the run and message: {}",
        gap.message
    );
    assert!(
        gap.message
            .contains("expected fragment 2 at byte offset 15"),
        "the error must name the expected fragment: {}",
        gap.message
    );
    assert!(
        gap.message.contains("got fragment 3 at byte offset 15"),
        "the error must name the appended fragment: {}",
        gap.message
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
fn a_streamed_message_reserves_an_ordinal_above_every_persisted_item() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (session, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Streamed ordinal")
        .unwrap();
    let run_id = RunId::new();
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "stream an ordinal".to_owned(),
            jev: None,
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at: Timestamp::from_unix_millis(1000),
            updated_at: Timestamp::from_unix_millis(1000),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let run_summaries = BTreeMap::from([(run_id, summary)]);
    let run_messages = BTreeMap::from([(
        run_id,
        (0..3)
            .map(|ordinal| DurableRunMessage {
                timeline_ordinal: ordinal,
                role: if ordinal == 0 {
                    loom_model::MessageRole::User
                } else {
                    loom_model::MessageRole::Assistant
                },
                content: format!("message {ordinal}"),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
            .collect::<Vec<_>>(),
    )]);
    let activity = AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        timeline_ordinal: 3,
        parent_id: None,
        step_id: Some(StepId::new()),
        kind: AgentActivityKind::ToolCall,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(1500),
        completed_at: None,
        elapsed_ms: None,
        data: AgentActivityData::ToolCall {
            call: loom_model::ToolCall {
                id: loom_core::ToolCallId::new(),
                name: "search_text".to_owned(),
                arguments: serde_json::json!({"query": "ordinal"}),
            },
            result: None,
        },
    };
    let run_activities = BTreeMap::from([(run_id, vec![activity])]);
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
            run_activities: Some(&run_activities),
            filesystem_records: None,
            feed: None,
        })
        .unwrap();

    // A streamed assistant turn reaches storage before the run assigns the
    // finished message its timeline ordinal. The row it creates must claim one of
    // its own instead of defaulting to zero, which is the ordinal of the
    // transcript's first message.
    persistence
        .append_run_message_fragment(run_id, session.id, 3, 0, 0, b"partial reply")
        .unwrap();

    let messages = persistence.load_run_messages(run_id).unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[3].timeline_ordinal, 4,
        "the streamed row reserves an ordinal above the persisted messages and the activity"
    );
    let mut ordinals = messages
        .iter()
        .map(|message| message.timeline_ordinal)
        .chain(
            persistence
                .load_run_activities(run_id)
                .unwrap()
                .iter()
                .map(|activity| activity.timeline_ordinal),
        )
        .collect::<Vec<_>>();
    ordinals.sort_unstable();
    assert!(
        ordinals.windows(2).all(|pair| pair[0] < pair[1]),
        "every timeline item keeps its own ordinal: {ordinals:?}"
    );
    assert_eq!(
        persistence
            .load_run_messages(run_id)
            .unwrap()
            .iter()
            .filter(|message| message.timeline_ordinal == 0)
            .count(),
        1,
        "only the first message may claim ordinal zero"
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn streamed_transcript_reuses_decoded_content_across_messages() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (session, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Repeated transcript content")
        .unwrap();
    let run_id = RunId::new();
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "reuse decoded content".to_owned(),
            jev: None,
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at: Timestamp::from_unix_millis(1000),
            updated_at: Timestamp::from_unix_millis(1000),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let run_summaries = BTreeMap::from([(run_id, summary)]);
    let large_content = "0123456789".repeat(60_000);
    let message = |role: loom_model::MessageRole, content: &str| DurableRunMessage {
        timeline_ordinal: 0,
        role,
        content: content.to_owned(),
        name: None,
        tool_call_id: None,
        tool_calls: Vec::new(),
        reasoning_content: None,
    };
    let run_messages = BTreeMap::from([(
        run_id,
        vec![
            message(loom_model::MessageRole::User, "question"),
            message(loom_model::MessageRole::Assistant, "seed"),
            message(loom_model::MessageRole::Assistant, ""),
            message(loom_model::MessageRole::User, &large_content),
            // The base and two fragments of message one, committed again.
            message(loom_model::MessageRole::Assistant, "seed"),
        ],
    )]);
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
    for (message_ordinal, fragment_ordinal, byte_offset, bytes) in [
        (1_u64, 0_u64, 4_u64, b"hello ".as_slice()),
        (1, 1, 10, b"world"),
        (1, 2, 15, b"!"),
        (2, 0, 0, b"part"),
        (2, 1, 4, b"ial"),
        (4, 0, 4, b"world"),
        (4, 1, 9, b"!"),
    ] {
        persistence
            .append_run_message_fragment(
                run_id,
                session.id,
                message_ordinal,
                fragment_ordinal,
                byte_offset,
                bytes,
            )
            .unwrap();
    }
    assert_eq!(stored_fragment_count(&path, run_id), 7);

    reset_decoded_content_calls();
    let transcript = persistence.load_run_messages(run_id).unwrap();
    // One decode per distinct content object: the question, `seed`, `hello `,
    // `world`, `!`, `part`, `ial` and the large body. Message four's repeat of
    // `seed`, `world` and `!` must not decode and hash them again.
    assert_eq!(decoded_content_calls(), 8);
    assert_eq!(transcript.len(), 5);
    assert_eq!(transcript[0].content, "question");
    assert_eq!(transcript[1].content, "seedhello world!");
    assert_eq!(transcript[2].content, "partial");
    assert_eq!(transcript[3].content, large_content);
    assert_eq!(transcript[4].content, "seedworld!");
    assert_eq!(
        transcript
            .iter()
            .map(|message| message.timeline_ordinal)
            .collect::<Vec<_>>(),
        vec![0, 0, 0, 0, 0]
    );
    // The cache is scoped to one call, so a second read decodes the same eight
    // objects again instead of serving state from the first read.
    reset_decoded_content_calls();
    let reloaded = persistence.load_run_messages(run_id).unwrap();
    assert_eq!(decoded_content_calls(), 8);
    assert_eq!(reloaded[1].content, "seedhello world!");
    assert_eq!(reloaded[4].content, "seedworld!");
    drop(persistence);
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
            jev: None,
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at,
            updated_at: Timestamp::from_unix_millis(1200),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
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

/// A logical tool call is written once: a later record that carries no arguments
/// belongs to the call and is accepted, while different arguments keep the value
/// that was stored first.
///
/// The refusal is confined to the run that carries the call: the save commits
/// every other run and names the refused one, so one divergent record cannot
/// stop the state of the whole worker.
#[test]
fn a_stored_tool_call_refuses_a_rewrite_for_its_own_run_only() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut sessions = SessionManager::default();
    let (session, _) = sessions
        .create_in_workspace(WorkspaceId::new(), "Placeholder owner")
        .unwrap();
    let run_id = RunId::new();
    let attempt_id = RunAttemptId::new();
    let call = loom_model::ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "run_command".to_owned(),
        arguments: serde_json::json!({"command": "ls"}),
    };
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id,
            control_revision: 1,
            session_id: session.id,
            task: "Keep tool calls immutable".to_owned(),
            jev: None,
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at: Timestamp::from_unix_millis(1000),
            updated_at: Timestamp::from_unix_millis(1000),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let summaries = BTreeMap::from([(run_id, summary)]);
    let activity_id = ActivityId::new();
    let activity = |call: loom_model::ToolCall, status| AgentActivityRecord {
        id: activity_id,
        run_id,
        timeline_ordinal: 0,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ToolCall,
        status,
        started_at: Timestamp::from_unix_millis(1000),
        completed_at: None,
        elapsed_ms: None,
        data: AgentActivityData::ToolCall { call, result: None },
    };
    let save = |record: AgentActivityRecord| {
        let activities = BTreeMap::from([(run_id, vec![record])]);
        persistence.save_state(DurableStateWrite {
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
    };

    save(activity(call.clone(), AgentActivityStatus::Started)).unwrap();
    // A later record that names the call but carries no arguments, such as
    // the answer to a call interrupted by a restart, must not be refused.
    let placeholder = loom_model::ToolCall {
        arguments: serde_json::Value::Null,
        ..call.clone()
    };
    save(activity(placeholder, AgentActivityStatus::Cancelled)).unwrap();
    // Different arguments are still a rewrite of the call and stay refused, but
    // only for this run: the save commits everything else and names the run it
    // left alone, because a refusal that failed the whole save would stop the
    // worker for every session.
    let rewritten = loom_model::ToolCall {
        arguments: serde_json::json!({"command": "rm -rf /"}),
        ..call
    };
    let outcome = save(activity(rewritten, AgentActivityStatus::Started)).unwrap();
    assert_eq!(
        outcome
            .confined_runs
            .iter()
            .map(|confined| confined.run_id)
            .collect::<Vec<_>>(),
        vec![run_id]
    );
    let nested = &outcome.confined_runs[0].error.message;
    assert!(
        nested.contains("'run_command'") && nested.contains("arguments"),
        "the refusal names the call and the part it refused: {nested}"
    );
    let stored = persistence.load_run_tool_calls(run_id).unwrap();
    assert_eq!(
        stored
            .iter()
            .map(|record| record.call.arguments.clone())
            .collect::<Vec<_>>(),
        vec![serde_json::json!({"command": "ls"})],
        "the stored call keeps the value it was first written with"
    );
    // A save that names a healthy run next to the refused one commits the
    // healthy run, so a bad call costs one run and not the whole state.
    let healthy_run_id = RunId::new();
    let mut both_summaries = summaries.clone();
    both_summaries.insert(
        healthy_run_id,
        DurableRunSummary {
            snapshot: AgentRunSnapshot {
                id: healthy_run_id,
                attempt_id: RunAttemptId::new(),
                ..summaries[&run_id].snapshot.clone()
            },
            ..summaries[&run_id].clone()
        },
    );
    let refused_activity = AgentActivityRecord {
        data: AgentActivityData::ToolCall {
            call: loom_model::ToolCall {
                arguments: serde_json::json!({"command": "rm -rf /"}),
                ..stored[0].call.clone()
            },
            result: None,
        },
        ..activity(stored[0].call.clone(), AgentActivityStatus::Started)
    };
    let healthy_activity = AgentActivityRecord {
        id: ActivityId::new(),
        run_id: healthy_run_id,
        data: AgentActivityData::ToolCall {
            call: loom_model::ToolCall {
                id: loom_core::ToolCallId::new(),
                name: "run_command".to_owned(),
                arguments: serde_json::json!({"command": "pwd"}),
            },
            result: None,
        },
        ..activity(stored[0].call.clone(), AgentActivityStatus::Started)
    };
    let activities = BTreeMap::from([
        (run_id, vec![refused_activity]),
        (healthy_run_id, vec![healthy_activity]),
    ]);
    let outcome = persistence
        .save_state(DurableStateWrite {
            sessions: &sessions.export_state(),
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: Some(&both_summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: Some(&activities),
            filesystem_records: None,
            feed: None,
        })
        .unwrap();
    assert_eq!(
        outcome
            .confined_runs
            .iter()
            .map(|confined| confined.run_id)
            .collect::<Vec<_>>(),
        vec![run_id]
    );
    assert_eq!(
        persistence
            .load_run_activities(healthy_run_id)
            .unwrap()
            .len(),
        1,
        "a run the save accepted keeps its committed activity"
    );
    fs::remove_file(path).unwrap();
}

/// A full state save snapshots a run's activities while that run's worker
/// keeps appending activities through the delta checkpoint. The snapshot is
/// then older than the stored rows, so the save must neither delete what the
/// checkpoint already committed nor shift what it writes: the runtime keeps
/// every appended activity in its vector, and the next save hands that vector
/// back in the same order. A save that pruned its own snapshot would drop the
/// activity and free its ordinal for a different one, which is how a durable
/// save started failing with "persisted activity order cannot be changed".
#[test]
fn a_stale_state_save_keeps_the_activities_a_checkpoint_appended() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut sessions = SessionManager::default();
    let (session, _) = sessions
        .create_in_workspace(WorkspaceId::new(), "Activity order race")
        .unwrap();
    let run_id = RunId::new();
    let started_at = Timestamp::from_unix_millis(1000);
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "append activities while saving state".to_owned(),
            jev: None,
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at,
            updated_at: started_at,
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let summaries = BTreeMap::from([(run_id, summary.clone())]);
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
    let plan = loom_model::AgentPlan {
        steps: Vec::new(),
        title: None,
    };
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(1),
        retention_limit: 250,
        events: Vec::new(),
        workspace_events: Vec::new(),
    };
    let activity = |timeline_ordinal: u64| AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        timeline_ordinal,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ModelTurn,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(1000 + timeline_ordinal),
        completed_at: Some(Timestamp::from_unix_millis(1001 + timeline_ordinal)),
        elapsed_ms: Some(1),
        data: AgentActivityData::ModelTurn {
            model: ModelId::new("deterministic-model"),
        },
    };
    let first = activity(0);
    let second = activity(1);
    let appended_by_the_checkpoint = activity(2);
    let appended_after_the_save = activity(3);
    let state_save = |activities: &[AgentActivityRecord]| {
        let activities = BTreeMap::from([(run_id, activities.to_vec())]);
        persistence.save_state(DurableStateWrite {
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
    };
    let checkpoint = |deltas: &[AgentActivityRecord]| {
        persistence.save_run_checkpoint(DurableRunCheckpointWrite {
            session: &session,
            session_next_sequence: EventSequence::new(1),
            prune_feed: false,
            summary: &summary,
            runtime_config: &runtime_config,
            context_checkpoint: None,
            plan: &plan,
            messages: &[],
            message_delta: None,
            activities: &[],
            activity_deltas: Some(deltas),
            filesystem: None,
            feed: &feed,
        })
    };

    state_save(&[first.clone(), second.clone()]).unwrap();
    // The worker appends a third activity and checkpoints it without
    // enumerating the run, so the row lands at the next ordinal.
    checkpoint(std::slice::from_ref(&appended_by_the_checkpoint)).unwrap();
    let persisted = vec![
        first.clone(),
        second.clone(),
        appended_by_the_checkpoint.clone(),
    ];
    assert_eq!(persistence.load_run_activities(run_id).unwrap(), persisted);
    // The state save that was already running still carries the older
    // snapshot of two activities.
    state_save(&[first.clone(), second.clone()]).unwrap();
    assert_eq!(
        persistence.load_run_activities(run_id).unwrap(),
        persisted,
        "a state save must not drop an activity a checkpoint already persisted"
    );
    // The next append must extend the stored order instead of taking the
    // ordinal the dropped activity would have reused.
    checkpoint(std::slice::from_ref(&appended_after_the_save)).unwrap();
    let runtime_vector = vec![
        first.clone(),
        second.clone(),
        appended_by_the_checkpoint.clone(),
        appended_after_the_save.clone(),
    ];
    assert_eq!(
        persistence.load_run_activities(run_id).unwrap(),
        runtime_vector
    );
    // The backend hands the runtime vector to the next state save, whose
    // positions have to agree with the stored ordinals.
    state_save(&runtime_vector).unwrap();
    assert_eq!(
        persistence.load_run_activities(run_id).unwrap(),
        runtime_vector,
        "a successful state save keeps every activity in its stored order"
    );

    // A list in a different order changes no stored place: every activity
    // keeps the ordinal it was first written at, and the save succeeds instead
    // of refusing an order it can no longer change.
    let reordered = vec![
        second.clone(),
        first.clone(),
        appended_by_the_checkpoint,
        appended_after_the_save,
    ];
    state_save(&reordered).unwrap();
    assert_eq!(
        persistence.load_run_activities(run_id).unwrap(),
        runtime_vector,
        "a save never renumbers an activity that is already stored"
    );
    drop(persistence);
    fs::remove_file(path).unwrap();
}

/// The database state the old state save left behind when it resolved its
/// activities by vector position and pruned every row its snapshot did not
/// name: it deleted an activity a checkpoint had just committed (the runtime
/// kept the activity and never re-sent it), and the next checkpoint handed the
/// freed ordinal to a different activity.
///
/// A state save has to repair that state instead of refusing the runtime
/// vector: an activity that is already stored keeps its ordinal, an activity
/// the save has not stored yet takes the next free ordinal, and no order is
/// ever refused. Before this rule the save returned the non-retryable
/// `MalformedPayload` "persisted activity order cannot be changed", which
/// latched the worker-wide fail-stop and exited the process. The test recreates
/// the pruned row directly, so it reproduces that refusal without any
/// concurrency.
#[test]
fn a_state_save_repairs_an_activity_ordinal_a_checkpoint_reused() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut sessions = SessionManager::default();
    let (session, _) = sessions
        .create_in_workspace(WorkspaceId::new(), "Activity ordinal repair")
        .unwrap();
    let run_id = RunId::new();
    let started_at = Timestamp::from_unix_millis(1000);
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "repair a reused activity ordinal".to_owned(),
            jev: None,
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at,
            updated_at: started_at,
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let summaries = BTreeMap::from([(run_id, summary.clone())]);
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
    let plan = loom_model::AgentPlan {
        steps: Vec::new(),
        title: None,
    };
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(1),
        retention_limit: 250,
        events: Vec::new(),
        workspace_events: Vec::new(),
    };
    let activity = |timeline_ordinal: u64| AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        timeline_ordinal,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ModelTurn,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(1000 + timeline_ordinal),
        completed_at: Some(Timestamp::from_unix_millis(1001 + timeline_ordinal)),
        elapsed_ms: Some(1),
        data: AgentActivityData::ModelTurn {
            model: ModelId::new("deterministic-model"),
        },
    };
    let first = activity(0);
    let second = activity(1);
    let pruned_by_the_state_save = activity(2);
    let taken_the_freed_ordinal = activity(3);
    let state_save = |activities: &[AgentActivityRecord]| {
        let activities = BTreeMap::from([(run_id, activities.to_vec())]);
        persistence.save_state(DurableStateWrite {
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
    };
    let checkpoint = |deltas: &[AgentActivityRecord]| {
        persistence.save_run_checkpoint(DurableRunCheckpointWrite {
            session: &session,
            session_next_sequence: EventSequence::new(1),
            prune_feed: false,
            summary: &summary,
            runtime_config: &runtime_config,
            context_checkpoint: None,
            plan: &plan,
            messages: &[],
            message_delta: None,
            activities: &[],
            activity_deltas: Some(deltas),
            filesystem: None,
            feed: &feed,
        })
    };
    let stored_ordinal = |activity: &AgentActivityRecord| -> i64 {
        Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT ordinal FROM run_activities WHERE run_id=?1 AND activity_id=?2",
                [
                    run_id.as_uuid().as_bytes().as_slice(),
                    activity.id.as_uuid().as_bytes().as_slice(),
                ],
                |row| row.get(0),
            )
            .unwrap()
    };

    state_save(&[first.clone(), second.clone()]).unwrap();
    checkpoint(std::slice::from_ref(&pruned_by_the_state_save)).unwrap();
    assert_eq!(stored_ordinal(&first), 0);
    assert_eq!(stored_ordinal(&second), 1);
    assert_eq!(stored_ordinal(&pruned_by_the_state_save), 2);

    // The state save that pruned the checkpoint's activity deleted this row;
    // the runtime kept the activity, so it is never sent again.
    Connection::open(&path)
        .unwrap()
        .execute(
            "DELETE FROM run_activities WHERE run_id=?1 AND activity_id=?2",
            [
                run_id.as_uuid().as_bytes().as_slice(),
                pruned_by_the_state_save.id.as_uuid().as_bytes().as_slice(),
            ],
        )
        .unwrap();
    // The next activity the worker records takes the ordinal the deleted row
    // freed.
    checkpoint(std::slice::from_ref(&taken_the_freed_ordinal)).unwrap();
    assert_eq!(stored_ordinal(&taken_the_freed_ordinal), 2);

    // The runtime still holds every activity it recorded, in order, and hands
    // that vector to the next state save.
    let runtime_vector = vec![
        first.clone(),
        second.clone(),
        pruned_by_the_state_save.clone(),
        taken_the_freed_ordinal.clone(),
    ];
    state_save(&runtime_vector).unwrap_or_else(|error| {
        panic!("a state save must repair the reused ordinal instead of refusing it: {error:?}")
    });
    assert_eq!(stored_ordinal(&first), 0);
    assert_eq!(stored_ordinal(&second), 1);
    assert_eq!(
        stored_ordinal(&taken_the_freed_ordinal),
        2,
        "an activity that is already stored keeps its ordinal"
    );
    assert_eq!(
        stored_ordinal(&pruned_by_the_state_save),
        3,
        "an activity the save has not stored yet takes the next free ordinal"
    );
    // The repair is stable: the same vector saved again changes nothing.
    state_save(&runtime_vector).unwrap();
    assert_eq!(
        persistence.load_run_activities(run_id).unwrap(),
        vec![
            first,
            second,
            taken_the_freed_ordinal,
            pruned_by_the_state_save,
        ],
        "the stored order is the order each activity was first written in"
    );
    drop(persistence);
    fs::remove_file(path).unwrap();
}

/// The transcript half of the race the activity tests above pin. A full state
/// save numbers transcript rows by their position in the runtime vector and
/// prunes every row its snapshot does not name, while the streaming fragment
/// writer commits the assistant message that is being produced. The stream
/// reaches storage before the runtime adds the finished message to its vector,
/// so its row is newer than the save's snapshot and sits at the ordinal one past
/// the last message the snapshot names.
///
/// The prune cannot take that row: it only deletes rows whose `fragments` is
/// `'[]'`, and the fragment writer creates the row and commits its descriptors in
/// one transaction. The save also cannot replace text the stream already
/// committed: while a row carries fragments that disagree with the save's
/// content, the stored base is kept and the fragments stay. This test drives both
/// defences without concurrency, and pins that no ordinal changes and no row is
/// duplicated or lost while the saves and the stream interleave.
#[test]
fn a_stale_state_save_keeps_the_streamed_message_the_fragment_writer_committed() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut sessions = SessionManager::default();
    let (session, _) = sessions
        .create_in_workspace(WorkspaceId::new(), "Streamed transcript race")
        .unwrap();
    let run_id = RunId::new();
    let started_at = Timestamp::from_unix_millis(1000);
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "stream a reply while saving state".to_owned(),
            jev: None,
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at,
            updated_at: started_at,
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        },
        usage: UsageSnapshot::default(),
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let summaries = BTreeMap::from([(run_id, summary)]);
    let message = |role: loom_model::MessageRole, content: &str| DurableRunMessage {
        timeline_ordinal: 0,
        role,
        content: content.to_owned(),
        name: None,
        tool_call_id: None,
        tool_calls: Vec::new(),
        reasoning_content: None,
    };
    let state_save = |messages: Vec<DurableRunMessage>| {
        let messages = BTreeMap::from([(run_id, messages)]);
        persistence.save_state(DurableStateWrite {
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
            run_messages: Some(&messages),
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
    };
    let transcript = || {
        persistence
            .load_run_messages(run_id)
            .unwrap()
            .into_iter()
            .map(|message| message.content)
            .collect::<Vec<_>>()
    };
    let user = || message(loom_model::MessageRole::User, "question");
    let assistant = |content: &str| message(loom_model::MessageRole::Assistant, content);

    // The runtime holds the user message when the model turn starts streaming.
    state_save(vec![user()]).unwrap();
    // The first streamed text reaches storage before the finished message joins
    // the runtime vector, so the save that follows still carries a snapshot of
    // one message and must not prune the row the stream committed.
    persistence
        .append_run_message_fragment(run_id, session.id, 1, 0, 0, b"partial")
        .unwrap();
    state_save(vec![user()]).unwrap();
    assert_eq!(
        transcript(),
        vec!["question", "partial"],
        "a state save must not prune the row the fragment writer committed"
    );
    assert_eq!(
        persistence
            .load_run_message_content_range(run_id, 1, 0, 7)
            .unwrap(),
        b"partial"
    );

    // The runtime commits the finished message with exactly the streamed text,
    // which consolidates the row: the content is stored and the descriptors go.
    state_save(vec![user(), assistant("partial")]).unwrap();
    assert_eq!(transcript(), vec!["question", "partial"]);
    assert_eq!(
        stored_fragment_count(&path, run_id),
        0,
        "content that matches the fragments consolidates them"
    );

    // The same assistant message keeps streaming after that consolidation. The
    // next save still carries the shorter content, and must keep the stored base
    // and the newer fragment instead of writing the older text over them.
    persistence
        .append_run_message_fragment(run_id, session.id, 1, 0, 7, b" more")
        .unwrap();
    state_save(vec![user(), assistant("partial")]).unwrap();
    assert_eq!(
        transcript(),
        vec!["question", "partial more"],
        "a state save must not replace a base that carries newer fragments"
    );
    assert_eq!(stored_fragment_count(&path, run_id), 1);
    assert_eq!(
        persistence
            .load_run_message_content_range(run_id, 1, 7, 5)
            .unwrap(),
        b" more"
    );

    // When the runtime's vector finally holds the assembled text, the save
    // consolidates it. Nothing was renumbered or duplicated along the way.
    state_save(vec![user(), assistant("partial more")]).unwrap();
    assert_eq!(transcript(), vec!["question", "partial more"]);
    assert_eq!(stored_fragment_count(&path, run_id), 0);
    let ordinals = {
        let connection = Connection::open(&path).unwrap();
        let mut statement = connection
            .prepare("SELECT ordinal FROM run_messages WHERE run_id=?1 ORDER BY ordinal")
            .unwrap();
        statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        ordinals,
        vec![0, 1],
        "the streamed message keeps the ordinal the save named for it"
    );
    drop(persistence);
    fs::remove_file(path).unwrap();
}
