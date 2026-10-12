//! Persistence tests: catalog.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn typed_session_catalog_round_trips_and_uses_the_picker_index() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (first, _) = manager
        .create_in_workspace(WorkspaceId::new(), "First session")
        .unwrap();
    let (second, _) = manager
        .create_in_workspace(first.workspace_id, "Second session")
        .unwrap();
    manager.archive(first.id).unwrap();
    let state = manager.export_state();

    persistence.save_state_with_sessions(&state).unwrap();
    let connection = Connection::open(&path).unwrap();
    let root_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sessions_hierarchy
             WHERE project_id=session_id AND parent_session_id IS NULL AND depth=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(root_rows, state.sessions.len() as i64);
    let first_project = persistence
        .load_project_snapshot(ProjectId::from_uuid(*first.id.as_uuid()))
        .unwrap()
        .unwrap();
    assert_eq!(first_project.root_session_id, first.id);
    assert_eq!(first_project.agents.len(), 1);
    assert_eq!(first_project.agents[0].session_id, first.id);
    assert_eq!(first_project.agents[0].project_id, first_project.project_id);
    assert_eq!(first_project.agents[0].depth, 1);
    assert_eq!(first_project.agents[0].parent_session_id, None);
    let restored = persistence.load_sessions().unwrap().unwrap();
    assert_eq!(restored, state);
    assert_eq!(
        SessionManager::from_state(restored)
            .unwrap()
            .get(second.id)
            .unwrap(),
        second
    );

    let plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN
             SELECT id, name, updated_at FROM sessions
             WHERE workspace_id = ?1 AND state != 'archived'
             ORDER BY updated_at DESC, id DESC LIMIT 20",
            [second.workspace_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(3),
        )
        .unwrap();
    assert!(plan.contains("sessions_visible"), "{plan}");
    connection
        .execute_batch(
            "CREATE TABLE session_updates(count INTEGER NOT NULL);
             INSERT INTO session_updates VALUES (0);
             CREATE TRIGGER track_session_updates AFTER UPDATE ON sessions BEGIN
                UPDATE session_updates SET count=count+1;
             END;",
        )
        .unwrap();
    drop(connection);
    persistence.save_state_with_sessions(&state).unwrap();
    let connection = Connection::open(&path).unwrap();
    let updates: i64 = connection
        .query_row("SELECT count FROM session_updates", [], |row| row.get(0))
        .unwrap();
    assert_eq!(updates, 0, "unchanged rows must not be rewritten");
    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn typed_workspace_catalog_round_trips_and_uses_its_activity_index() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = WorkspaceManager::default();
    let first = manager.create("First workspace").unwrap();
    let second = manager.create("Second workspace").unwrap();
    let state = manager.export_state();

    persistence
        .save_state_with_catalogs_and_feed(&SessionManager::default().export_state(), &state, None)
        .unwrap();
    assert_eq!(persistence.load_workspaces().unwrap().unwrap(), state);

    let connection = Connection::open(&path).unwrap();
    let plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT id, name, updated_at FROM workspaces
             ORDER BY updated_at DESC, id DESC LIMIT 20",
            [],
            |row| row.get(3),
        )
        .unwrap();
    assert!(plan.contains("workspaces_by_activity"), "{plan}");
    connection
        .execute_batch(
            "CREATE TABLE workspace_updates(count INTEGER NOT NULL);
             INSERT INTO workspace_updates VALUES (0);
             CREATE TRIGGER track_workspace_updates AFTER UPDATE ON workspaces BEGIN
                UPDATE workspace_updates SET count=count+1;
             END;",
        )
        .unwrap();
    drop(connection);
    persistence
        .save_state_with_catalogs_and_feed(&SessionManager::default().export_state(), &state, None)
        .unwrap();
    let connection = Connection::open(&path).unwrap();
    let updates: i64 = connection
        .query_row("SELECT count FROM workspace_updates", [], |row| row.get(0))
        .unwrap();
    assert_eq!(updates, 0, "unchanged workspace rows must not be rewritten");
    assert_ne!(first.id, second.id);
    drop(connection);
    fs::remove_file(path).unwrap();
}

#[test]
fn session_and_workspace_settings_are_bounded_indexed_and_atomic() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut sessions = SessionManager::default();
    let (session, _) = sessions
        .create_in_workspace(WorkspaceId::new(), "Settings owner")
        .unwrap();
    let mut workspaces = WorkspaceManager::default();
    let workspace = workspaces.create("Configured workspace").unwrap();
    let settings = DurableSessionSettings {
        approval_policies: BTreeMap::from([(session.id, ApprovalPolicy::default())]),
        auto_approve_actions: BTreeMap::from([(session.id, true)]),
    };
    let config = WorkspaceConfig {
        revision: 4,
        ..WorkspaceConfig::default()
    };
    let configs = BTreeMap::from([(workspace.id, config.clone())]);
    let provider_config = ProviderConfig::deterministic();
    let provider_id = provider_config.id.clone();
    let provider_state = DurableProviderState {
        configs: BTreeMap::from([(provider_id.clone(), provider_config.clone())]),
        health: BTreeMap::from([(provider_id.clone(), ProviderHealth::default())]),
    };
    let model_id = ModelId::new("deterministic-model");
    let mut usage = UsageLedger::default();
    usage.record(
        provider_id.clone(),
        model_id.clone(),
        loom_model::TokenUsage {
            input_tokens: 3,
            output_tokens: 5,
            cached_input_tokens: 1,
        },
        17,
    );
    usage.record(
        provider_id.clone(),
        model_id.clone(),
        loom_model::TokenUsage {
            input_tokens: 4,
            output_tokens: 2,
            cached_input_tokens: 0,
        },
        9,
    );
    let request_id = RequestId::new();
    let idempotency = BTreeMap::from([(
        request_id,
        DurableIdempotencyRecord {
            created_at: Timestamp::from_unix_millis(1234),
            expires_at: Some(Timestamp::from_unix_millis(604_801_234)),
            request: serde_json::json!({"method": "list_sessions"}),
            response: serde_json::json!({"sessions": []}),
        },
    )]);
    let run_id = RunId::new();
    let run_summary = DurableRunSummary {
        snapshot: AgentRunSnapshot {
            id: run_id,
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id: session.id,
            task: "indexed run summary".to_owned(),
            model: ModelId::new("deterministic-model"),
            state: AgentRunState::Completed,
            started_at: Timestamp::from_unix_millis(1000),
            updated_at: Timestamp::from_unix_millis(2000),
            completed_at: Some(Timestamp::from_unix_millis(2000)),
            summary: Some("finished".to_owned()),
            evidence: vec![loom_core::EvidenceLink {
                label: "Build output".to_owned(),
                uri: "file:///workspace/build.log".to_owned(),
            }],
        },
        usage: UsageSnapshot {
            input_tokens: 13,
            output_tokens: 7,
            ..UsageSnapshot::default()
        },
        attempts: None,
        execution_state: None,
        interactions: None,
    };
    let second_run_id = RunId::new();
    let mut second_summary = run_summary.clone();
    second_summary.snapshot.id = second_run_id;
    second_summary.snapshot.attempt_id = loom_core::RunAttemptId::new();
    second_summary.usage = UsageSnapshot::default();
    let run_summaries = BTreeMap::from([(run_id, run_summary), (second_run_id, second_summary)]);
    let run_runtime_config = DurableRunRuntimeConfig {
        system_instructions: Some("Use the project conventions".to_owned()),
        repository_instructions: Some("Do not modify generated files".to_owned()),
        approval_policy: ApprovalPolicy {
            write: PolicyDecision::Deny,
            ..ApprovalPolicy::default()
        },
        limits: SessionLimits {
            max_duration_ms: Some(60_000),
            max_input_tokens: Some(12_000),
            max_output_tokens: Some(3_000),
            max_tool_calls: Some(12),
            max_cost_micros: Some(250_000),
        },
        context_options: ContextAssemblyOptions {
            context_window: Some(16_000),
            max_input_tokens: Some(12_000),
            reserved_output_tokens: Some(3_000),
        },
        checkpoint_id: Some(CheckpointId::new()),
        input_cost_micros_per_1k: 17,
        output_cost_micros_per_1k: 29,
        context_inspection: None,
        project_delegation_enabled: true,
        project_messaging_enabled: true,
        project_inspection_enabled: true,
        project_child_control_enabled: true,
        project_worktree_enabled: true,
        project_review_enabled: false,
        project_integration_enabled: false,
        project_branch_messaging_enabled: true,
    };
    let run_runtime_configs = BTreeMap::from([
        (run_id, run_runtime_config.clone()),
        (second_run_id, run_runtime_config.clone()),
    ]);
    let context_checkpoint = DurableRunContextCheckpoint {
        session_id: session.id,
        summary: ContextSummary {
            text: "older conversation summary".to_owned(),
            source_message_count: 12,
            projection_version: 1,
            source_digest: "ab".repeat(32),
            created_at: Timestamp::from_unix_millis(2100),
        },
    };
    let run_context_checkpoints = BTreeMap::from([(run_id, Some(context_checkpoint.clone()))]);
    let run_plans = BTreeMap::from([(
        run_id,
        AgentPlan {
            steps: vec![AgentPlanStep {
                id: "inspect".to_owned(),
                description: "Inspect the relevant source and build output".to_owned(),
            }],
        },
    )]);
    let activity_call = loom_model::ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "web_search".to_owned(),
        arguments: serde_json::json!({"query": "needle"}),
    };
    let activity = AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        timeline_ordinal: 0,
        parent_id: None,
        step_id: Some(StepId::new()),
        kind: AgentActivityKind::ToolCall,
        status: AgentActivityStatus::AwaitingApproval,
        started_at: Timestamp::from_unix_millis(1500),
        completed_at: None,
        elapsed_ms: None,
        data: AgentActivityData::ToolCall {
            call: activity_call.clone(),
            result: None,
        },
    };
    let run_activities = BTreeMap::from([(run_id, vec![activity.clone()])]);
    let run_messages = BTreeMap::from([(
        run_id,
        vec![
            DurableRunMessage {
                timeline_ordinal: 0,
                role: loom_model::MessageRole::User,
                content: "large transcript content ".repeat(500),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            DurableRunMessage {
                timeline_ordinal: 0,
                role: loom_model::MessageRole::Assistant,
                content: String::new(),
                name: Some("assistant".to_owned()),
                tool_call_id: None,
                tool_calls: vec![loom_model::ToolCall {
                    id: loom_core::ToolCallId::new(),
                    name: "inspect".to_owned(),
                    arguments: serde_json::json!({"path": "src/main.rs"}),
                }],
                reasoning_content: Some("step-by-step".to_owned()),
            },
        ],
    )]);
    let checkpoint_id = CheckpointId::new();
    let checkpoint = Checkpoint {
        id: checkpoint_id,
        session_id: session.id,
        label: "rollback point".to_owned(),
        created_at: Timestamp::from_unix_millis(4321),
        files: BTreeMap::from([(
            "src/main.rs".to_owned(),
            CheckpointFile {
                existed: true,
                content: "checkpoint text ".repeat(500),
                revision: "revision-a".to_owned(),
                expected_revision: "revision-b".to_owned(),
            },
        )]),
    };
    let repository_id = RepositoryId::new();
    let repositories = BTreeMap::from([(
        repository_id,
        SessionRepository {
            id: repository_id,
            source: "https://example.test/repo.git".to_owned(),
            path: "repositories/example".to_owned(),
            revision: Some("abc123".to_owned()),
            attached_at: Timestamp::from_unix_millis(4322),
        },
    )]);
    let directories = vec![SessionDirectory {
        source: "/tmp/external-docs".to_owned(),
        path: "docs".to_owned(),
    }];
    let filesystem_records = [DurableFilesystemRecord {
        session_id: session.id,
        root: "/tmp/loom-session-fs".to_owned(),
        control: WorkspaceControl::Agent,
        checkpoints: vec![checkpoint.clone()],
        edits: vec![DurableFilesystemEdit {
            id: 1,
            path: "src/main.rs".to_owned(),
            before: Some("before contents".to_owned()),
            before_bytes: Some(b"before contents".to_vec()),
            after_revision: "revision-after".to_owned(),
            source: WorkspaceControl::Agent,
        }],
        changes: vec![SessionFilesystemChange {
            sequence: EventSequence::new(1),
            session_id: session.id,
            path: "src/main.rs".to_owned(),
            kind: WorkspaceChangeKind::Modified,
            revision: Some("revision-after".to_owned()),
        }],
        repositories: repositories.clone(),
        directories: directories.clone(),
        payload: serde_json::json!({
            "filesystem": {
                "session_id": session.id,
                "root": "/tmp/loom-session-fs",
                "control": "agent",
                "checkpoints": [],
                "edits": [],
                "next_sequence": 1,
                "changes": []
            },
            "details": "checkpoint state ".repeat(500)
        }),
        delta: None,
    }];
    persistence
        .save_state(DurableStateWrite {
            sessions: &sessions.export_state(),
            workspaces: Some(&workspaces.export_state()),
            settings: Some(&settings),
            workspace_configs: Some(&configs),
            providers: Some(&provider_state),
            usage: Some(&usage),
            idempotency: Some(&idempotency),
            run_summaries: Some(&run_summaries),
            run_runtime_configs: Some(&run_runtime_configs),
            run_context_checkpoints: Some(&run_context_checkpoints),
            run_plans: Some(&run_plans),
            run_messages: Some(&run_messages),
            run_activities: Some(&run_activities),
            filesystem_records: Some(&filesystem_records),
            feed: None,
        })
        .unwrap();
    assert_eq!(
        persistence
            .load_session_settings()
            .unwrap()
            .approval_policies,
        settings.approval_policies
    );
    assert_eq!(
        persistence
            .load_session_settings()
            .unwrap()
            .auto_approve_actions,
        settings.auto_approve_actions
    );
    assert_eq!(persistence.load_workspace_configs().unwrap(), configs);
    assert_eq!(
        persistence.load_run_runtime_config(run_id).unwrap(),
        Some(run_runtime_config.clone())
    );
    assert_eq!(
        persistence.load_provider_configs().unwrap(),
        vec![provider_config]
    );
    assert_eq!(
        persistence.load_provider_health().unwrap(),
        provider_state.health
    );
    assert_eq!(persistence.load_provider_usage().unwrap(), usage);
    assert_eq!(
        persistence.load_run_activities(run_id).unwrap(),
        vec![activity.clone()]
    );
    assert_eq!(
        persistence.load_run_tool_calls(run_id).unwrap(),
        vec![AgentToolCallRecord {
            run_id,
            session_id: session.id,
            call: activity_call.clone(),
            created_at: Timestamp::from_unix_millis(1500),
        }]
    );
    assert_eq!(
        persistence.load_run_tool_attempts(run_id).unwrap(),
        vec![AgentToolAttemptRecord {
            run_id,
            session_id: session.id,
            id: activity.id,
            call_id: activity_call.id,
            attempt_number: 1,
            state: AgentToolAttemptState::AwaitingApproval,
            started_at: Timestamp::from_unix_millis(1500),
            completed_at: None,
            result: None,
        }]
    );
    let mut completed_activity = activity.clone();
    completed_activity.status = AgentActivityStatus::Completed;
    completed_activity.completed_at = Some(Timestamp::from_unix_millis(1600));
    completed_activity.elapsed_ms = Some(100);
    let tool_result = ToolResult {
        tool_call_id: activity_call.id,
        name: activity_call.name.clone(),
        success: true,
        output: "read result".to_owned(),
    };
    completed_activity.data = AgentActivityData::ToolCall {
        call: activity_call.clone(),
        result: Some(tool_result.clone()),
    };
    let completed_activities = BTreeMap::from([(run_id, vec![completed_activity])]);
    let invalid_feed = invalid_feed_for_rollback();
    assert!(
        persistence
            .save_state(DurableStateWrite {
                sessions: &sessions.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: None,
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: None,
                run_activities: Some(&completed_activities),
                filesystem_records: None,
                feed: Some(&invalid_feed),
            })
            .is_err()
    );
    assert_eq!(
        persistence.load_run_activities(run_id).unwrap(),
        vec![activity.clone()]
    );
    assert_eq!(
        persistence.load_run_tool_attempts(run_id).unwrap()[0].state,
        AgentToolAttemptState::AwaitingApproval
    );
    persistence
        .save_state(DurableStateWrite {
            sessions: &sessions.export_state(),
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: None,
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: Some(&completed_activities),
            filesystem_records: None,
            feed: None,
        })
        .unwrap();
    assert_eq!(
        persistence.load_run_tool_attempts(run_id).unwrap(),
        vec![AgentToolAttemptRecord {
            run_id,
            session_id: session.id,
            id: activity.id,
            call_id: activity_call.id,
            attempt_number: 1,
            state: AgentToolAttemptState::Completed,
            started_at: Timestamp::from_unix_millis(1500),
            completed_at: Some(Timestamp::from_unix_millis(1600)),
            result: Some(tool_result),
        }]
    );
    let loaded_idempotency = persistence.load_idempotency_records().unwrap();
    assert_eq!(loaded_idempotency.len(), 1);
    let loaded_record = &loaded_idempotency[&request_id];
    assert_eq!(loaded_record.created_at, Timestamp::from_unix_millis(1234));
    assert_eq!(
        loaded_record.expires_at,
        Some(Timestamp::from_unix_millis(604_801_234))
    );
    assert_eq!(
        loaded_record.request,
        serde_json::json!({"method": "list_sessions"})
    );
    assert_eq!(loaded_record.response, serde_json::json!({"sessions": []}));
    assert_eq!(persistence.load_run_summaries().unwrap(), run_summaries);
    assert_eq!(
        persistence.load_run_plan(run_id).unwrap(),
        run_plans[&run_id]
    );
    assert_eq!(
        persistence
            .load_session_usage(session.id, &BTreeSet::new())
            .unwrap(),
        run_summaries[&run_id].usage
    );
    assert_eq!(
        persistence
            .load_session_usage(session.id, &BTreeSet::from([run_id]))
            .unwrap(),
        UsageSnapshot::default()
    );
    assert_eq!(
        persistence
            .load_active_run_summaries()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        run_summaries
            .iter()
            .filter(|(_, summary)| !matches!(
                summary.snapshot.state,
                AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
            ))
            .map(|(run_id, _)| *run_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        persistence.load_run_summary(run_id).unwrap(),
        run_summaries.get(&run_id).cloned()
    );
    assert_eq!(
        persistence.load_run_runtime_config(run_id).unwrap(),
        Some(run_runtime_config)
    );
    assert_eq!(
        persistence.load_run_runtime_config(second_run_id).unwrap(),
        Some(run_runtime_configs[&run_id].clone())
    );
    let connection = persistence.connection().unwrap();
    let (stored_tool_limit, stored_policy, limit_storage_type): (i64, String, String) = connection
        .query_row(
            "SELECT max_tool_calls, policy_write, typeof(max_tool_calls)
                 FROM run_runtime_config JOIN runtime_configurations USING(configuration_hash)
                 WHERE run_id=?1",
            [run_id.as_uuid().as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(stored_tool_limit, 12);
    assert_eq!(stored_policy, "deny");
    assert_eq!(limit_storage_type, "integer");
    let runtime_config_columns = connection
        .prepare("PRAGMA table_info(runtime_configurations)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<BTreeSet<_>, _>>()
        .unwrap();
    assert!(!runtime_config_columns.contains("options"));
    assert!(!runtime_config_columns.contains("approval_policy"));
    let profile_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM runtime_configurations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        profile_count, 1,
        "identical runtime configurations share one profile"
    );
    drop(connection);
    let mut changed_runtime_config = run_runtime_configs[&run_id].clone();
    changed_runtime_config.approval_policy.write = PolicyDecision::Allow;
    changed_runtime_config.project_branch_messaging_enabled = false;
    let mut changed_configs = BTreeMap::from([(run_id, changed_runtime_config.clone())]);
    let mut connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    let transaction = connection.transaction().unwrap();
    save_run_runtime_config_rows(&transaction, &changed_configs).unwrap();
    transaction.commit().unwrap();
    let remaining_profiles: i64 = connection
        .query_row("SELECT COUNT(*) FROM runtime_configurations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        remaining_profiles, 2,
        "the shared old profile remains in use"
    );
    drop(connection);
    assert_eq!(
        persistence.load_run_runtime_config(run_id).unwrap(),
        Some(changed_runtime_config.clone())
    );
    assert_eq!(
        persistence.load_run_runtime_config(second_run_id).unwrap(),
        Some(run_runtime_configs[&run_id].clone())
    );

    changed_configs.insert(second_run_id, changed_runtime_config.clone());
    let mut connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    let transaction = connection.transaction().unwrap();
    save_run_runtime_config_rows(&transaction, &changed_configs).unwrap();
    transaction.commit().unwrap();
    let remaining_profiles: i64 = connection
        .query_row("SELECT COUNT(*) FROM runtime_configurations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(remaining_profiles, 1, "the unused profile is collected");
    drop(connection);
    assert_eq!(
        persistence.load_run_runtime_config(second_run_id).unwrap(),
        Some(changed_runtime_config)
    );
    assert_eq!(
        persistence.load_run_context_checkpoint(run_id).unwrap(),
        Some(context_checkpoint.clone())
    );
    assert_eq!(
        persistence
            .load_latest_run_summary_for_session(session.id)
            .unwrap()
            .as_ref()
            .map(|summary| summary.snapshot.session_id),
        Some(session.id)
    );
    assert_eq!(
        persistence
            .load_run_summaries_for_session(session.id)
            .unwrap()
            .len(),
        run_summaries
            .values()
            .filter(|summary| summary.snapshot.session_id == session.id)
            .count()
    );
    assert_eq!(
        persistence.load_run_messages(run_id).unwrap(),
        run_messages[&run_id]
    );
    let newest_page = persistence.load_run_message_page(run_id, None, 1).unwrap();
    assert_eq!(newest_page.len(), 1);
    assert_eq!(newest_page[0].ordinal, 1);
    assert_eq!(
        newest_page[0].tool_calls,
        run_messages[&run_id][1].tool_calls
    );
    assert_eq!(
        newest_page[0].reasoning_content.as_deref(),
        Some("step-by-step")
    );
    assert_eq!(
        persistence.list_filesystem_sessions().unwrap(),
        vec![session.id]
    );
    let loaded_filesystem = persistence
        .load_filesystem_record(session.id)
        .unwrap()
        .unwrap();
    assert_eq!(loaded_filesystem.payload, filesystem_records[0].payload);
    assert_eq!(loaded_filesystem.edits, filesystem_records[0].edits);
    assert!(loaded_filesystem.changes.is_empty());
    assert_eq!(
        persistence
            .load_filesystem_changes_page(session.id, None, 512)
            .unwrap()
            .changes,
        filesystem_records[0].changes
    );
    let watcher_change = SessionFilesystemChange {
        sequence: EventSequence::new(2),
        session_id: session.id,
        path: "src/generated.rs".to_owned(),
        kind: WorkspaceChangeKind::Created,
        revision: Some("revision-new".to_owned()),
    };
    persistence
        .save_filesystem_changes(
            session.id,
            EventSequence::new(2),
            std::slice::from_ref(&watcher_change),
        )
        .unwrap();
    let reloaded_filesystem = persistence
        .load_filesystem_record(session.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded_filesystem.payload["filesystem"]["next_sequence"],
        serde_json::json!(2)
    );
    assert_eq!(
        persistence
            .load_filesystem_changes_page(session.id, Some(EventSequence::new(1)), 512)
            .unwrap()
            .changes,
        vec![watcher_change]
    );
    let invalid_change = SessionFilesystemChange {
        sequence: EventSequence::new(3),
        session_id: AgentSessionId::new(),
        path: "wrong-session.txt".to_owned(),
        kind: WorkspaceChangeKind::Created,
        revision: None,
    };
    assert!(
        persistence
            .save_filesystem_changes(session.id, EventSequence::new(3), &[invalid_change],)
            .is_err()
    );
    assert_eq!(
        persistence
            .load_filesystem_record(session.id)
            .unwrap()
            .unwrap()
            .payload["filesystem"]["next_sequence"],
        serde_json::json!(2),
        "sequence high-water and change rows commit atomically"
    );
    assert_eq!(loaded_filesystem.repositories, repositories);
    assert_eq!(loaded_filesystem.directories, directories);
    assert_eq!(loaded_filesystem.checkpoints, vec![checkpoint.clone()]);
    assert!(loaded_filesystem.payload.get("repositories").is_none());
    assert!(loaded_filesystem.payload.get("directories").is_none());

    let invalid_feed = invalid_feed_for_rollback();
    assert!(
        persistence
            .save_state(DurableStateWrite {
                sessions: &sessions.export_state(),
                workspaces: Some(&workspaces.export_state()),
                settings: Some(&DurableSessionSettings::default()),
                workspace_configs: Some(&BTreeMap::new()),
                providers: Some(&DurableProviderState::default()),
                usage: Some(&UsageLedger::default()),
                idempotency: Some(&BTreeMap::new()),
                run_summaries: Some(&run_summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: Some(&BTreeMap::new()),
                run_messages: None,
                run_activities: None,
                filesystem_records: Some(&[DurableFilesystemRecord {
                    checkpoints: Vec::new(),
                    ..filesystem_records[0].clone()
                }]),
                feed: Some(&invalid_feed),
            })
            .is_err()
    );
    assert_eq!(
        persistence.load_run_plan(run_id).unwrap(),
        run_plans[&run_id]
    );
    assert_eq!(
        persistence.load_run_summary(run_id).unwrap(),
        run_summaries.get(&run_id).cloned()
    );
    assert_eq!(
        persistence
            .load_session_settings()
            .unwrap()
            .approval_policies,
        settings.approval_policies
    );
    assert_eq!(persistence.load_workspace_configs().unwrap(), configs);
    assert_eq!(
        persistence.load_provider_health().unwrap(),
        provider_state.health
    );
    assert_eq!(persistence.load_provider_usage().unwrap(), usage);
    assert_eq!(persistence.load_idempotency_records().unwrap(), idempotency);
    assert_eq!(persistence.load_run_summaries().unwrap(), run_summaries);
    let retained_filesystem = persistence
        .load_filesystem_record(session.id)
        .unwrap()
        .unwrap();
    let mut expected_filesystem_payload = filesystem_records[0].payload.clone();
    expected_filesystem_payload["filesystem"]["next_sequence"] = serde_json::json!(2);
    assert_eq!(retained_filesystem.payload, expected_filesystem_payload);
    assert_eq!(retained_filesystem.checkpoints, vec![checkpoint.clone()]);

    let connection = Connection::open(&path).unwrap();
    let (stored_policy, policy_storage_type): (String, String) = connection
        .query_row(
            "SELECT policy_write, typeof(policy_write) FROM session_settings
             WHERE session_id=?1",
            [session.id.as_uuid().as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored_policy, "require_approval");
    assert_eq!(policy_storage_type, "text");
    let settings_columns = connection
        .prepare("PRAGMA table_info(session_settings)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<BTreeSet<_>, _>>()
        .unwrap();
    assert!(!settings_columns.contains("approval_policy"));
    let repo_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT path FROM session_repositories
             WHERE session_id=?1 AND path=?2",
            params![
                session.id.as_uuid().as_bytes().as_slice(),
                "repositories/example"
            ],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        repo_plan.contains("session_repositories_by_path"),
        "{repo_plan}"
    );
    let directory_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT source FROM session_directories
             WHERE session_id=?1 AND path=?2",
            params![session.id.as_uuid().as_bytes().as_slice(), "docs"],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        directory_plan.contains("sqlite_autoindex_session_directories_2"),
        "{directory_plan}"
    );
    let summary_text: Option<String> = connection
        .query_row(
            "SELECT summary FROM run_summaries WHERE run_id=?1",
            [run_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(summary_text.as_deref(), Some("finished"));
    let plan_rows: i64 = connection
        .query_row(
            "SELECT count(*) FROM run_plan_steps WHERE run_id=?1",
            [run_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(plan_rows, run_plans[&run_id].steps.len() as i64);
    let plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT revision, config FROM workspace_configs WHERE workspace_id=?1",
            [workspace.id.as_uuid().as_bytes().as_slice()],
            |row| row.get(3),
        )
        .unwrap();
    assert!(plan.contains("PRIMARY KEY"), "{plan}");
    let usage_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT SUM(input_tokens) FROM run_summaries WHERE session_id=?1",
            [session.id.as_uuid().as_bytes().as_slice()],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        usage_plan.contains("runs_by_session_activity"),
        "{usage_plan}"
    );
    let idempotency_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT request_id FROM idempotency_records
             WHERE expires_at IS NOT NULL AND expires_at<=?1
             ORDER BY expires_at, request_id LIMIT 32",
            [Timestamp::now().as_unix_millis() as i64],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        idempotency_plan.contains("idempotency_expiry"),
        "{idempotency_plan}"
    );
    let run_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT run_id FROM run_summaries
             WHERE session_id=?1 ORDER BY updated_at DESC, run_id DESC LIMIT 50",
            [session.id.as_uuid().as_bytes().as_slice()],
            |row| row.get(3),
        )
        .unwrap();
    assert!(run_plan.contains("runs_by_session_activity"), "{run_plan}");
    let filesystem_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT payload FROM session_filesystems WHERE session_id=?1",
            [session.id.as_uuid().as_bytes().as_slice()],
            |row| row.get(3),
        )
        .unwrap();
    assert!(filesystem_plan.contains("PRIMARY KEY"), "{filesystem_plan}");
    let filesystem_change_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT sequence FROM filesystem_changes
             WHERE session_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 32",
            params![session.id.as_uuid().as_bytes().as_slice(), 0_i64],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        filesystem_change_plan.contains("PRIMARY KEY"),
        "{filesystem_change_plan}"
    );
    let checkpoint_file_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT content_hash FROM checkpoint_files
             WHERE session_id=?1 AND checkpoint_id=?2 ORDER BY path",
            params![
                session.id.as_uuid().as_bytes().as_slice(),
                checkpoint_id.as_uuid().as_bytes().as_slice()
            ],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        checkpoint_file_plan.contains("PRIMARY KEY"),
        "{checkpoint_file_plan}"
    );
    let run_message_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT role, content_hash FROM run_messages
             WHERE run_id=?1 ORDER BY ordinal",
            [run_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        run_message_plan.contains("PRIMARY KEY"),
        "{run_message_plan}"
    );
    let activity_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT activity_id FROM run_activities
             WHERE session_id=?1 ORDER BY started_at DESC, activity_id DESC LIMIT 20",
            [session.id.as_uuid().as_bytes().as_slice()],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        activity_plan.contains("run_activities_by_session_time"),
        "{activity_plan}"
    );
    let (content_codec, content_count): (i64, i64) = connection
        .query_row(
            "SELECT MAX(codec), COUNT(*) FROM content_blobs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(content_codec, 1);
    assert!(content_count > 0 && content_count < 9);
    let (filesystem_codec, raw_size, payload_size): (i64, i64, i64) = connection
        .query_row(
            "SELECT payload_codec, raw_size, length(payload) FROM session_filesystems
             WHERE session_id=?1",
            [session.id.as_uuid().as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(filesystem_codec, 1);
    assert!(payload_size < raw_size);
    let config_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT config FROM provider_configs WHERE provider_id=?1",
            [provider_id.as_str()],
            |row| row.get(3),
        )
        .unwrap();
    assert!(config_plan.contains("PRIMARY KEY"), "{config_plan}");
    let usage_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT requests FROM provider_usage_totals
             WHERE provider_id=?1 AND model_id=?2",
            params![provider_id.as_str(), model_id.as_str()],
            |row| row.get(3),
        )
        .unwrap();
    assert!(usage_plan.contains("PRIMARY KEY"), "{usage_plan}");
    drop(connection);

    let empty_filesystem_records = [DurableFilesystemRecord {
        checkpoints: Vec::new(),
        ..filesystem_records[0].clone()
    }];
    persistence
        .save_state(DurableStateWrite {
            sessions: &sessions.export_state(),
            workspaces: Some(&workspaces.export_state()),
            settings: Some(&settings),
            workspace_configs: Some(&configs),
            providers: Some(&provider_state),
            usage: Some(&usage),
            idempotency: Some(&idempotency),
            run_summaries: Some(&run_summaries),
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: Some(&empty_filesystem_records),
            feed: None,
        })
        .unwrap();
    assert!(
        persistence
            .load_filesystem_record(session.id)
            .unwrap()
            .unwrap()
            .checkpoints
            .is_empty()
    );
    let connection = Connection::open(&path).unwrap();
    let content_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM content_objects", [], |row| row.get(0))
        .unwrap();
    // Message tool calls and tool attempts are inline JSON now, so the
    // reachable content objects are the transcript, context checkpoint,
    // logical tool-call arguments, filesystem undo, and run instructions.
    assert!(
        content_count >= 7,
        "retained transcript, context checkpoint, logical tool-call, filesystem undo, and run-instruction content remain reachable"
    );
    drop(connection);
    assert_eq!(
        persistence
            .prune_expired_idempotency_records(Timestamp::now())
            .unwrap(),
        1
    );
    assert!(persistence.load_idempotency_records().unwrap().is_empty());
    fs::remove_file(path).unwrap();
}

#[test]
fn workspace_only_events_share_the_cursor_and_persist_with_bounded_retention() {
    let path = std::env::temp_dir().join(format!("loom-workspace-feed-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    let workspace_id = WorkspaceId::new();
    let workspace_event = |sequence, name: &str| WorkspaceEventEnvelope {
        protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
        sequence: EventSequence::new(sequence),
        workspace_id,
        event: loom_model::WorkspaceEvent::Renamed {
            name: name.to_owned(),
        },
    };
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(2),
        retention_limit: 1,
        events: Vec::new(),
        workspace_events: vec![workspace_event(1, "First"), workspace_event(2, "Second")],
    };
    store
        .save_state_with_sessions_and_feed(&SessionManager::default().export_state(), Some(&feed))
        .unwrap();
    let events = store
        .load_feed_workspace_events_since(workspace_id, None)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], WorkspaceFeedEvent::Workspace(event)
        if event.sequence == EventSequence::new(2)
            && matches!(&event.event, loom_model::WorkspaceEvent::Renamed { name } if name == "Second")));
    let cursor = store
        .load_feed_workspace_cursor(workspace_id)
        .unwrap()
        .unwrap();
    assert_eq!(cursor.latest_sequence, EventSequence::new(2));
    assert_eq!(cursor.pruned_through, EventSequence::new(1));
    assert_eq!(cursor.oldest_retained_sequence, Some(EventSequence::new(2)));
    let loaded = store.load_feed_state().unwrap().unwrap();
    assert_eq!(loaded.workspace_events.len(), 1);
    assert_eq!(loaded.workspace_events[0].sequence, EventSequence::new(2));
    fs::remove_file(path).unwrap();
}

fn child_snapshot(
    id: AgentSessionId,
    workspace_id: WorkspaceId,
    name: &str,
) -> AgentSessionSnapshot {
    AgentSessionSnapshot {
        id,
        workspace_id,
        name: name.to_owned(),
        state: AgentSessionState::Idle,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    }
}

fn delegated_task(
    project_id: ProjectId,
    requester_session_id: AgentSessionId,
    target_session_id: AgentSessionId,
    child_name: &str,
    dependencies: Vec<TaskId>,
) -> DelegatedTaskRecord {
    DelegatedTaskRecord {
        task_id: TaskId::new(),
        project_id,
        requester_session_id,
        target_session_id,
        child_name: child_name.to_owned(),
        intent: "inspect the persistence layer".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies,
        code_change: false,
        permissions: ProjectAgentPermissions::default(),
        status: DelegatedTaskStatus::Queued,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    }
}

fn session_row_count(
    connection: &Connection,
    table: &str,
    column: &str,
    session_id: AgentSessionId,
) -> i64 {
    connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {column}=?1"),
            [session_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
}

fn table_count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn task_row_count(connection: &Connection, table: &str, column: &str, task_id: TaskId) -> i64 {
    connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {column}=?1"),
            [task_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
}

fn content_object_exists(connection: &Connection, hash: &[u8]) -> bool {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM content_objects WHERE hash=?1)",
            [hash],
            |row| row.get(0),
        )
        .unwrap()
}

fn queued_content_candidates(connection: &Connection, hash: &[u8]) -> i64 {
    connection
        .query_row(
            "SELECT COUNT(*) FROM content_gc_candidates WHERE hash=?1",
            [hash],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn deleted_sessions_lose_their_rows_and_unused_content_in_one_transaction() {
    let path = std::env::temp_dir().join(format!("loom-persistence-delete-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let workspace_id = WorkspaceId::new();
    let mut manager = SessionManager::default();
    let (root, _) = manager
        .create_in_workspace(workspace_id, "Project manager")
        .unwrap();
    persistence
        .save_state_with_sessions(&manager.export_state())
        .unwrap();

    let project_id = ProjectId::from_uuid(*root.id.as_uuid());
    let survivor_id = AgentSessionId::new();
    let deleted_id = AgentSessionId::new();
    let grandchild_id = AgentSessionId::new();
    let survivor_snapshot = child_snapshot(survivor_id, workspace_id, "Surviving child");
    let deleted_snapshot = child_snapshot(deleted_id, workspace_id, "Deleted child");
    let grandchild_snapshot = child_snapshot(grandchild_id, workspace_id, "Grandchild");

    let survivor_task = persistence
        .create_project_child(
            RequestId::new(),
            &survivor_snapshot,
            manager.export_state().next_sequence,
            &delegated_task(
                project_id,
                root.id,
                survivor_id,
                "Surviving child",
                Vec::new(),
            ),
        )
        .unwrap();
    let deleted_task = delegated_task(
        project_id,
        root.id,
        deleted_id,
        "Deleted child",
        vec![survivor_task.task_id],
    );
    let initial_worktree = ProjectWorktreeRecord {
        project_id,
        task_id: deleted_task.task_id,
        parent_session_id: root.id,
        child_session_id: deleted_id,
        parent_repository_id: RepositoryId::new(),
        child_repository_id: RepositoryId::new(),
        relative_path: "worktrees/deleted-child".to_owned(),
        worktree_name: "deleted-child".to_owned(),
        branch_name: "loom/deleted-child".to_owned(),
        base_revision: "base-sha".to_owned(),
        result_revision: None,
        integrated_revision: None,
        status: ProjectWorktreeStatus::Creating,
        conflict_paths: Vec::new(),
        error: None,
        cleanup_disposition: None,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    };
    let created_task = persistence
        .create_project_child_with_worktree(
            RequestId::new(),
            &deleted_snapshot,
            manager.export_state().next_sequence,
            &deleted_task,
            &initial_worktree,
        )
        .unwrap();
    // The deepest task survives and depends on the deleted session's task, so
    // the deletion has to remove a dependency edge whose owning task outlives it.
    let grandchild_task = persistence
        .create_project_child(
            RequestId::new(),
            &grandchild_snapshot,
            manager.export_state().next_sequence,
            &delegated_task(
                project_id,
                survivor_id,
                grandchild_id,
                "Grandchild",
                vec![created_task.task_id],
            ),
        )
        .unwrap();

    // A message that only references the deleted task would block the delegated
    // task cascade, so the deletion has to remove it first.
    let task_message = persistence
        .accept_agent_message(
            RequestId::new(),
            &AgentMessageDraft {
                project_id,
                task_id: Some(created_task.task_id),
                sender_session_id: survivor_id,
                target_session_id: root.id,
                kind: AgentMessageKind::Question,
                body: "Is the deleted child done?".to_owned(),
            },
        )
        .unwrap();
    let targeted_message = persistence
        .accept_agent_message(
            RequestId::new(),
            &AgentMessageDraft {
                project_id,
                task_id: None,
                sender_session_id: root.id,
                target_session_id: deleted_id,
                kind: AgentMessageKind::Direction,
                body: "Wrap up the child task".to_owned(),
            },
        )
        .unwrap();
    assert_ne!(task_message.message_id, targeted_message.message_id);
    let surviving_message = persistence
        .accept_agent_message(
            RequestId::new(),
            &AgentMessageDraft {
                project_id,
                task_id: Some(survivor_task.task_id),
                sender_session_id: root.id,
                target_session_id: survivor_id,
                kind: AgentMessageKind::Progress,
                body: "Surviving child status".to_owned(),
            },
        )
        .unwrap();

    let deleted_run_id = RunId::new();
    let survivor_run_id = RunId::new();
    let deleted_message = "deleted session transcript content ".repeat(20);
    let survivor_message = "survivor session transcript content ".repeat(20);
    let deleted_content_hash = Sha256::digest(deleted_message.as_bytes()).to_vec();
    let survivor_content_hash = Sha256::digest(survivor_message.as_bytes()).to_vec();
    let checkpoint_text = "deleted checkpoint file content ".repeat(20);
    let checkpoint_content_hash = Sha256::digest(checkpoint_text.as_bytes()).to_vec();
    let run_summaries = BTreeMap::from([
        (
            deleted_run_id,
            DurableRunSummary {
                snapshot: AgentRunSnapshot {
                    id: deleted_run_id,
                    attempt_id: loom_core::RunAttemptId::new(),
                    control_revision: 0,
                    session_id: deleted_id,
                    task: "delete me".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    state: AgentRunState::Completed,
                    started_at: Timestamp::from_unix_millis(1_000),
                    updated_at: Timestamp::from_unix_millis(2_000),
                    completed_at: Some(Timestamp::from_unix_millis(2_000)),
                    summary: Some("finished".to_owned()),
                    evidence: Vec::new(),
                },
                usage: UsageSnapshot::default(),
                attempts: None,
                execution_state: None,
                interactions: None,
            },
        ),
        (
            survivor_run_id,
            DurableRunSummary {
                snapshot: AgentRunSnapshot {
                    id: survivor_run_id,
                    attempt_id: loom_core::RunAttemptId::new(),
                    control_revision: 0,
                    session_id: survivor_id,
                    task: "keep me".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    state: AgentRunState::Completed,
                    started_at: Timestamp::from_unix_millis(1_000),
                    updated_at: Timestamp::from_unix_millis(2_000),
                    completed_at: Some(Timestamp::from_unix_millis(2_000)),
                    summary: None,
                    evidence: Vec::new(),
                },
                usage: UsageSnapshot::default(),
                attempts: None,
                execution_state: None,
                interactions: None,
            },
        ),
    ]);
    let run_messages = BTreeMap::from([
        (
            deleted_run_id,
            vec![DurableRunMessage {
                timeline_ordinal: 0,
                role: loom_model::MessageRole::User,
                content: deleted_message.clone(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            }],
        ),
        (
            survivor_run_id,
            vec![DurableRunMessage {
                timeline_ordinal: 0,
                role: loom_model::MessageRole::User,
                content: survivor_message.clone(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            }],
        ),
    ]);
    let checkpoint_id = CheckpointId::new();
    let filesystem_records = [DurableFilesystemRecord {
        session_id: deleted_id,
        root: "/tmp/loom-deleted-session".to_owned(),
        control: WorkspaceControl::Agent,
        checkpoints: vec![Checkpoint {
            id: checkpoint_id,
            session_id: deleted_id,
            label: "rollback point".to_owned(),
            created_at: Timestamp::from_unix_millis(1_500),
            files: BTreeMap::from([(
                "src/main.rs".to_owned(),
                CheckpointFile {
                    existed: true,
                    content: checkpoint_text.clone(),
                    revision: "revision-a".to_owned(),
                    expected_revision: "revision-b".to_owned(),
                },
            )]),
        }],
        edits: Vec::new(),
        changes: Vec::new(),
        repositories: BTreeMap::new(),
        directories: Vec::new(),
        payload: serde_json::json!({
            "filesystem": {
                "session_id": deleted_id,
                "root": "/tmp/loom-deleted-session",
                "control": "agent",
                "checkpoints": [],
                "edits": [],
                "next_sequence": 0,
                "changes": []
            }
        }),
        delta: None,
    }];
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(4),
        retention_limit: 250,
        events: vec![
            ServerEventEnvelope {
                protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(1),
                session_id: deleted_id,
                event: loom_model::ServerEvent::AgentSessionArchived {
                    session_id: deleted_id,
                },
            },
            ServerEventEnvelope {
                protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(2),
                session_id: survivor_id,
                event: loom_model::ServerEvent::AgentSessionRenamed {
                    session_id: survivor_id,
                    name: "Surviving child".to_owned(),
                },
            },
        ],
        workspace_events: vec![
            WorkspaceEventEnvelope {
                protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(3),
                workspace_id,
                event: loom_model::WorkspaceEvent::Renamed {
                    name: "workspace".to_owned(),
                },
            },
            WorkspaceEventEnvelope {
                protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(4),
                workspace_id,
                event: loom_model::WorkspaceEvent::ConfigChanged { revision: 1 },
            },
        ],
    };
    let mut stored_state = manager.export_state();
    stored_state.sessions.insert(survivor_id, survivor_snapshot);
    stored_state.sessions.insert(deleted_id, deleted_snapshot);
    stored_state
        .sessions
        .insert(grandchild_id, grandchild_snapshot);
    let settings = DurableSessionSettings {
        approval_policies: BTreeMap::from([
            (deleted_id, ApprovalPolicy::default()),
            (survivor_id, ApprovalPolicy::default()),
        ]),
        auto_approve_actions: BTreeMap::from([(deleted_id, true), (survivor_id, false)]),
    };
    persistence
        .save_state(DurableStateWrite {
            sessions: &stored_state,
            workspaces: None,
            settings: Some(&settings),
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
            filesystem_records: Some(&filesystem_records),
            feed: Some(&feed),
        })
        .unwrap();

    // The current writer only stores workspace-only events in `workspace_feed_events`,
    // but an older or migrated row can still carry a session envelope. The
    // deletion decodes the affected workspaces to find those rows.
    let legacy_workspace_row =
        serde_json::to_vec(&WorkspaceFeedEvent::Session(ServerEventEnvelope {
            protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(5),
            session_id: deleted_id,
            event: loom_model::ServerEvent::AgentSessionArchived {
                session_id: deleted_id,
            },
        }))
        .unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspace_feed_events(sequence, workspace_id, payload_codec, payload)
             VALUES (5, ?1, 0, ?2)",
            params![
                workspace_id.as_uuid().as_bytes().as_slice(),
                legacy_workspace_row
            ],
        )
        .unwrap();
    drop(connection);

    // An empty id set is a no-op.
    let connection = Connection::open(&path).unwrap();
    assert_eq!(table_count(&connection, "sessions"), 4);
    drop(connection);
    assert_eq!(persistence.delete_sessions(&BTreeSet::new()).unwrap(), 0);
    let connection = Connection::open(&path).unwrap();
    assert_eq!(table_count(&connection, "sessions"), 4);
    drop(connection);

    assert_eq!(
        persistence
            .delete_sessions(&BTreeSet::from([deleted_id]))
            .unwrap(),
        1
    );

    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        session_row_count(&connection, "sessions", "id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(&connection, "sessions", "id", survivor_id),
        1
    );
    assert_eq!(
        session_row_count(&connection, "sessions_hierarchy", "session_id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(
            &connection,
            "sessions_hierarchy",
            "session_id",
            grandchild_id
        ),
        1
    );
    assert_eq!(
        session_row_count(&connection, "run_summaries", "session_id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(&connection, "run_messages", "session_id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(&connection, "run_summaries", "session_id", survivor_id),
        1
    );
    assert_eq!(
        session_row_count(&connection, "run_messages", "session_id", survivor_id),
        1
    );
    assert_eq!(
        session_row_count(&connection, "session_filesystems", "session_id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(
            &connection,
            "filesystem_change_state",
            "session_id",
            deleted_id
        ),
        0
    );
    assert_eq!(
        session_row_count(&connection, "checkpoints", "session_id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(&connection, "checkpoint_files", "session_id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(&connection, "session_settings", "session_id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(&connection, "session_settings", "session_id", survivor_id),
        1
    );
    assert_eq!(
        session_row_count(
            &connection,
            "delegated_tasks",
            "target_session_id",
            deleted_id
        ),
        0
    );
    assert_eq!(
        session_row_count(
            &connection,
            "delegated_tasks",
            "target_session_id",
            survivor_id
        ),
        1
    );
    assert_eq!(
        session_row_count(
            &connection,
            "delegated_tasks",
            "target_session_id",
            grandchild_id
        ),
        1
    );
    // Both dependency edges go: the deleted session's own edge and the edge of
    // the surviving task that pointed at the deleted task.
    assert_eq!(table_count(&connection, "delegated_task_dependencies"), 0);
    assert_eq!(
        session_row_count(
            &connection,
            "project_worktrees",
            "child_session_id",
            deleted_id
        ),
        0
    );
    assert_eq!(
        task_row_count(
            &connection,
            "project_worktree_conflict_paths",
            "task_id",
            created_task.task_id
        ),
        0
    );
    assert_eq!(table_count(&connection, "project_agent_messages"), 1);
    let remaining_message_id: Vec<u8> = connection
        .query_row(
            "SELECT message_id FROM project_agent_messages WHERE task_id=?1",
            [survivor_task.task_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        remaining_message_id,
        surviving_message.message_id.as_uuid().as_bytes().to_vec()
    );
    assert_eq!(
        session_row_count(&connection, "feed_events", "session_id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(&connection, "feed_events", "session_id", survivor_id),
        1
    );
    assert_eq!(
        session_row_count(&connection, "feed_session_meta", "session_id", deleted_id),
        0
    );
    assert_eq!(
        session_row_count(&connection, "feed_session_meta", "session_id", survivor_id),
        1
    );
    let workspace_feed_sequences: Vec<i64> = connection
        .prepare("SELECT sequence FROM workspace_feed_events ORDER BY sequence")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(workspace_feed_sequences, vec![3, 4]);
    // Content reachable only from the deleted session is reclaimed in the same
    // call, with no queued candidate left behind, while the surviving session's
    // content stays.
    assert!(!content_object_exists(&connection, &deleted_content_hash));
    assert!(!content_object_exists(
        &connection,
        &checkpoint_content_hash
    ));
    assert_eq!(
        queued_content_candidates(&connection, &deleted_content_hash),
        0
    );
    assert_eq!(
        queued_content_candidates(&connection, &checkpoint_content_hash),
        0
    );
    assert!(content_object_exists(&connection, &survivor_content_hash));
    assert_eq!(
        queued_content_candidates(&connection, &survivor_content_hash),
        0
    );
    drop(connection);

    let project = persistence
        .load_project_snapshot(project_id)
        .unwrap()
        .unwrap();
    assert_eq!(project.tasks.len(), 2);
    assert!(project.worktrees.is_empty());
    assert!(
        persistence
            .load_delegated_task(created_task.task_id)
            .unwrap()
            .is_none()
    );
    assert!(
        persistence
            .load_delegated_task(grandchild_task.task_id)
            .unwrap()
            .is_some()
    );

    drop(persistence);
    fs::remove_file(path).unwrap();
}

#[test]
fn deleting_unknown_sessions_reports_no_rows() {
    let path = std::env::temp_dir().join(format!("loom-persistence-delete-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    manager
        .create_in_workspace(WorkspaceId::new(), "Untouched")
        .unwrap();
    persistence
        .save_state_with_sessions(&manager.export_state())
        .unwrap();

    assert_eq!(
        persistence
            .delete_sessions(&BTreeSet::from([AgentSessionId::new()]))
            .unwrap(),
        0
    );

    drop(persistence);
    fs::remove_file(path).unwrap();
}

#[test]
fn session_deletion_rolls_back_when_a_workspace_feed_row_is_corrupt() {
    let path = std::env::temp_dir().join(format!("loom-persistence-delete-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let workspace_id = WorkspaceId::new();
    let mut manager = SessionManager::default();
    let (session, _) = manager
        .create_in_workspace(workspace_id, "Corrupt feed")
        .unwrap();
    let feed = DurableFeedState {
        next_sequence: EventSequence::new(1),
        retention_limit: 250,
        events: vec![ServerEventEnvelope {
            protocol_version: loom_core::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(1),
            session_id: session.id,
            event: loom_model::ServerEvent::AgentSessionArchived {
                session_id: session.id,
            },
        }],
        workspace_events: Vec::new(),
    };
    persistence
        .save_state_with_sessions_and_feed(&manager.export_state(), Some(&feed))
        .unwrap();
    // The deletion decodes the workspace feed of the affected workspace, so a
    // corrupt row has to abort the whole transaction instead of leaving the
    // session half-deleted.
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO workspace_feed_events(sequence, workspace_id, payload_codec, payload)
             VALUES (2, ?1, 0, ?2)",
            params![
                workspace_id.as_uuid().as_bytes().as_slice(),
                b"not a feed event".to_vec()
            ],
        )
        .unwrap();
    drop(connection);

    let error = persistence
        .delete_sessions(&BTreeSet::from([session.id]))
        .unwrap_err();
    assert_eq!(error.code, loom_core::ErrorCode::MalformedPayload);

    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        session_row_count(&connection, "sessions", "id", session.id),
        1
    );
    assert_eq!(
        session_row_count(&connection, "feed_events", "session_id", session.id),
        1
    );
    assert_eq!(table_count(&connection, "workspace_feed_events"), 1);
    drop(connection);

    drop(persistence);
    fs::remove_file(path).unwrap();
}

#[test]
fn session_deletion_reaches_the_store_through_the_persistence_trait_object() {
    let path = std::env::temp_dir().join(format!("loom-persistence-delete-{}.db", Uuid::new_v4()));
    let store = Arc::new(FilePersistence::open(&path).unwrap());
    let persistence: Arc<dyn Persistence> = store.clone();
    let workspace_id = WorkspaceId::new();
    let mut manager = SessionManager::default();
    let (survivor, _) = manager
        .create_in_workspace(workspace_id, "Survivor")
        .unwrap();
    let (first, _) = manager
        .create_in_workspace(workspace_id, "First deleted")
        .unwrap();
    let (second, _) = manager
        .create_in_workspace(workspace_id, "Second deleted")
        .unwrap();
    manager.archive(first.id).unwrap();
    manager.archive(second.id).unwrap();
    // The server holds `Arc<dyn Persistence>` and clones the store behind an
    // `Arc`, so deletion has to work through both handles.
    persistence
        .save_state(DurableStateWrite {
            sessions: &manager.export_state(),
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: None,
            run_runtime_configs: None,
            run_context_checkpoints: None,
            run_plans: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
        .unwrap();

    assert_eq!(
        store.delete_sessions(&BTreeSet::from([first.id])).unwrap(),
        1
    );
    assert_eq!(
        persistence
            .delete_sessions(&BTreeSet::from([second.id]))
            .unwrap(),
        1
    );

    let sessions = persistence.load_sessions().unwrap().unwrap().sessions;
    assert!(sessions.contains_key(&survivor.id));
    assert!(!sessions.contains_key(&first.id));
    assert!(!sessions.contains_key(&second.id));

    drop(persistence);
    drop(store);
    fs::remove_file(path).unwrap();
}
