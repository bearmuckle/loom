//! Persistence tests: content.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn small_content_is_inline_compressed_and_read_by_range() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let text = "repeated transcript text with useful detail\n".repeat(48);
    let connection = Connection::open(&path).unwrap();
    initialize_schema(&connection).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    let hash = store_content(&transaction, text.as_bytes()).unwrap();
    transaction.commit().unwrap();

    let (codec, payload_length, parts, blobs): (i64, i64, i64, i64) = connection
        .query_row(
            "SELECT objects.inline_codec, length(objects.inline_payload),
                    (SELECT COUNT(*) FROM content_parts WHERE content_hash=objects.hash),
                    (SELECT COUNT(*) FROM content_blobs)
             FROM content_objects objects WHERE objects.hash=?1",
            [&hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(codec, 1);
    assert!(payload_length < text.len() as i64);
    assert_eq!((parts, blobs), (0, 0));
    assert_eq!(decode_content(&connection, &hash).unwrap(), text);
    assert_eq!(
        load_content_range(&connection, &hash, 13, 41).unwrap(),
        text.as_bytes()[13..54]
    );

    connection
        .execute(
            "UPDATE content_objects SET inline_payload=x'0102' WHERE hash=?1",
            [&hash],
        )
        .unwrap();
    assert_eq!(
        decode_content(&connection, &hash).unwrap_err().code,
        ErrorCode::MalformedPayload
    );
    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn corrupt_content_objects_are_rejected_during_restore() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let text = "content whose integrity must survive process restarts ".repeat(200);
    let connection = Connection::open(&path).unwrap();
    initialize_schema(&connection).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    store_content(&transaction, text.as_bytes()).unwrap();
    transaction.commit().unwrap();
    let (object_hash, blob_hash): (Vec<u8>, Vec<u8>) = connection
        .query_row(
            "SELECT objects.hash, parts.blob_hash
             FROM content_objects AS objects
             JOIN content_parts AS parts ON parts.content_hash=objects.hash
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    connection
        .execute(
            "UPDATE content_blobs SET codec=1, payload=x'0102' WHERE hash=?1",
            [&blob_hash],
        )
        .unwrap();
    let malformed_compressed = decode_content(&connection, &object_hash).unwrap_err();
    assert_eq!(malformed_compressed.code, ErrorCode::MalformedPayload);

    connection
        .execute(
            "UPDATE content_blobs SET codec=0, payload=x'00' WHERE hash=?1",
            [&blob_hash],
        )
        .unwrap();
    let damaged_content = decode_content(&connection, &object_hash).unwrap_err();
    assert_eq!(damaged_content.code, ErrorCode::MalformedPayload);

    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn reconnect_event_decoder_accepts_compressed_rows_and_rejects_corruption() {
    let session_id = AgentSessionId::new();
    let event = ServerEventEnvelope {
        protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(7),
        session_id,
        event: loom_model::ServerEvent::AgentSessionCreated {
            snapshot: AgentSessionSnapshot {
                id: session_id,
                workspace_id: WorkspaceId::new(),
                name: "compressed event".to_owned(),
                state: AgentSessionState::Idle,
                created_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(1),
                title_source: loom_core::SessionTitleSource::Auto,
            },
        },
    };
    let raw = serde_json::to_vec(&event).unwrap();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&raw).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() < raw.len());
    assert_eq!(
        decode_feed_event(
            7,
            session_id.as_uuid().as_bytes().to_vec(),
            1,
            compressed.clone(),
        )
        .unwrap(),
        event
    );
    assert_eq!(
        decode_feed_event(
            7,
            session_id.as_uuid().as_bytes().to_vec(),
            99,
            compressed.clone(),
        )
        .unwrap_err()
        .code,
        ErrorCode::MalformedPayload
    );
    assert_eq!(
        decode_feed_event(
            7,
            session_id.as_uuid().as_bytes().to_vec(),
            1,
            b"not a zlib stream".to_vec(),
        )
        .unwrap_err()
        .code,
        ErrorCode::MalformedPayload
    );
    assert_eq!(
        decode_feed_event(8, session_id.as_uuid().as_bytes().to_vec(), 1, compressed,)
            .unwrap_err()
            .code,
        ErrorCode::MalformedPayload
    );
}

fn stored_content_objects(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT COUNT(*) FROM content_objects", [], |row| row.get(0))
        .unwrap()
}

fn stored_gc_candidates(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT COUNT(*) FROM content_gc_candidates", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn collect_staged_candidates(connection: &Connection, hashes: [&Vec<u8>; 5]) {
    let transaction = connection.unchecked_transaction().unwrap();
    for hash in hashes {
        transaction
            .execute(
                "INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', ?1)",
                [hash.as_slice()],
            )
            .unwrap();
    }
    collect_unused_content(&transaction, 256).unwrap();
    transaction.commit().unwrap();
}

#[test]
fn content_gc_keeps_objects_referenced_by_indexed_hash_columns() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let connection = Connection::open(&path).unwrap();
    initialize_schema(&connection).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    let message_hash =
        store_content(&transaction, b"transcript content kept by its message").unwrap();
    let summary_hash = store_content(
        &transaction,
        b"context summary content kept by its checkpoint",
    )
    .unwrap();
    let activity_hash = store_content(
        &transaction,
        b"activity payload content kept by its activity",
    )
    .unwrap();
    let arguments_hash =
        store_content(&transaction, b"tool arguments content kept by its call").unwrap();
    let instructions_hash = store_content(
        &transaction,
        b"instructions content kept by its configuration",
    )
    .unwrap();
    transaction.commit().unwrap();

    let run_id = [7u8; 16];
    let session_id = [0u8; 16];
    connection
        .execute(
            "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
             VALUES (?1, ?2, 'content gc session', 'idle', 1, 1)",
            params![session_id.as_slice(), [1u8; 16].as_slice()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO run_summaries(
                run_id, session_id, attempt_id, control_revision, state, started_at,
                updated_at, task, model, input_tokens, output_tokens, cached_input_tokens,
                tool_calls, cost_micros, elapsed_ms)
             VALUES (?1, ?2, ?3, 0, 'executing', 1, 1, 'content gc run', 'model', 0, 0, 0, 0, 0, 0)",
            params![
                run_id.as_slice(),
                session_id.as_slice(),
                [5u8; 16].as_slice()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO run_messages(
                run_id, session_id, ordinal, timeline_ordinal, role, content_hash,
                tool_calls, fragments)
             VALUES (?1, ?2, 0, 0, 'assistant', ?3, '[]', '[]')",
            params![
                run_id.as_slice(),
                session_id.as_slice(),
                message_hash.as_slice()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO run_context_checkpoints(
                run_id, session_id, summary_hash, source_message_count, projection_version,
                source_digest, created_at)
             VALUES (?1, ?2, ?3, 1, 1, 'digest', 1)",
            params![
                run_id.as_slice(),
                session_id.as_slice(),
                summary_hash.as_slice()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO run_activities(
                run_id, session_id, activity_id, ordinal, timeline_ordinal, kind, status,
                started_at, data_hash)
             VALUES (?1, ?2, ?3, 0, 0, 'model_turn', 'completed', 1, ?4)",
            params![
                run_id.as_slice(),
                session_id.as_slice(),
                [9u8; 16].as_slice(),
                activity_hash.as_slice()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO run_tool_calls(
                run_id, session_id, tool_call_id, name, arguments_hash, created_at)
             VALUES (?1, ?2, ?3, 'read_file', ?4, 1)",
            params![
                run_id.as_slice(),
                session_id.as_slice(),
                [8u8; 16].as_slice(),
                arguments_hash.as_slice()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runtime_configurations(
                configuration_hash, system_instructions_hash, repository_instructions_hash,
                policy_read, policy_write, policy_command, policy_network, policy_destructive,
                input_cost_micros_per_1k, output_cost_micros_per_1k)
             VALUES (?1, ?2, NULL, 'allow', 'allow', 'allow', 'allow', 'allow', 0, 0)",
            params![[6u8; 32].as_slice(), instructions_hash.as_slice()],
        )
        .unwrap();

    collect_staged_candidates(
        &connection,
        [
            &message_hash,
            &summary_hash,
            &activity_hash,
            &arguments_hash,
            &instructions_hash,
        ],
    );
    assert_eq!(
        stored_content_objects(&connection),
        5,
        "content referenced by an indexed hash column must survive collection"
    );
    assert_eq!(
        stored_gc_candidates(&connection),
        0,
        "processed candidates must be pruned"
    );

    for table in [
        "run_messages",
        "run_context_checkpoints",
        "run_activities",
        "run_tool_calls",
        "runtime_configurations",
    ] {
        connection
            .execute(&format!("DELETE FROM {table}"), [])
            .unwrap();
    }
    collect_staged_candidates(
        &connection,
        [
            &message_hash,
            &summary_hash,
            &activity_hash,
            &arguments_hash,
            &instructions_hash,
        ],
    );
    assert_eq!(
        stored_content_objects(&connection),
        0,
        "content without any reference must be collected"
    );
    assert_eq!(stored_gc_candidates(&connection), 0);

    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn run_checkpoint_collects_content_orphaned_by_its_own_transcript_reset() {
    let path = std::env::temp_dir().join(format!("loom-persistence-gc-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut sessions = SessionManager::default();
    let (session, _) = sessions
        .create_in_workspace(WorkspaceId::new(), "content gc session")
        .unwrap();
    let run_id = RunId::new();
    let summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "collect orphaned content".to_owned(),
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Executing,
            started_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(1),
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
    let orphaned = "orphaned transcript content ".repeat(400);
    let transcript = BTreeMap::from([(
        run_id,
        vec![DurableRunMessage {
            timeline_ordinal: 0,
            role: loom_model::MessageRole::Assistant,
            content: orphaned,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            reasoning_content: None,
        }],
    )]);
    let summaries = BTreeMap::from([(run_id, summary.clone())]);
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
            run_messages: Some(&transcript),
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
        .unwrap();

    let connection = Connection::open(&path).unwrap();
    let orphaned_hash: Vec<u8> = connection
        .query_row("SELECT hash FROM content_objects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stored_content_objects(&connection), 1);

    let mut updated_summary = summary;
    updated_summary.snapshot.updated_at = Timestamp::from_unix_millis(2);
    let mut updated_session = session;
    updated_session.state = AgentSessionState::Planning;
    updated_session.updated_at = Timestamp::now();
    let delta = DurableRunMessageDelta {
        start_ordinal: 0,
        reset: true,
        messages: vec![DurableRunMessage {
            timeline_ordinal: 1,
            role: loom_model::MessageRole::Assistant,
            content: "replacement transcript content".to_owned(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            reasoning_content: None,
        }],
    };
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
    persistence
        .save_run_checkpoint(DurableRunCheckpointWrite {
            session: &updated_session,
            session_next_sequence: EventSequence::new(1),
            prune_feed: false,
            summary: &updated_summary,
            runtime_config: &runtime_config,
            context_checkpoint: None,
            plan: &plan,
            messages: &[],
            message_delta: Some(&delta),
            activities: &[],
            activity_deltas: Some(&[]),
            filesystem: None,
            feed: &feed,
        })
        .unwrap();

    let remaining: Vec<Vec<u8>> = connection
        .prepare("SELECT hash FROM content_objects")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "the checkpoint must keep exactly the content its transcript references"
    );
    assert!(!remaining.contains(&orphaned_hash));
    assert_eq!(
        stored_gc_candidates(&connection),
        0,
        "one garbage-collection pass per checkpoint must drain its own candidates"
    );
    drop(connection);
    assert_eq!(persistence.load_run_messages(run_id).unwrap().len(), 1);
    drop(persistence);
    fs::remove_file(path).unwrap();
}
