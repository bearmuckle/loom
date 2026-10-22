//! Persistence tests: project.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn session_projection_reader_keeps_run_and_cursor_on_one_sqlite_snapshot() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let store = FilePersistence::open(&path).unwrap();
    store
        .save_state_with_sessions(&SessionManager::default().export_state())
        .unwrap();

    let session_id = AgentSessionId::new();
    let workspace_id = WorkspaceId::new();
    let run_id = RunId::new();
    let attempt_id = RunAttemptId::new();
    {
        let setup = Connection::open(&path).unwrap();
        setup
            .execute(
                "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
             VALUES (?1, ?2, 'snapshot', 'idle', 1, 1)",
                params![
                    session_id.as_uuid().as_bytes().as_slice(),
                    workspace_id.as_uuid().as_bytes().as_slice()
                ],
            )
            .unwrap();
        setup
            .execute(
                "INSERT INTO run_summaries(
                run_id, session_id, attempt_id, control_revision, state, started_at,
                updated_at, completed_at, task, model, summary, input_tokens,
                output_tokens, cached_input_tokens, tool_calls, cost_micros, elapsed_ms
             ) VALUES (?1, ?2, ?3, 1, 'executing', 1, 1, NULL, 'before', 'model', NULL,
                       0, 0, 0, 0, 0, 0)",
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    session_id.as_uuid().as_bytes().as_slice(),
                    attempt_id.as_uuid().as_bytes().as_slice()
                ],
            )
            .unwrap();
        setup.execute(
            "INSERT INTO feed_session_meta(session_id, first_sequence, latest_sequence, pruned_through)
             VALUES (?1, 1, 1, 0)",
            [session_id.as_uuid().as_bytes().as_slice()],
        ).unwrap();
    }

    let read = store
        .load_session_projection_read_between(session_id, || {
            let writer = Connection::open(&path).unwrap();
            let transaction = writer.unchecked_transaction().unwrap();
            transaction
                .execute(
                    "UPDATE run_summaries SET task='after', updated_at=2 WHERE run_id=?1",
                    [run_id.as_uuid().as_bytes().as_slice()],
                )
                .unwrap();
            transaction
                .execute(
                    "UPDATE feed_session_meta SET latest_sequence=2 WHERE session_id=?1",
                    [session_id.as_uuid().as_bytes().as_slice()],
                )
                .unwrap();
            transaction.commit().unwrap();
            Ok(())
        })
        .unwrap();
    assert_eq!(read.latest_run.as_ref().unwrap().snapshot.task, "before");
    assert_eq!(read.latest_sequence, Some(EventSequence::new(1)));

    let fresh = store.load_session_projection_read(session_id).unwrap();
    assert_eq!(fresh.latest_run.as_ref().unwrap().snapshot.task, "after");
    assert_eq!(fresh.latest_sequence, Some(EventSequence::new(2)));
    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn project_grants_json_is_forward_and_backward_compatible() {
    // Missing keys default to disabled and unknown keys are ignored, so a
    // future grant is added without a schema migration.
    let grants: RunProjectGrants = decode_json(
        r#"{"delegation":true,"future_grant":true}"#,
        "run project grants",
    )
    .unwrap();
    let mut config = DurableRunRuntimeConfig {
        system_instructions: None,
        repository_instructions: None,
        approval_policy: ApprovalPolicy::default(),
        limits: SessionLimits::default(),
        context_options: ContextAssemblyOptions::default(),
        checkpoint_id: None,
        input_cost_micros_per_1k: 0,
        output_cost_micros_per_1k: 0,
        context_inspection: None,
        project_delegation_enabled: false,
        project_messaging_enabled: true,
        project_inspection_enabled: false,
        project_child_control_enabled: false,
        project_worktree_enabled: false,
        project_review_enabled: false,
        project_integration_enabled: false,
        project_branch_messaging_enabled: false,
    };
    grants.apply(&mut config);
    assert!(config.project_delegation_enabled);
    assert!(!config.project_messaging_enabled);
    assert_eq!(
        RunProjectGrants::of(&config),
        RunProjectGrants {
            delegation: true,
            ..RunProjectGrants::default()
        }
    );

    // Round trip through the stored representation.
    let payload = serde_json::to_string(&RunProjectGrants::of(&config)).unwrap();
    let restored: RunProjectGrants = decode_json(&payload, "run project grants").unwrap();
    assert!(restored.delegation);
    assert!(!restored.messaging);
}

#[test]
fn project_hierarchy_constraints_reject_invalid_depth_and_cross_project_parent() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (first, _) = manager
        .create_in_workspace(WorkspaceId::new(), "First project")
        .unwrap();
    let (second, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Second project")
        .unwrap();
    persistence
        .save_state_with_sessions(&manager.export_state())
        .unwrap();

    let connection = Connection::open(&path).unwrap();
    let child = Uuid::new_v4();
    let cross_project = connection.execute(
        "INSERT INTO sessions_hierarchy(project_id, session_id, parent_session_id, depth)
         VALUES (?1, ?2, ?3, 2)",
        params![
            first.id.as_uuid().as_bytes().as_slice(),
            child.as_bytes().as_slice(),
            second.id.as_uuid().as_bytes().as_slice()
        ],
    );
    assert!(cross_project.is_err());

    let too_deep = connection.execute(
        "INSERT INTO sessions_hierarchy(project_id, session_id, parent_session_id, depth)
         VALUES (?1, ?2, ?3, 4)",
        params![
            first.id.as_uuid().as_bytes().as_slice(),
            child.as_bytes().as_slice(),
            first.id.as_uuid().as_bytes().as_slice()
        ],
    );
    assert!(too_deep.is_err());
    drop(connection);
    drop(persistence);
    fs::remove_file(path).unwrap();
}

#[test]
fn sub_projects_record_their_parent_project_and_refuse_other_parents() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (parent, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Parent project")
        .unwrap();
    persistence
        .save_state_with_sessions(&manager.export_state())
        .unwrap();
    let parent_project_id = ProjectId::from_uuid(*parent.id.as_uuid());

    let sub_project = AgentSessionSnapshot {
        id: AgentSessionId::new(),
        workspace_id: parent.workspace_id,
        name: "Branch out".to_owned(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        title_source: loom_core::SessionTitleSource::Auto,
    };
    persistence
        .create_sub_project(&sub_project, parent_project_id, EventSequence::new(2))
        .unwrap();

    // The sub project is a project of its own whose snapshot names the project
    // it branched out of, while the parent keeps no link of its own.
    let snapshot = persistence
        .load_project_snapshot(ProjectId::from_uuid(*sub_project.id.as_uuid()))
        .unwrap()
        .expect("the sub project is durable");
    assert_eq!(snapshot.root_session_id, sub_project.id);
    assert_eq!(snapshot.parent_project_id, Some(parent_project_id));
    assert_eq!(snapshot.agents.len(), 1);
    assert_eq!(snapshot.agents[0].depth, 1);
    let parent_snapshot = persistence
        .load_project_snapshot(parent_project_id)
        .unwrap()
        .expect("the parent project is durable");
    assert_eq!(parent_snapshot.parent_project_id, None);
    // Looking the branch up by its session finds the same project.
    assert_eq!(
        persistence
            .load_project_snapshot_for_session(sub_project.id)
            .unwrap()
            .expect("the sub project is reachable by session"),
        snapshot
    );

    // An existing session that is not a project root of the same workspace
    // cannot be a parent, and a project cannot be its own parent.
    let unrelated = AgentSessionId::new();
    let error = persistence
        .create_sub_project(
            &AgentSessionSnapshot {
                id: unrelated,
                workspace_id: parent.workspace_id,
                name: "Unrelated".to_owned(),
                ..sub_project.clone()
            },
            ProjectId::new(),
            EventSequence::new(3),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    let error = persistence
        .create_sub_project(
            &AgentSessionSnapshot {
                id: sub_project.id,
                ..sub_project.clone()
            },
            ProjectId::from_uuid(*sub_project.id.as_uuid()),
            EventSequence::new(3),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);

    // A sub project is a project of its own, so it can be the parent of a
    // further sub project.
    let nested = AgentSessionSnapshot {
        id: AgentSessionId::new(),
        name: "Branch out again".to_owned(),
        ..sub_project.clone()
    };
    persistence
        .create_sub_project(
            &nested,
            ProjectId::from_uuid(*sub_project.id.as_uuid()),
            EventSequence::new(3),
        )
        .unwrap();
    assert_eq!(
        persistence
            .load_project_snapshot(ProjectId::from_uuid(*nested.id.as_uuid()))
            .unwrap()
            .expect("the nested sub project is durable")
            .parent_project_id,
        Some(ProjectId::from_uuid(*sub_project.id.as_uuid()))
    );

    drop(persistence);
    fs::remove_file(path).unwrap();
}

#[test]
fn project_child_and_agent_messages_are_atomic_ordered_and_request_idempotent() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (root, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Project manager")
        .unwrap();
    persistence
        .save_state_with_sessions(&manager.export_state())
        .unwrap();

    let child_id = AgentSessionId::new();
    let child_snapshot = AgentSessionSnapshot {
        id: child_id,
        workspace_id: root.workspace_id,
        name: "Research agent".to_owned(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        title_source: loom_core::SessionTitleSource::Auto,
    };
    let task = DelegatedTaskRecord {
        task_id: TaskId::new(),
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        requester_session_id: root.id,
        target_session_id: child_id,
        child_name: child_snapshot.name.clone(),
        intent: "Inspect the relevant module".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: vec![TaskContextReference {
            label: "architecture".to_owned(),
            uri: "docs/architecture.md".to_owned(),
        }],
        dependencies: Vec::new(),
        code_change: true,
        permissions: ProjectAgentPermissions {
            delegation: false,
            branch_messaging: true,
            child_control: false,
            inspection: true,
            worktree_creation: true,
            review: true,
            integration: false,
        },
        status: DelegatedTaskStatus::Queued,
        created_at: child_snapshot.created_at,
        updated_at: child_snapshot.updated_at,
    };
    let parent_repository_id = RepositoryId::new();
    let child_repository_id = RepositoryId::new();
    let initial_worktree = ProjectWorktreeRecord {
        project_id: task.project_id,
        task_id: task.task_id,
        parent_session_id: root.id,
        child_session_id: child_id,
        parent_repository_id,
        child_repository_id,
        relative_path: "worktrees/task-a".to_owned(),
        worktree_name: "task-a".to_owned(),
        branch_name: "codex/task-a".to_owned(),
        base_revision: "base-sha".to_owned(),
        base_refresh: Some(ProjectWorktreeBaseRefresh::Fetched {
            remote: "origin".to_owned(),
            base_revision: "base-sha".to_owned(),
            default_branch: "main".to_owned(),
            default_branch_revision: "tip-sha".to_owned(),
            ahead: 0,
            behind: 2,
            fetched_at: child_snapshot.created_at,
        }),
        result_revision: None,
        integrated_revision: None,
        status: ProjectWorktreeStatus::Creating,
        conflict_paths: Vec::new(),
        error: None,
        cleanup_disposition: None,
        created_at: child_snapshot.created_at,
        updated_at: child_snapshot.updated_at,
    };
    let request_id = RequestId::new();
    let created = persistence
        .create_project_child_with_worktree(
            request_id,
            &child_snapshot,
            manager.export_state().next_sequence,
            &task,
            &initial_worktree,
        )
        .unwrap();
    assert_eq!(created, task);
    let worktree = ProjectWorktreeRecord {
        result_revision: Some("result-sha".to_owned()),
        integrated_revision: None,
        status: ProjectWorktreeStatus::Conflict,
        conflict_paths: vec!["src/main.rs".to_owned(), "docs/plan.md".to_owned()],
        error: Some("integration found conflicts".to_owned()),
        cleanup_disposition: Some(ProjectWorktreeCleanupDisposition::Retain),
        updated_at: Timestamp::now(),
        ..initial_worktree.clone()
    };
    persistence.save_project_worktree(&worktree).unwrap();
    assert_eq!(
        persistence
            .load_project_worktree_by_task(task.task_id)
            .unwrap(),
        Some(worktree.clone())
    );
    let project_snapshot = persistence
        .load_project_snapshot(task.project_id)
        .unwrap()
        .unwrap();
    assert_eq!(project_snapshot.worktrees, vec![worktree]);
    let child_projection = project_snapshot
        .agents
        .iter()
        .find(|agent| agent.session_id == child_id)
        .unwrap();
    assert_eq!(
        child_projection.task_summary.as_deref(),
        Some(task.intent.as_str())
    );
    assert_eq!(
        persistence.load_delegated_task(task.task_id).unwrap(),
        Some(task.clone())
    );
    assert_eq!(
        persistence.list_project_tasks(task.project_id).unwrap(),
        vec![task.clone()]
    );
    let task_spec = DelegatedTaskSpec {
        intent: task.intent.clone(),
        model_id: task.model_id.clone(),
        context_references: task.context_references.clone(),
        dependencies: task.dependencies.clone(),
        code_change: task.code_change,
        permissions: task.permissions,
    };
    assert_eq!(
        persistence
            .load_project_child_by_request(
                request_id,
                task.project_id,
                task.requester_session_id,
                &task.child_name,
                &task_spec,
            )
            .unwrap(),
        Some(task.clone())
    );
    assert!(
        persistence
            .load_project_child_by_request(
                request_id,
                task.project_id,
                task.requester_session_id,
                "different child name",
                &task_spec,
            )
            .is_err()
    );

    // Retrying the atomic code-task creation must preserve its worktree
    // intent identity even after the worktree has advanced to conflict.
    assert_eq!(
        persistence
            .create_project_child_with_worktree(
                request_id,
                &child_snapshot,
                manager.export_state().next_sequence,
                &task,
                &initial_worktree,
            )
            .unwrap(),
        task
    );
    assert!(
        persistence
            .update_delegated_task_status(
                task.task_id,
                DelegatedTaskStatus::Running,
                Timestamp::now()
            )
            .unwrap()
    );
    assert_eq!(
        persistence
            .load_delegated_task(task.task_id)
            .unwrap()
            .unwrap()
            .status,
        DelegatedTaskStatus::Running
    );
    assert!(
        !persistence
            .update_delegated_task_status_if_queued(
                task.task_id,
                DelegatedTaskStatus::Blocked,
                Timestamp::now(),
            )
            .unwrap(),
        "a scheduler claim must not overwrite a status advanced by runtime recovery"
    );

    let draft = AgentMessageDraft {
        project_id: task.project_id,
        task_id: Some(task.task_id),
        sender_session_id: root.id,
        target_session_id: child_id,
        kind: AgentMessageKind::Direction,
        body: "Start with the persistence layer".to_owned(),
    };
    let message_request = RequestId::new();
    let accepted = persistence
        .accept_agent_message(message_request, &draft)
        .unwrap();
    assert_eq!(accepted.project_sequence, 1);
    assert_eq!(accepted.kind, AgentMessageKind::Direction);
    assert_eq!(
        persistence
            .accept_agent_message(message_request, &draft)
            .unwrap(),
        accepted
    );
    let next = persistence
        .accept_agent_message(
            RequestId::new(),
            &AgentMessageDraft {
                target_session_id: root.id,
                body: "Child completed a first pass".to_owned(),
                kind: AgentMessageKind::Progress,
                ..draft.clone()
            },
        )
        .unwrap();
    assert_eq!(next.project_sequence, 2);
    assert_eq!(
        persistence
            .list_agent_messages(task.project_id, child_id, 0, 10)
            .unwrap(),
        vec![accepted]
    );

    let mismatch = AgentMessageDraft {
        body: "changed payload".to_owned(),
        ..draft
    };
    assert!(
        persistence
            .accept_agent_message(message_request, &mismatch)
            .is_err()
    );
    drop(persistence);
    fs::remove_file(path).unwrap();
}

#[test]
fn project_cancellation_cascade_intent_is_ordered_idempotent_and_removable() {
    let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (root, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Project manager")
        .unwrap();
    persistence
        .save_state_with_sessions(&manager.export_state())
        .unwrap();
    let child_id = AgentSessionId::new();
    let child_snapshot = AgentSessionSnapshot {
        id: child_id,
        workspace_id: root.workspace_id,
        name: "Research agent".to_owned(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        title_source: loom_core::SessionTitleSource::Auto,
    };
    let task = DelegatedTaskRecord {
        task_id: TaskId::new(),
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        requester_session_id: root.id,
        target_session_id: child_id,
        child_name: child_snapshot.name.clone(),
        intent: "Inspect the relevant module".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: false,
        permissions: ProjectAgentPermissions::default(),
        status: DelegatedTaskStatus::Queued,
        created_at: child_snapshot.created_at,
        updated_at: child_snapshot.updated_at,
    };
    persistence
        .create_project_child(
            RequestId::new(),
            &child_snapshot,
            manager.export_state().next_sequence,
            &task,
        )
        .unwrap();
    let cascade = ProjectCancellationCascadeRecord {
        project_id: task.project_id,
        root_task_id: task.task_id,
        manager_session_id: root.id,
        members: vec![(task.task_id, child_id)],
        created_at: Timestamp::now(),
    };
    assert_eq!(
        persistence
            .begin_project_cancellation_cascade(&cascade)
            .unwrap(),
        cascade
    );
    assert!(
        persistence
            .has_pending_project_cancellation_cascade(task.project_id)
            .unwrap()
    );
    assert_eq!(
        persistence
            .list_pending_project_cancellation_cascades()
            .unwrap(),
        vec![cascade.clone()]
    );
    assert_eq!(
        persistence
            .begin_project_cancellation_cascade(&cascade)
            .unwrap(),
        cascade
    );
    let invalid = ProjectCancellationCascadeRecord {
        members: Vec::new(),
        ..cascade.clone()
    };
    assert!(
        persistence
            .begin_project_cancellation_cascade(&invalid)
            .is_err()
    );
    assert!(
        persistence
            .complete_project_cancellation_cascade(task.project_id, task.task_id)
            .unwrap()
    );
    assert!(
        !persistence
            .has_pending_project_cancellation_cascade(task.project_id)
            .unwrap()
    );
    assert!(
        persistence
            .list_pending_project_cancellation_cascades()
            .unwrap()
            .is_empty()
    );
    assert!(
        !persistence
            .complete_project_cancellation_cascade(task.project_id, task.task_id)
            .unwrap()
    );
    drop(persistence);
    fs::remove_file(path).unwrap();
}

#[test]
fn project_task_queries_and_cancellation_cover_lifecycle_paths() {
    let path = std::env::temp_dir().join(format!("loom-project-{}.db", Uuid::new_v4()));
    let persistence = FilePersistence::open(&path).unwrap();
    let mut manager = SessionManager::default();
    let (root, _) = manager
        .create_in_workspace(WorkspaceId::new(), "Project manager")
        .unwrap();
    persistence
        .save_state_with_sessions(&manager.export_state())
        .unwrap();

    let child_id = AgentSessionId::new();
    let now = Timestamp::now();
    let child_snapshot = AgentSessionSnapshot {
        id: child_id,
        workspace_id: root.workspace_id,
        name: "Research agent".to_owned(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at: now,
        updated_at: now,
        title_source: loom_core::SessionTitleSource::Auto,
    };
    let task = DelegatedTaskRecord {
        task_id: TaskId::new(),
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        requester_session_id: root.id,
        target_session_id: child_id,
        child_name: child_snapshot.name.clone(),
        intent: "Inspect the relevant module".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: vec![TaskContextReference {
            label: "architecture".to_owned(),
            uri: "docs/architecture.md".to_owned(),
        }],
        dependencies: Vec::new(),
        code_change: false,
        permissions: ProjectAgentPermissions {
            delegation: false,
            branch_messaging: true,
            child_control: false,
            inspection: true,
            worktree_creation: true,
            review: true,
            integration: false,
        },
        status: DelegatedTaskStatus::Queued,
        created_at: now,
        updated_at: now,
    };
    let request_id = RequestId::new();
    let created = persistence
        .create_project_child(
            request_id,
            &child_snapshot,
            manager.export_state().next_sequence,
            &task,
        )
        .unwrap();
    assert_eq!(created, task);

    let spec = DelegatedTaskSpec {
        intent: task.intent.clone(),
        model_id: task.model_id.clone(),
        context_references: task.context_references.clone(),
        dependencies: Vec::new(),
        code_change: false,
        permissions: task.permissions,
    };
    assert_eq!(
        persistence
            .load_project_child_by_request(
                request_id,
                task.project_id,
                root.id,
                &child_snapshot.name,
                &spec,
            )
            .unwrap(),
        Some(task.clone())
    );
    assert_eq!(
        persistence.list_project_tasks(task.project_id).unwrap(),
        vec![task.clone()]
    );
    assert_eq!(
        persistence
            .load_delegated_task_for_target(child_id)
            .unwrap(),
        Some(task.clone())
    );
    assert!(
        persistence
            .update_delegated_task_status(
                task.task_id,
                DelegatedTaskStatus::Running,
                Timestamp::now(),
            )
            .unwrap()
    );
    assert!(
        !persistence
            .update_delegated_task_status_if_queued(
                task.task_id,
                DelegatedTaskStatus::Cancelled,
                Timestamp::now(),
            )
            .unwrap()
    );
    assert_eq!(
        persistence
            .load_project_snapshot_for_session(child_id)
            .unwrap()
            .unwrap()
            .root_session_id,
        root.id
    );

    let draft = AgentMessageDraft {
        project_id: task.project_id,
        task_id: Some(task.task_id),
        sender_session_id: root.id,
        target_session_id: child_id,
        kind: AgentMessageKind::Direction,
        body: "Continue with the next step".to_owned(),
    };
    let message_request = RequestId::new();
    let message = persistence
        .accept_agent_message(message_request, &draft)
        .unwrap();
    assert_eq!(
        persistence
            .load_agent_message_by_request(message_request)
            .unwrap()
            .map(|record| record.message_id),
        Some(message.message_id)
    );
    assert_eq!(
        persistence
            .list_agent_messages(task.project_id, child_id, 0, 10)
            .unwrap()
            .len(),
        1
    );

    let cascade = ProjectCancellationCascadeRecord {
        project_id: task.project_id,
        root_task_id: task.task_id,
        manager_session_id: root.id,
        members: vec![(task.task_id, child_id)],
        created_at: Timestamp::now(),
    };
    persistence
        .begin_project_cancellation_cascade(&cascade)
        .unwrap();
    assert!(
        persistence
            .has_pending_project_cancellation_cascade(task.project_id)
            .unwrap()
    );
    assert_eq!(
        persistence
            .list_pending_project_cancellation_cascades()
            .unwrap()
            .len(),
        1
    );
    assert!(
        persistence
            .complete_project_cancellation_cascade(task.project_id, task.task_id)
            .unwrap()
    );

    let _ = fs::remove_file(&path);
}
