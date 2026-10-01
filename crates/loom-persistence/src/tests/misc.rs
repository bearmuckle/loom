//! Persistence tests: misc.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn filesystem_change_writes_retain_only_the_newest_entries() {
    let session_id = AgentSessionId::new();
    let changes = (1..=(MAX_FILESYSTEM_CHANGE_HISTORY as u64 + 1))
        .map(|sequence| SessionFilesystemChange {
            sequence: EventSequence::new(sequence),
            session_id,
            path: format!("file-{sequence}"),
            kind: WorkspaceChangeKind::Created,
            revision: None,
        })
        .collect::<Vec<_>>();
    let mut connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE filesystem_changes(
                session_id BLOB NOT NULL, sequence INTEGER NOT NULL,
                path TEXT NOT NULL, kind TEXT NOT NULL, revision TEXT,
                PRIMARY KEY(session_id, sequence)
            ) WITHOUT ROWID;",
        )
        .unwrap();
    let transaction = connection.transaction().unwrap();
    save_filesystem_change_rows(&transaction, session_id, &changes).unwrap();
    let (count, min_sequence, max_sequence): (i64, i64, i64) = transaction
        .query_row(
            "SELECT COUNT(*), MIN(sequence), MAX(sequence) FROM filesystem_changes",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(count, MAX_FILESYSTEM_CHANGE_HISTORY as i64);
    assert_eq!(min_sequence, 2);
    assert_eq!(max_sequence, MAX_FILESYSTEM_CHANGE_HISTORY as i64 + 1);
}

#[test]
fn persistence_trait_object_forwards_read_paths_on_an_empty_store() {
    let store: std::sync::Arc<dyn Persistence> = std::sync::Arc::new(FilePersistence::in_memory());
    let _ = store.path();
    let _ = store.load_sessions();
    let _ = store.load_workspaces();
    let _ = store.load_session_settings();
    let _ = store.load_workspace_configs();
    let _ = store.load_provider_configs();
    let _ = store.load_provider_health();
    let _ = store.load_provider_usage();
    let _ = store.load_idempotency_records();
    let _ = store.list_filesystem_sessions();
    let _ = store.list_pending_project_cancellation_cascades();
    let _ = store.list_project_manager_waits_by_child(TaskId::new());
    let _ = store.list_unfinished_project_manager_waits();
    let _ = store.list_project_tasks(ProjectId::new());
    let _ = store.list_agent_messages(ProjectId::new(), AgentSessionId::new(), 0, 10);
    let _ = store.has_pending_project_cancellation_cascade(ProjectId::new());
    let _ = store.load_active_run_summaries();
    let _ = store.load_delegated_task(TaskId::new());
    let _ = store.load_delegated_task_for_target(AgentSessionId::new());
    let _ = store.load_project_child_by_request(
        RequestId::new(),
        ProjectId::new(),
        AgentSessionId::new(),
        "child",
        &DelegatedTaskSpec {
            intent: "inspect".to_owned(),
            model_id: "deterministic/demo".to_owned(),
            context_references: Vec::new(),
            dependencies: Vec::new(),
            code_change: false,
            permissions: ProjectAgentPermissions::default(),
        },
    );
    let _ = store.load_project_manager_wait(ProjectManagerWaitId::new());
    let _ = store.load_project_snapshot(ProjectId::new());
    let _ = store.load_project_snapshot_for_session(AgentSessionId::new());
    let _ = store.load_project_worktree_by_task(TaskId::new());
    let _ = store.load_latest_run_summary_for_session(AgentSessionId::new());
    let _ = store.load_run_activities(RunId::new());
    let _ = store.load_run_attempts(RunId::new());
    let _ = store.load_run_context_checkpoint(RunId::new());
    let _ = store.load_run_execution_state(RunId::new());
    let _ = store.load_run_interactions(RunId::new());
    let _ = store.load_run_message_content_range(RunId::new(), 0, 0, 4);
    let _ = store.load_run_messages(RunId::new());
    let _ = store.load_run_plan(RunId::new());
    let _ = store.load_run_runtime_config(RunId::new());
    let _ = store.load_run_summary(RunId::new());
    let _ = store.load_session_projection_read(AgentSessionId::new());
    let _ = store.load_session_usage(AgentSessionId::new(), &BTreeSet::new());
    let _ = store.load_feed_header();
    let _ = store.load_feed_events_since(None, None);
    let _ = store.load_feed_session_cursor(AgentSessionId::new());
    let _ = store.load_feed_workspace_cursor(WorkspaceId::new());
    let _ = store.load_feed_workspace_events_since(WorkspaceId::new(), None);
    let _ = store.load_recent_feed_events(AgentSessionId::new(), 1);
    let _ = store.load_filesystem_changes_page(AgentSessionId::new(), None, 1);
    let _ = store.load_filesystem_record(AgentSessionId::new());
    let _ = store.load_run_message_page(RunId::new(), None, 10);
    let _ = store.next_run_message_fragment_position(RunId::new(), 0);
    let _ = store.prune_expired_idempotency_records(Timestamp::now());
    let _ = store.load_agent_message_by_request(RequestId::new());
    let _ = store.release_exclusive_writer();
}

#[test]
fn action_only_results_are_not_duplicated_onto_attempts() {
    let call = |name: &str| loom_model::ToolCall {
        id: loom_core::ToolCallId::new(),
        name: name.to_owned(),
        arguments: serde_json::Value::Null,
    };

    let read = ToolResult::success(&call("read_file"), "fn main() {}".to_owned());
    let stored = compact_stored_result(Some(read)).unwrap();
    assert!(stored.success);
    assert_eq!(stored.name, "read_file");
    assert!(
        stored.output.is_empty(),
        "file contents must not be stored a second time on the attempt"
    );

    let command = ToolResult::success(&call("run_command"), "stdout: ok".to_owned());
    let stored = compact_stored_result(Some(command)).unwrap();
    assert!(
        stored.output.is_empty(),
        "a successful command keeps its row, not its stdout"
    );

    // Only a successful result that exists nowhere else is kept.
    let search = ToolResult::success(&call("web_search"), r#"{"results":[]}"#.to_owned());
    assert_eq!(
        compact_stored_result(Some(search.clone())).unwrap(),
        search,
        "a successful result that exists nowhere else is kept in full"
    );

    let failure = ToolResult::failure(&call("run_command"), "exit status: 1");
    let stored = compact_stored_result(Some(failure)).unwrap();
    assert!(!stored.success);
    assert!(stored.output.is_empty());

    let content_failure = ToolResult::failure(&call("web_search"), "request failed");
    let stored = compact_stored_result(Some(content_failure)).unwrap();
    assert!(
        stored.output.is_empty(),
        "a failure is trimmed too; the agent reports whether it matters"
    );

    assert!(compact_stored_result(None).is_none());
}
