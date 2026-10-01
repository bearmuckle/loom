use super::{
    LoomView, SETTINGS_SECTIONS, SessionSourceChoice, SessionSourceDialog,
    SessionSourceDialogPurpose, SettingsSection, WorkerConnectionState, WorkerNodeEntry,
};
use crate::state::GitHubLoginState;
use crate::state::InspectorTab;
use crate::state::RenameDialogState;
use crate::state::ReviewRow;
use crate::state::ThemeChoice;
use crate::state::TimelineItem;
use crate::state::{
    AssistantPart, AssistantTurn, EvidenceText, SystemNote, SystemTone, ToolPart, ToolPartStatus,
};
use gpui_kit::test::{TestAppContextExt, TestWindowExt};
use gpui_kit::{AppContext, TestAppContext, px, size};
use loom_core::CapabilitySet;
use loom_core::UsageSnapshot;
use loom_core::{ActivityId, AgentSessionId, ErrorCode, RunId, Timestamp, ToolCallId};
use loom_model::ProviderUsageSummary;
use loom_model::{
    ModelCapabilities, ModelDescriptor, ModelId, ProviderHealth, ProviderKind, ProviderSummary,
    ToolCall,
};
use loom_protocol::ToolResult;
use loom_protocol::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, ClientRequest,
    ClonedRepository, ContextBudget, ContextInspection, ContextItem, ContextItemKind,
    ContextSummary, EventsResponse, FileActivityOperation, FilesystemRequest, FilesystemResponse,
    GitDiff, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitFileStatus, GitFileStatusKind,
    GitHubRepository, GitRepositoryStatus, ProviderRequest, RequestEnvelope, RunRequest,
    RunResponse, ServerResponse, SessionFilesystemChange, SessionFilesystemFile, SessionRepository,
    SessionRequest, SessionResponse, WorkerNodeResources, WorkerNodeStatus, WorkspaceChangeKind,
    WorkspaceEntry, WorkspaceEntryKind, WorkspaceRequest,
};
use std::collections::BTreeSet;
use std::time::Duration;

fn render_scenario(cx: &mut TestAppContext, configure: impl FnOnce(&mut LoomView)) {
    render_scenario_at(cx, size(px(1280.), px(800.)), configure);
}

fn render_scenario_at(
    cx: &mut TestAppContext,
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
    configure: impl FnOnce(&mut LoomView),
) {
    let handle = cx.open_window(window_size, |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            configure(&mut view);
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

fn nested_project_view(focus_handle: gpui_kit::FocusHandle) -> LoomView {
    use loom_core::{
        AgentSessionSnapshot, AgentSessionState, DelegatedTaskRecord, DelegatedTaskStatus,
        ProjectAgentPermissions, ProjectAgentRecord, ProjectSnapshot, ProjectWorktreeRecord,
        ProjectWorktreeStatus, RepositoryId, TaskId, WorkspaceId,
    };

    let workspace_id = WorkspaceId::new();
    let root_id = AgentSessionId::new();
    let manager_id = AgentSessionId::new();
    let worker_id = AgentSessionId::new();
    let project_id = loom_core::ProjectId::from_uuid(*root_id.as_uuid());
    let timestamp = Timestamp::from_unix_millis(1);
    let sessions = [
        (root_id, "Project", AgentSessionState::Idle),
        (manager_id, "Manager", AgentSessionState::Executing),
        (worker_id, "Worker", AgentSessionState::Completed),
    ];
    let agents = sessions
        .iter()
        .enumerate()
        .map(|(index, (session_id, _, state))| ProjectAgentRecord {
            session_id: *session_id,
            project_id,
            parent_session_id: match index {
                0 => None,
                1 => Some(root_id),
                _ => Some(manager_id),
            },
            depth: index as u8 + 1,
            state: *state,
            task_summary: None,
            output_cursor: Default::default(),
            updated_at: timestamp,
        })
        .collect();
    let manager_task_id = TaskId::new();
    let worker_task_id = TaskId::new();
    let manager_task = DelegatedTaskRecord {
        task_id: manager_task_id,
        project_id,
        requester_session_id: root_id,
        target_session_id: manager_id,
        child_name: "Manager".to_owned(),
        intent: "Coordinate the delegated work".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: false,
        permissions: ProjectAgentPermissions {
            delegation: true,
            branch_messaging: true,
            child_control: true,
            inspection: true,
            worktree_creation: true,
            review: true,
            integration: true,
        },
        status: DelegatedTaskStatus::Running,
        created_at: timestamp,
        updated_at: timestamp,
    };
    let worker_task = DelegatedTaskRecord {
        task_id: worker_task_id,
        project_id,
        requester_session_id: manager_id,
        target_session_id: worker_id,
        child_name: "Worker".to_owned(),
        intent: "Implement the requested code change".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: true,
        permissions: ProjectAgentPermissions::default(),
        status: DelegatedTaskStatus::Completed,
        created_at: timestamp,
        updated_at: timestamp,
    };
    let worker_worktree = ProjectWorktreeRecord {
        project_id,
        task_id: worker_task_id,
        parent_session_id: manager_id,
        child_session_id: worker_id,
        parent_repository_id: RepositoryId::new(),
        child_repository_id: RepositoryId::new(),
        relative_path: "worktrees/worker".to_owned(),
        worktree_name: "worker".to_owned(),
        branch_name: "agent/worker".to_owned(),
        base_revision: "parent-base".to_owned(),
        result_revision: Some("child-result".to_owned()),
        integrated_revision: None,
        status: ProjectWorktreeStatus::Ready,
        conflict_paths: Vec::new(),
        error: None,
        cleanup_disposition: None,
        created_at: timestamp,
        updated_at: timestamp,
    };
    let session_snapshots = sessions
        .iter()
        .map(|(id, name, state)| AgentSessionSnapshot {
            id: *id,
            workspace_id,
            name: (*name).to_owned(),
            state: *state,
            created_at: timestamp,
            updated_at: timestamp,
        })
        .collect::<Vec<_>>();
    let mut view = LoomView::new_for_test(focus_handle);
    view.workspace_id = workspace_id;
    view.active_session = session_snapshots[0].clone();
    for session in &session_snapshots {
        view.session_node_ids
            .insert(session.id, view.default_backend_node_id.clone());
    }
    view.sessions = session_snapshots;
    view.project_snapshot = Some(ProjectSnapshot {
        project_id,
        root_session_id: root_id,
        agents,
        tasks: vec![manager_task, worker_task],
        worktrees: vec![worker_worktree],
    });
    view
}

#[gpui_kit::test]
fn empty_session_view_renders_without_a_backend_round_trip(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |_| {});
}

#[gpui_kit::test]
fn startup_rejects_credential_bearing_remote_urls_and_missing_tokens(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let options = |remote: &str, token: Option<&str>| super::UiOptions {
            project: None,
            task: "startup validation".to_owned(),
            demo: false,
            model: ModelId::new("deterministic/demo"),
            endpoint: None,
            api_key: None,
            remote: Some(remote.to_owned()),
            token: token.map(str::to_owned),
            reset_state: false,
        };
        let error = match LoomView::try_new(
            &options("ws://user:secret@worker.example", Some("token")),
            cx.focus_handle(),
        ) {
            Err(error) => error,
            Ok(_) => panic!("credential-bearing URL was accepted"),
        };
        assert!(error.message.contains("must not contain credentials"));

        let error =
            match LoomView::try_new(&options("ws://worker.example", None), cx.focus_handle()) {
                Err(error) => error,
                Ok(_) => panic!("remote connection without a token was accepted"),
            };
        assert!(error.message.contains("require LOOM_TOKEN"));

        let error = match LoomView::try_new(
            &options("not a WebSocket URL", Some("token")),
            cx.focus_handle(),
        ) {
            Err(error) => error,
            Ok(_) => panic!("invalid remote URL was accepted"),
        };
        assert_eq!(error.code, ErrorCode::InvalidRequest);

        let invalid_workspace =
            std::env::temp_dir().join(format!("loom-ui-missing-{}", uuid::Uuid::new_v4()));
        let local_options = super::UiOptions {
            project: Some(invalid_workspace),
            task: "startup validation".to_owned(),
            demo: false,
            model: ModelId::new("deterministic/demo"),
            endpoint: None,
            api_key: None,
            remote: None,
            token: None,
            reset_state: false,
        };
        let error = match LoomView::try_new(&local_options, cx.focus_handle()) {
            Err(error) => error,
            Ok(_) => panic!("missing workspace directory was accepted"),
        };
        assert_eq!(error.code, ErrorCode::WorkspaceAccessDenied);
        LoomView::new_for_test(cx.focus_handle())
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn connection_bootstrap_creates_and_attaches_a_local_workspace_session(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let _handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let options = super::UiOptions {
            project: None,
            task: "bootstrap test".to_owned(),
            demo: false,
            model: ModelId::new("deterministic/demo"),
            endpoint: None,
            api_key: None,
            remote: None,
            token: None,
            reset_state: false,
        };
        let workspace_root =
            std::env::temp_dir().join(format!("loom-ui-bootstrap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&workspace_root).unwrap();
        let local_backend = loom_local::OwnedBackend::new();
        let connection = super::ClientConnection::InProcess(Box::new(local_backend.connect()));
        let connection_after_teardown = connection.clone();
        let negotiation = crate::connection::negotiate(&connection).unwrap();
        let mut view = LoomView::initialize_from_connection(
            &options,
            connection,
            workspace_root.clone(),
            false,
            None,
            cx.focus_handle(),
            false,
            Some(negotiation.protocol_version),
            None,
        )
        .unwrap();
        view.owned_backend = Some(local_backend);
        assert_eq!(view.workspaces.len(), 1);
        assert_eq!(view.sessions.len(), 1);
        assert!(view.models.contains(&ModelId::new("deterministic/demo")));
        assert_eq!(view.session_directories.len(), 1);
        // Avoid starting the live worker's delayed status poll in this synchronous UI test.
        view.worker_nodes.clear();
        let _ = std::fs::remove_dir_all(workspace_root);
        view.shutdown_owned_backend();
        assert!(
            connection_after_teardown
                .request(RequestEnvelope::new(ClientRequest::Workspace(
                    WorkspaceRequest::ListWorkspaces
                )))
                .result
                .is_err()
        );
        view
    });
}

#[gpui_kit::test]
fn session_list_renders_owner_and_resource_summary(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.active_session.name = "Test session".to_owned();
        view.sessions = vec![view.active_session.clone()];
        view.session_node_ids
            .insert(view.active_session.id, "test-node".to_owned());
        view.node_names
            .insert("test-node".to_owned(), "Local worker".to_owned());
        view.worker_nodes.push(WorkerNodeEntry {
            id: 0,
            status: WorkerNodeStatus {
                node_id: "test-node".to_owned(),
                name: "Local worker".to_owned(),
                online: true,
                capabilities: CapabilitySet::default(),
                resources: WorkerNodeResources {
                    cpu_count: 4,
                    cpu_usage_percent: Some(45),
                    memory_usage_percent: Some(61),
                    memory_total_bytes: Some(8 * 1024 * 1024 * 1024),
                    memory_available_bytes: Some(3 * 1024 * 1024 * 1024),
                    disk_total_bytes: Some(64 * 1024 * 1024 * 1024),
                    disk_available_bytes: Some(32 * 1024 * 1024 * 1024),
                },
            },
            is_local: true,
            url: None,
            connection: None,
            connection_state: WorkerConnectionState::Connected,
            connection_detail: None,
            severe_load_streak: 0,
        });
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(("session-tree-root", 0usize)).visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn nested_project_tree_selects_grandchild_session_by_click(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered_view = std::rc::Rc::new(std::cell::RefCell::new(None));
    let rendered_view_for_window = rendered_view.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| nested_project_view(cx.focus_handle()));
        *rendered_view_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered_view.borrow().as_ref().unwrap().clone();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        for index in 0usize..3 {
            assert!(window.find(("session-tree-root", index)).visible());
        }
        window.click(("session-tree-root", 2usize), cx);
        assert_eq!(view.read(cx).active_session.name, "Worker");
    })
    .unwrap();
}

#[gpui_kit::test]
fn phone_drawer_selects_session_and_closes_the_drawer(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered_view = std::rc::Rc::new(std::cell::RefCell::new(None));
    let rendered_view_for_window = rendered_view.clone();
    let handle = cx.open_window(size(px(390.), px(844.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            view.session_drawer_open = true;
            view
        });
        *rendered_view_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered_view.borrow().as_ref().unwrap().clone();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("mobile-session-drawer").visible());
        window.click(("session-tree-root", 2usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_session.name, "Worker");
        assert!(
            window.try_find("mobile-session-drawer").is_none(),
            "selecting a session should close the phone drawer"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn archiving_a_project_removes_descendant_sessions_from_the_tree(cx: &mut TestAppContext) {
    // Build the view without opening a window so the project poll (which
    // would run while a live project is rendered) never starts.
    let view = cx.new(|cx| {
        let mut view = nested_project_view(cx.focus_handle());
        let root_id = view.project_snapshot.as_ref().unwrap().root_session_id;
        assert_eq!(view.sessions.len(), 3);
        let removed_active = view.forget_archived_session(root_id);
        assert!(removed_active, "the active root session should be removed");
        assert!(
            view.sessions.is_empty(),
            "archived descendants must leave the session list"
        );
        assert!(view.project_snapshot.is_none());
        view
    });
    let _ = view;
}

#[gpui_kit::test]
fn archiving_a_child_removes_only_that_session(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = nested_project_view(cx.focus_handle());
        let child_id = view
            .project_snapshot
            .as_ref()
            .unwrap()
            .agents
            .iter()
            .find(|agent| agent.depth == 3)
            .expect("nested project has a depth-three agent")
            .session_id;
        let removed_active = view.forget_archived_session(child_id);
        assert!(
            !removed_active,
            "the active session is the root, not the child"
        );
        assert_eq!(view.sessions.len(), 2);
        let project = view.project_snapshot.as_ref().unwrap();
        assert!(
            project
                .agents
                .iter()
                .all(|agent| agent.session_id != child_id)
        );
        assert!(
            project
                .tasks
                .iter()
                .all(|task| task.target_session_id != child_id)
        );
        assert!(
            project
                .worktrees
                .iter()
                .all(|worktree| worktree.child_session_id != child_id)
        );
        view
    });
    let _ = view;
}

#[gpui_kit::test]
fn project_session_popup_menu_dispatches_child_control_review_and_integration(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let (control_window, control_view) = open_nested_project_menu(cx, 1);
    cx.update_window(control_window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("popup-menu").visible());
        let mut menu = window.within("popup-menu");
        assert_eq!(menu.find(2usize).label(), Some("Pause child"));
        assert_eq!(menu.find(3usize).label(), Some("Interrupt child"));
        assert_eq!(
            menu.find(4usize).label(),
            Some("Cancel child and descendants")
        );
        menu.click(2usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(control_window, |_, _, cx| {
        assert!(
            control_view
                .read(cx)
                .status_banner
                .as_ref()
                .is_some_and(|note| {
                    note.tone == SystemTone::Error
                        && note
                            .heading
                            .as_deref()
                            .is_some_and(|heading| heading.starts_with("control project child"))
                })
        );
    })
    .unwrap();

    let (review_window, review_view) = open_nested_project_menu(cx, 2);
    cx.update_window(review_window, |_, window, cx| {
        window.render_frame(cx);
        let mut menu = window.within("popup-menu");
        assert_eq!(menu.find(2usize).label(), Some("Review child changes"));
        assert_eq!(menu.find(3usize).label(), Some("Integrate child changes"));
        assert_eq!(menu.find(4usize).label(), Some("Keep child checkout"));
        assert_eq!(
            menu.find(5usize).label(),
            Some("Remove clean child checkout")
        );
        menu.click(2usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(review_window, |_, _, cx| {
        assert!(
            review_view
                .read(cx)
                .status_banner
                .as_ref()
                .is_some_and(|note| {
                    note.tone == SystemTone::Error
                        && note
                            .heading
                            .as_deref()
                            .is_some_and(|heading| heading.starts_with("review project child"))
                })
        );
    })
    .unwrap();

    let (integration_window, integration_view) = open_nested_project_menu(cx, 2);
    cx.update_window(integration_window, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(3usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(integration_window, |_, _, cx| {
        assert!(
            integration_view
                .read(cx)
                .status_banner
                .as_ref()
                .is_some_and(|note| {
                    note.tone == SystemTone::Error
                        && note.heading.as_deref().is_some_and(|heading| {
                            heading.starts_with("check child review before integration")
                        })
                })
        );
    })
    .unwrap();

    drop(control_view);
    drop(review_view);
    drop(integration_view);
    cx.quit();
    cx.run_until_parked();
}

fn open_nested_project_menu(
    cx: &mut TestAppContext,
    session_index: usize,
) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<LoomView>) {
    let rendered_view = std::rc::Rc::new(std::cell::RefCell::new(None));
    let rendered_view_for_window = rendered_view.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = nested_project_view(cx.focus_handle());
            view.project_poll_scheduled = true;
            view
        });
        let session = view.read(cx).sessions[session_index].clone();
        let project = view.read(cx).project_snapshot.clone();
        let menu_view = view.clone();
        let menu = gpui_kit::component::menu::PopupMenu::build(window, cx, move |menu, _, _| {
            LoomView::build_project_session_context_menu(menu, session, project, menu_view)
        });
        *rendered_view_for_window.borrow_mut() = Some(view);
        gpui_kit::component::Root::new(menu, window, cx)
    });
    let view = rendered_view.borrow_mut().take().unwrap();
    (handle.into(), view)
}

#[gpui_kit::test]
fn run_projection_maps_messages_plan_and_completion_evidence(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |view| {
        view.sessions = vec![view.active_session.clone()];
        view.apply_run_projection(loom_protocol::AgentRunSnapshotProjection {
            run: loom_protocol::AgentRunSnapshot {
                id: RunId::new(),
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: view.active_session.id,
                task: "inspect the repository".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Completed,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: Some(Timestamp::from_unix_millis(2)),
                summary: Some("Reviewed the project".to_owned()),
                evidence: vec![loom_core::EvidenceLink {
                    label: "readme".to_owned(),
                    uri: "file:///README.md".to_owned(),
                }],
            },
            plan: vec![loom_protocol::AgentPlanStep {
                id: "step-1".to_owned(),
                description: "Read the project files".to_owned(),
            }],
            plan_progress: loom_protocol::AgentPlanProgress {
                completed: vec![0],
                active: None,
            },
            messages: vec![
                loom_model::ModelMessage::new(loom_model::MessageRole::System, "system"),
                loom_model::ModelMessage::new(loom_model::MessageRole::User, "inspect"),
                loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, "first"),
                loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, "second"),
                loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, ""),
                loom_model::ModelMessage::new(loom_model::MessageRole::Tool, "tool output"),
            ],
            pending_approval: None,
            pending_input: None,
            usage: Default::default(),
            activities: Vec::new(),
            message_timeline_ordinals: vec![0, 1, 2, 3, 4, 5],
        });
        let plan = view.plan.as_ref().expect("plan restored from projection");
        assert_eq!(plan.total(), 1);
        assert_eq!(plan.done_count(), 1);
    });
}

#[gpui_kit::test]
fn run_projection_keeps_existing_timeline_and_suppresses_redundant_summary(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |view| {
        view.sessions = vec![view.active_session.clone()];
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn::text(
            "existing transcript",
        ))];
        view.apply_run_projection(loom_protocol::AgentRunSnapshotProjection {
            run: loom_protocol::AgentRunSnapshot {
                id: RunId::new(),
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: view.active_session.id,
                task: "task".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Completed,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: Some(Timestamp::from_unix_millis(2)),
                summary: Some("Completed task: task".to_owned()),
                evidence: Vec::new(),
            },
            plan: vec![
                loom_protocol::AgentPlanStep {
                    id: "step-1".to_owned(),
                    description: "Keep the timeline".to_owned(),
                },
                loom_protocol::AgentPlanStep {
                    id: "step-2".to_owned(),
                    description: "Restore progress".to_owned(),
                },
            ],
            plan_progress: loom_protocol::AgentPlanProgress {
                completed: vec![0],
                active: Some(1),
            },
            messages: vec![loom_model::ModelMessage::new(
                loom_model::MessageRole::User,
                "do not duplicate",
            )],
            pending_approval: None,
            pending_input: None,
            usage: Default::default(),
            activities: Vec::new(),
            message_timeline_ordinals: vec![0],
        });
        // The plan is seeded from the projection even though the timeline
        // already held a transcript, so reopening a run keeps its plan.
        assert_eq!(view.timeline.len(), 1);
        let plan = view.plan.as_ref().expect("plan restored with progress");
        assert_eq!(plan.total(), 2);
        assert_eq!(plan.done_count(), 1);
        assert_eq!(plan.active_step(), Some("Restore progress"));
    });
}

#[gpui_kit::test]
fn session_activation_resets_projection_and_resolves_backend_ownership(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let session_id = loom_core::AgentSessionId::new();
        let model = ModelId::new("deterministic/next");
        view.session_models.insert(session_id, model.clone());
        view.session_auto_approve_actions.insert(session_id, false);
        view.timeline
            .push(TimelineItem::System(SystemNote::status("old status")));
        view.pending_input = Some("old prompt".to_owned());
        view.active_run_id = Some(RunId::new());
        view.review.selected_path = Some("old.rs".to_owned());
        view.session_node_ids
            .insert(session_id, view.default_backend_node_id.clone());

        view.activate_session(loom_core::AgentSessionSnapshot {
            id: session_id,
            workspace_id: view.workspace_id,
            name: "Next session".to_owned(),
            state: loom_core::AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(1),
        });

        assert_eq!(view.model, model);
        assert!(!view.auto_approve_actions);
        assert!(view.timeline.is_empty());
        assert!(view.pending_input.is_none());
        assert!(view.active_run_id.is_none());
        assert!(view.review.selected_path.is_none());
        assert!(
            view.backend_for_request(&loom_protocol::ClientRequest::Session(
                SessionRequest::GetAgentSessionSnapshot { session_id }
            ))
            .is_ok()
        );
        assert!(
            view.backend_for_request(&loom_protocol::ClientRequest::Provider(
                ProviderRequest::ListProviders
            ))
            .is_ok()
        );

        view.review.rows = vec![
            ReviewRow::Hunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
            },
            ReviewRow::Line(GitDiffLine {
                kind: GitDiffLineKind::Removed,
                old_line: Some(1),
                new_line: None,
                content: "removed".to_owned(),
            }),
            ReviewRow::Line(GitDiffLine {
                kind: GitDiffLineKind::Added,
                old_line: None,
                new_line: Some(1),
                content: "added".to_owned(),
            }),
            ReviewRow::Line(GitDiffLine {
                kind: GitDiffLineKind::Context,
                old_line: Some(2),
                new_line: Some(2),
                content: "context".to_owned(),
            }),
        ];
        view.review.hunk_rows = vec![0];
        view.review.collapsed_hunks.insert(0);
        for index in 0..=view.review.rows.len() {
            let _ = view.render_review_row(index);
        }
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn settings_about_and_providers_panes_render(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |view| view.settings_open = true);
    render_scenario(cx, |view| {
        view.settings_open = true;
        view.settings_section = SettingsSection::About;
    });
    render_scenario(cx, |view| {
        view.settings_open = true;
        view.settings_section = SettingsSection::Providers;
    });
}

#[gpui_kit::test]
fn settings_about_pane_renders_every_backend_mode_and_a_phone_layout(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let open_about = |view: &mut LoomView| {
        view.settings_open = true;
        view.settings_section = SettingsSection::About;
    };
    // Local in-process backend.
    render_scenario(cx, open_about);
    // Deterministic demo backend.
    render_scenario(cx, |view| {
        open_about(view);
        view.demo_workspace = true;
        view.server_protocol_version = Some(loom_protocol::CURRENT_PROTOCOL_VERSION);
    });
    // Native `--remote` backend, with a matching server protocol.
    render_scenario(cx, |view| {
        open_about(view);
        view.backend_endpoint = Some("wss://worker.example:8443/ws".to_owned());
        view.server_protocol_version = Some(loom_protocol::CURRENT_PROTOCOL_VERSION);
    });
    // Browser client, with a server protocol that differs from the client's so
    // the warning tone path is exercised.
    render_scenario(cx, |view| {
        open_about(view);
        view.browser_client = true;
        view.backend_endpoint = Some("wss://worker.example:8443/ws?token=hidden".to_owned());
        view.server_protocol_version = Some(loom_core::ProtocolVersion::new(11, 0));
    });
    // Phone layout stacks every control below its label.
    render_scenario_at(cx, size(px(460.), px(820.)), open_about);
}

#[gpui_kit::test]
fn settings_about_pane_exposes_stable_test_ids(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.settings_open = true;
            view.settings_section = SettingsSection::About;
            view.backend_endpoint = Some("wss://worker.example:8443/ws?token=hidden".to_owned());
            view.server_protocol_version = Some(loom_core::ProtocolVersion::new(11, 1));
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        for id in [
            "about-subtitle",
            "about-version",
            "about-platform",
            "about-protocol",
            "about-link-docs",
            "about-link-source",
            "about-link-releases",
            "about-link-security",
        ] {
            assert!(
                window.within("settings-dialog").find(id).visible(),
                "expected {id} to be visible in the About pane"
            );
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn settings_section_navigation_renders_every_pane(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        LoomView::new_for_test(cx.focus_handle())
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("settings-button", cx);
        window.render_frame(cx);
        for index in 0..SETTINGS_SECTIONS.len() {
            window
                .within("settings-dialog")
                .click(("settings-section", index), cx);
            window.render_frame(cx);
            assert!(
                window
                    .within("settings-dialog")
                    .find(("settings-section", index))
                    .visible()
            );
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn settings_dialog_close_control_handles_a_real_click(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        LoomView::new_for_test(cx.focus_handle())
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("settings-button", cx);
        window.render_frame(cx);
        assert!(
            window
                .within("settings-dialog")
                .find("close-settings")
                .visible()
        );
        window
            .within("settings-dialog")
            .click("cpu-pulse-threshold-decrease", cx);
        window
            .within("settings-dialog")
            .click("cpu-pulse-threshold-increase", cx);
        window
            .within("settings-dialog")
            .click("project-agent-concurrency-decrease", cx);
        window
            .within("settings-dialog")
            .click("project-agent-concurrency-increase", cx);
        window
            .within("settings-dialog")
            .click("session-auto-approve-toggle", cx);
        window.within("settings-dialog").click("close-settings", cx);
        window.render_frame(cx);
        assert!(window.try_find("settings-dialog").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn session_source_dialog_choices_and_close_button_work(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::Empty,
                local_directory_available: true,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: None,
            });
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("source-local-directory").visible());
        window.click("source-local-directory", cx);
        window.click("source-github", cx);
        window.click("close-source-dialog", cx);
        window.render_frame(cx);
        assert!(window.try_find("source-local-directory").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn local_worker_current_directory_prefills_the_source_dialog(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.local_current_directory = Some(std::path::PathBuf::from("/tmp/current-project"));

            // Adding to a session opens on the local folder and prefills it.
            view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
            assert_eq!(
                view.pending_source_path.as_deref(),
                Some("/tmp/current-project")
            );

            // Choosing the local folder after another choice prefills again.
            view.pending_source_path = None;
            view.choose_source(SessionSourceChoice::GitHub, cx);
            view.choose_source(SessionSourceChoice::LocalDirectory, cx);
            assert_eq!(
                view.pending_source_path.as_deref(),
                Some("/tmp/current-project")
            );

            // The explicit action replaces any pending path, and is a no-op
            // when there is no native current directory.
            view.pending_source_path = Some("/somewhere/else".to_owned());
            view.use_current_source_directory(cx);
            assert_eq!(
                view.pending_source_path.as_deref(),
                Some("/tmp/current-project")
            );
            view.local_current_directory = None;
            view.pending_source_path = None;
            view.use_current_source_directory(cx);
            assert!(view.pending_source_path.is_none());

            // Restore the dialog for a real render and click.
            view.local_current_directory = Some(std::path::PathBuf::from("/tmp/current-project"));
            view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("use-current-session-directory").visible());
        window.click("use-current-session-directory", cx);
        window.render_frame(cx);
        // Confirming without a typed path uses the prefilled current folder.
        window.click("confirm-session-source", cx);
        window.render_frame(cx);
    })
    .unwrap();
}

#[gpui_kit::test]
fn phone_source_dialog_stacks_choices_and_keeps_actions_visible(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(520.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: true,
                filter_subscription: None,
                repositories: (0..40)
                    .map(|index| GitHubRepository {
                        full_name: format!("owner/repository-{index}"),
                        description: Some("example repository".to_owned()),
                        clone_url: format!("https://github.com/owner/repository-{index}.git"),
                        private: false,
                        default_branch: "main".to_owned(),
                    })
                    .collect(),
                selected_repository: None,
                repositories_loading: false,
                error: None,
            });
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        let viewport = window.viewport_size();
        let empty = window.find("source-empty");
        let local = window.find("source-local-directory");
        let github = window.find("source-github");
        assert!(empty.visible() && local.visible() && github.visible());
        // The three source choices stack vertically on a phone-width window
        // instead of overflowing the dialog's right edge.
        assert!(empty.bounds().bottom() <= local.bounds().top());
        assert!(local.bounds().bottom() <= github.bounds().top());
        for id in [
            "source-empty",
            "source-local-directory",
            "source-github",
            "cancel-session-source",
            "confirm-session-source",
        ] {
            let bounds = window.find(id).bounds();
            assert!(
                bounds.right() <= viewport.width,
                "{id} ran off the right edge: {bounds:?}"
            );
        }
        // On a phone the dialog renders as a full-height pane rather than a
        // fixed-width modal that clips its contents.
        assert!(window.find("source-dialog-pane").visible());
        assert!(window.find("close-source-dialog").visible());
        assert!(window.find("source-dialog-content").visible());
        let pane = window.find("source-dialog-pane").bounds();
        assert_eq!(pane.size.width, viewport.width);
        // The pane fills the central area below the window title bar.
        assert_eq!(pane.size.height, viewport.height - px(30.));
        // The actions stay pinned and reachable even though the repository
        // list inside the scrollable body is longer than the window.
        assert!(window.find("cancel-session-source").visible());
        assert!(window.find("confirm-session-source").visible());
        // The pane sits above the window chrome, so its controls receive
        // clicks rather than the content behind them.
        window.click("cancel-session-source", cx);
        window.render_frame(cx);
        assert!(window.try_find("source-dialog-pane").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn github_source_body_stacks_vertically(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: false,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: None,
            });
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        // The search field sits on its own row below the choices and spans the
        // form, rather than being squeezed into a row with the headings.
        let choices = window.find("source-github").bounds();
        let filter = window.find("github-repository-filter").bounds();
        assert!(filter.top() >= choices.bottom());
        assert!(filter.size.width >= px(400.), "{filter:?}");
    })
    .unwrap();
}

#[gpui_kit::test]
fn desktop_source_dialog_renders_as_a_pane(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        // Desktop uses the same full-height pane as phone instead of a modal.
        assert!(window.find("source-dialog-pane").visible());
        assert!(window.find("close-source-dialog").visible());
        assert!(window.find("confirm-session-source").visible());
        assert!(window.find("source-dialog-pane").bounds().right() <= window.viewport_size().width);
    })
    .unwrap();
}

#[gpui_kit::test]
fn session_source_and_review_actions_cover_empty_invalid_and_missing_states(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());

            view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
            view.choose_source(SessionSourceChoice::LocalDirectory, cx);
            view.confirm_source_dialog(cx);
            assert!(view.source_dialog.is_some());
            assert!(view.status_banner.is_some());

            view.choose_source(SessionSourceChoice::GitHub, cx);
            view.choose_source(SessionSourceChoice::Empty, cx);
            view.confirm_source_dialog(cx);
            assert!(view.source_dialog.is_none());

            view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
            view.choose_source(SessionSourceChoice::GitHub, cx);
            view.confirm_source_dialog(cx);
            assert!(view.source_dialog.is_some());

            view.review.open = false;
            view.toggle_review_pane(cx);
            assert!(view.review.open);
            view.jump_review_hunk(true, cx);
            view.review.hunk_rows = vec![2, 5];
            view.jump_review_hunk(true, cx);
            assert_eq!(view.review.selected_hunk, 0);
            view.jump_review_hunk(false, cx);
            assert_eq!(view.review.selected_hunk, 0);
            view.open_review_diff("missing.txt".to_owned(), false, cx);
            assert!(view.review.selected_path.is_none());
            view.open_review_file("missing.txt".to_owned(), cx);
            assert_eq!(view.review.selected_path.as_deref(), Some("missing.txt"));
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn settings_provider_views_and_theme_actions_update_the_view_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.review.open = true;
        view.settings_open = true;
        view.github_login = Some(GitHubLoginState::Starting);
        view.open_providers_from_menu(cx);
        assert!(view.settings_open);
        assert_eq!(view.settings_section, SettingsSection::Providers);
        assert!(!view.review.open);
        assert!(view.github_login.is_none());
        assert_eq!(view.providers_node_id.as_deref(), Some("test-node"));

        view.observe_system_appearance(window, cx);
        view.observe_system_appearance(window, cx);
        view.select_theme(ThemeChoice::Light, window, cx);
        assert_eq!(view.theme_choice, ThemeChoice::Light);
        view.select_theme(ThemeChoice::Dark, window, cx);
        assert_eq!(view.theme_choice, ThemeChoice::Dark);
        view.select_theme(ThemeChoice::System, window, cx);
        assert_eq!(view.theme_choice, ThemeChoice::System);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn parked_runs_keep_polling_until_terminal(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        LoomView::new_for_test(cx.focus_handle())
    });
    cx.update_window(handle.into(), |view, _, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        view.update(cx, |view, _| {
            view.active_run_id = Some(RunId::new());
            view.run_state = Some(loom_protocol::AgentRunState::Executing);
            assert!(view.run_should_poll());
            view.run_state = Some(loom_protocol::AgentRunState::Paused);
            assert!(view.run_should_poll());
            view.run_state = Some(loom_protocol::AgentRunState::Completed);
            assert!(!view.run_should_poll());
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn repeated_tool_calls_collapse_into_a_group(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let parts = ["a.rs", "b.rs", "c.rs", "d.rs"]
            .iter()
            .map(|path| {
                AssistantPart::Tool(Box::new(ToolPart {
                    id: ToolCallId::new(),
                    name: "read_file".to_owned(),
                    title: format!("Read {path}"),
                    status: ToolPartStatus::Completed,
                    detail: Some((*path).to_owned()),
                    output: None,
                    elapsed_ms: Some(1),
                    approval_pending: false,
                }))
            })
            .collect();
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts,
            streaming: false,
        })];
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        window.render_frame(cx);
        // The run of calls starts behind the collapsed usage summary.
        assert!(window.try_find(("tool-usage-header", 0u64)).is_some());
        assert!(window.try_find(("tool-group-header", 0u64)).is_none());
        window.click(("tool-usage-header", 0u64), cx);
        window.render_frame(cx);
        assert!(window.try_find(("tool-group-header", 0u64)).is_some());
        assert!(window.try_find(("tool-header", 0u64)).is_none());
        window.click(("tool-group-header", 0u64), cx);
        window.render_frame(cx);
        assert!(window.try_find(("tool-header", 0u64)).is_some());
        let view = view.downcast::<LoomView>().unwrap();
        view.update(cx, |view, _| {
            assert!(view.expanded_tool_usage.contains(&0));
            assert!(view.expanded_tool_groups.contains(&0));
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn tool_usage_summary_lines_collapse_by_default(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let tool = |name: &str, title: &str| {
        AssistantPart::Tool(Box::new(ToolPart {
            id: ToolCallId::new(),
            name: name.to_owned(),
            title: title.to_owned(),
            status: ToolPartStatus::Completed,
            detail: Some(title.to_owned()),
            output: None,
            elapsed_ms: Some(4),
            approval_pending: false,
        }))
    };
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts: vec![
                tool("read_file", "a.rs"),
                tool("read_file", "b.rs"),
                tool("run_command", "cargo test"),
            ],
            streaming: false,
        })];
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        // Collapsed by default: only the single summary line is visible.
        assert!(window.try_find(("tool-usage-header", 0u64)).is_some());
        assert!(window.try_find(("tool-header", 0u64)).is_none());
        assert!(window.try_find(("tool-header", 1u64)).is_none());
        let summary = window.find(("tool-usage-header", 0u64));
        let label = summary.label().unwrap_or_default().to_owned();
        assert!(label.contains("Read ×2"), "summary was {label:?}");
        assert!(label.contains("Run ×1"), "summary was {label:?}");

        // Expanding reveals the existing per-tool presentation.
        window.click(("tool-usage-header", 0u64), cx);
        window.render_frame(cx);
        assert!(window.try_find(("tool-header", 0u64)).is_some());
        assert!(window.try_find(("tool-header", 1u64)).is_some());
        assert!(window.try_find(("tool-header", 2u64)).is_some());
        view.update(cx, |view, _| {
            assert!(view.expanded_tool_usage.contains(&0));
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_running_tool_usage_stays_collapsed_until_expanded(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        // A running call is enough to exercise the collapsed summary; an active
        // run state is deliberately omitted because rendering it would schedule
        // a poll that never parks the test harness.
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts: vec![AssistantPart::Tool(Box::new(ToolPart {
                id: ToolCallId::new(),
                name: "run_command".to_owned(),
                title: "Run cargo test".to_owned(),
                status: ToolPartStatus::Running,
                detail: Some("cargo test".to_owned()),
                output: Some("running tests".to_owned()),
                elapsed_ms: None,
                approval_pending: false,
            }))],
            streaming: false,
        })];
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        // A running call must not pop the summary open and shut as it settles.
        assert!(window.try_find(("tool-usage-header", 0u64)).is_some());
        assert!(window.try_find(("tool-header", 0u64)).is_none());
        // The live status still reads from the collapsed line.
        let summary = window.find(("tool-usage-header", 0u64));
        let label = summary.label().unwrap_or_default().to_owned();
        assert!(label.contains("running"), "summary was {label:?}");

        // Clicking opens it, and it stays open.
        window.click(("tool-usage-header", 0u64), cx);
        window.render_frame(cx);
        assert!(window.try_find(("tool-header", 0u64)).is_some());
        view.update(cx, |view, _| {
            assert!(view.expanded_tool_usage.contains(&0));
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_running_tool_group_stays_collapsed_until_expanded(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let parts = (0..3)
            .map(|offset| {
                AssistantPart::Tool(Box::new(ToolPart {
                    id: ToolCallId::new(),
                    name: "read_file".to_owned(),
                    title: format!("Read {offset}.rs"),
                    status: ToolPartStatus::Running,
                    detail: Some(format!("{offset}.rs")),
                    output: None,
                    elapsed_ms: None,
                    approval_pending: false,
                }))
            })
            .collect();
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts,
            streaming: false,
        })];
        // Reveal the group row behind the usage summary.
        view.expanded_tool_usage.insert(0);
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        assert!(window.try_find(("tool-group-header", 0u64)).is_some());
        assert!(window.try_find(("tool-header", 0u64)).is_none());
        window.click(("tool-group-header", 0u64), cx);
        window.render_frame(cx);
        assert!(window.try_find(("tool-header", 0u64)).is_some());
        view.update(cx, |view, _| {
            assert!(view.expanded_tool_groups.contains(&0));
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn an_approval_gated_tool_usage_opens_for_the_decision(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let call = loom_model::ToolCall {
        id: ToolCallId::new(),
        name: "run_command".to_owned(),
        arguments: serde_json::Value::Null,
    };
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.pending_approval = Some(call.clone());
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts: vec![AssistantPart::Tool(Box::new(ToolPart {
                id: call.id,
                name: "run_command".to_owned(),
                title: "Run cargo test".to_owned(),
                status: ToolPartStatus::AwaitingApproval,
                detail: Some("cargo test".to_owned()),
                output: None,
                elapsed_ms: None,
                approval_pending: true,
            }))],
            streaming: false,
        })];
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // The decision control must be reachable without an extra click.
        assert!(window.try_find(("tool-header", 0u64)).is_some());
        assert!(window.try_find(("approve-tool", 0u64)).is_some());
        assert!(window.try_find(("reject-tool", 0u64)).is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn recovered_failures_do_not_condemn_the_tool_summary(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let tool = |name: &str, status: ToolPartStatus| {
        AssistantPart::Tool(Box::new(ToolPart {
            id: ToolCallId::new(),
            name: name.to_owned(),
            title: name.to_owned(),
            status,
            detail: None,
            output: None,
            elapsed_ms: Some(1),
            approval_pending: false,
        }))
    };
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts: vec![
                tool("read_file", ToolPartStatus::Completed),
                tool("read_file", ToolPartStatus::Failed),
                tool("read_file", ToolPartStatus::Completed),
                tool("run_command", ToolPartStatus::Completed),
            ],
            streaming: false,
        })];
        // Expand the usage summary so the same-kind group row also renders.
        view.expanded_tool_usage.insert(0);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let summary = window.find(("tool-usage-header", 0u64));
        let label = summary.label().unwrap_or_default().to_owned();
        assert!(label.contains("done"), "summary was {label:?}");
        assert!(label.contains("1 failed"), "summary was {label:?}");

        let group = window.find(("tool-group-header", 0u64));
        let label = group.label().unwrap_or_default().to_owned();
        assert!(label.contains("Read 3 files"), "group was {label:?}");
        assert!(label.contains("done"), "group was {label:?}");
        assert!(label.contains("1 failed"), "group was {label:?}");
    })
    .unwrap();
}

#[gpui_kit::test]
fn reasoning_and_tool_cycles_gather_into_one_agent_entry(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let tool = |name: &str, title: &str| {
        AssistantPart::Tool(Box::new(ToolPart {
            id: ToolCallId::new(),
            name: name.to_owned(),
            title: title.to_owned(),
            status: ToolPartStatus::Completed,
            detail: Some(title.to_owned()),
            output: None,
            elapsed_ms: Some(3),
            approval_pending: false,
        }))
    };
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.show_reasoning = true;
        view.sessions = vec![view.active_session.clone()];
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts: vec![
                AssistantPart::Reasoning("first thought".to_owned()),
                tool("read_file", "a.rs"),
                AssistantPart::Reasoning("second thought".to_owned()),
                tool("run_command", "cargo test"),
            ],
            streaming: false,
        })];
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        // The reasonings gather into one disclosure at the top of the entry.
        assert!(window.try_find(("reasoning-header", 0u64)).is_some());
        assert!(window.try_find(("reasoning-header", 2u64)).is_none());
        // Both cycles share a single tool run rather than one line each.
        assert!(window.try_find(("tool-usage-header", 1u64)).is_some());
        assert!(window.try_find(("tool-usage-header", 3u64)).is_none());
        assert!(window.try_find(("tool-header", 1u64)).is_none());
        let summary = window.find(("tool-usage-header", 1u64));
        let label = summary.label().unwrap_or_default().to_owned();
        assert!(label.contains("Read ×1"), "summary was {label:?}");
        assert!(label.contains("Run ×1"), "summary was {label:?}");

        // The gathered reasoning is one disclosure that expands in place.
        window.click(("reasoning-header", 0u64), cx);
        window.render_frame(cx);
        view.update(cx, |view, _| {
            assert!(view.expanded_reasoning.contains(&0));
        });

        window.click(("tool-usage-header", 1u64), cx);
        window.render_frame(cx);
        assert!(window.try_find(("tool-header", 1u64)).is_some());
        assert!(window.try_find(("tool-header", 3u64)).is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn tool_groups_render_pending_active_and_failed_statuses(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |view| {
        view.timeline = [
            ToolPartStatus::Queued,
            ToolPartStatus::Running,
            ToolPartStatus::Failed,
            ToolPartStatus::Cancelled,
        ]
        .into_iter()
        .map(|status| {
            let parts = (0..3)
                .map(|offset| {
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: ToolCallId::new(),
                        name: "run_command".to_owned(),
                        title: format!("Run command {offset}"),
                        status,
                        detail: None,
                        output: None,
                        elapsed_ms: None,
                        approval_pending: false,
                    }))
                })
                .collect();
            TimelineItem::Assistant(AssistantTurn {
                parts,
                streaming: false,
            })
        })
        .collect();
    });
}

#[gpui_kit::test]
fn command_palette_and_composer_completion_render(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        LoomView::new_for_test(cx.focus_handle())
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        view.update(cx, |view, cx| view.toggle_command_palette(cx));
        window.render_frame(cx);
        assert!(window.try_find("command-palette").is_some());
        view.update(cx, |view, cx| view.close_command_palette(cx));
        window.render_frame(cx);
        assert!(window.try_find("command-palette").is_none());

        view.update(cx, |view, cx| {
            if let Some(input) = view.composer_input.clone() {
                input.update(cx, |state, cx| state.set_value("/re", window, cx));
            }
            view.composer_completion = Some(super::ComposerCompletion {
                kind: super::CompletionKind::Command,
                query: "/re".to_owned(),
                selected: 0,
            });
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.try_find("composer-completions").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn primary_shift_p_opens_the_command_palette(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        LoomView::new_for_test(cx.focus_handle())
    });
    let window: gpui_kit::AnyWindowHandle = handle.into();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    // Platform key events report the shifted character for `key`, so the
    // binding must match `P` as well as `p`.
    let shortcut = || gpui_kit::Keystroke {
        modifiers: gpui_kit::Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        },
        key: "P".to_owned(),
        key_char: None,
    };
    cx.dispatch_keystroke(window, shortcut());
    cx.update_window(window, |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        assert!(view.read(cx).command_palette_open);
        window.render_frame(cx);
        assert!(window.try_find("command-palette").is_some());
    })
    .unwrap();
    // Pressing it again while the palette's own input is focused must
    // close the palette, not get swallowed by that input.
    cx.dispatch_keystroke(window, shortcut());
    cx.update_window(window, |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        assert!(!view.read(cx).command_palette_open);
        // Tear down the palette input, dropping focus, before pressing again.
        window.render_frame(cx);
    })
    .unwrap();
    cx.dispatch_keystroke(window, shortcut());
    cx.update_window(window, |view, _window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        assert!(view.read(cx).command_palette_open);
    })
    .unwrap();
}

#[gpui_kit::test]
fn github_login_failures_update_account_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.handle_github_device_code(
            Err(loom_core::LoomError::new(
                ErrorCode::ProviderUnavailable,
                "device failed",
                true,
            )),
            cx,
        );
        assert!(matches!(
            view.github_login,
            Some(GitHubLoginState::Error(_))
        ));
        view.finish_github_login(
            Err(loom_core::LoomError::new(
                ErrorCode::ProviderUnavailable,
                "poll failed",
                true,
            )),
            cx,
        );
        assert!(matches!(
            view.github_login,
            Some(GitHubLoginState::Error(_))
        ));
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn github_repository_login_finishes_by_configuring_repository_access(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.github_login_kind = crate::state::GitHubLoginKind::Repository;
        view.finish_github_login(Ok("gho_repo_token".to_owned()), cx);
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            assert!(view.github_repository_connected);
            assert!(matches!(view.github_login, Some(GitHubLoginState::Success)));
            view.refresh_github_repository_access(view.default_backend_node_id.clone(), cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn worker_connection_rejects_empty_credentialed_and_duplicate_inputs(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.connect_worker_node(cx);
        view.node_input_initial = "wss://user:secret@worker.example/ws token".to_owned();
        view.connect_worker_node(cx);
        assert_eq!(view.worker_nodes.len(), 1);
        assert_eq!(
            view.worker_nodes[0].connection_state,
            WorkerConnectionState::Failed
        );
        assert!(
            view.worker_nodes[0]
                .connection_detail
                .as_deref()
                .unwrap()
                .contains("Do not include credentials")
        );
        view.connect_worker_node(cx);
        assert_eq!(view.worker_nodes.len(), 1);
        view.node_input_initial = "wss://worker-without-token.example/ws".to_owned();
        view.connect_worker_node(cx);
        assert_eq!(view.worker_nodes.len(), 2);
        assert!(
            view.worker_nodes[1]
                .connection_detail
                .as_deref()
                .unwrap()
                .contains("URL followed by its access token")
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
async fn worker_connection_failure_after_valid_input_is_reported(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.node_input_initial = "ws://127.0.0.1:1/ws test-token".to_owned();
        view.connect_worker_node(cx);
        assert_eq!(view.worker_nodes.len(), 1);
        view
    });
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window.root::<LoomView>().flatten().is_some_and(|view| {
            let view = view.read(cx);
            view.worker_nodes.iter().any(|node| {
                node.url.as_deref() == Some("ws://127.0.0.1:1/ws")
                    && node.connection_state == WorkerConnectionState::Failed
                    && node.connection.is_none()
            })
        })
    })
    .await;
}

#[gpui_kit::test]
fn reconnect_rejects_saved_url_credentials_and_worker_can_be_removed(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.worker_nodes.push(super::connection_placeholder(
            9,
            "wss://user:secret@worker.example/ws".to_owned(),
            WorkerConnectionState::Failed,
            None,
        ));
        view.reconnect_configured_worker_nodes(cx);
        assert_eq!(
            view.worker_nodes[0].connection_state,
            WorkerConnectionState::Failed
        );
        assert!(
            view.worker_nodes[0]
                .connection_detail
                .as_deref()
                .unwrap()
                .contains("credentials")
        );
        view.remove_worker_node(9, cx);
        assert!(view.worker_nodes.is_empty());
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn tool_blocks_toggle_and_show_inline_approvals(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let run_id = RunId::new();
    let call = ToolCall {
        id: ToolCallId::new(),
        name: "run_command".to_owned(),
        arguments: serde_json::json!({"command": "cargo", "args": ["test"]}),
    };
    let record = AgentActivityRecord {
        id: ActivityId::new(),
        run_id,
        timeline_ordinal: 0,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::Command,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(1),
        completed_at: None,
        elapsed_ms: Some(10),
        data: AgentActivityData::Command {
            call: call.clone(),
            command: "cargo".to_owned(),
            args: vec!["test".to_owned()],
            cwd: Some("repo".to_owned()),
            result: Some(ToolResult::success(&call, "test result: ok".to_owned())),
        },
    };
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.active_run_id = Some(run_id);
        view.consume_agent_event(&loom_protocol::AgentEvent::ActivityRecorded {
            run_id,
            activity: record.clone(),
        });
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        window.click(("tool-usage-header", 0u64), cx);
        window.render_frame(cx);
        window.click(("tool-header", 0u64), cx);
        window.render_frame(cx);
        view.update(cx, |view, _| {
            assert!(view.expanded_tools.contains(&call.id));
            // Late activity updates keep the block in place and reveal approval.
            view.consume_agent_event(&loom_protocol::AgentEvent::ActivityRecorded {
                run_id,
                activity: AgentActivityRecord {
                    status: AgentActivityStatus::AwaitingApproval,
                    ..record.clone()
                },
            });
            view.pending_approval = Some(call.clone());
        });
        window.render_frame(cx);
        assert!(window.try_find(("approve-tool", 0u64)).is_some());
        assert!(window.try_find(("reject-tool", 0u64)).is_some());
        window.click(("approve-tool", 0u64), cx);
        window.render_frame(cx);
    })
    .unwrap();
}

#[gpui_kit::test]
fn action_only_tools_hide_their_result_body(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let read_id = ToolCallId::new();
    let command_id = ToolCallId::new();
    let failed_command_id = ToolCallId::new();
    let search_id = ToolCallId::new();
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts: vec![
                AssistantPart::Tool(Box::new(ToolPart {
                    id: read_id,
                    name: "read_file".to_owned(),
                    title: "Read src/lib.rs".to_owned(),
                    status: ToolPartStatus::Completed,
                    detail: Some("src/lib.rs".to_owned()),
                    output: Some("fn main() {}".to_owned()),
                    elapsed_ms: Some(4),
                    approval_pending: false,
                })),
                AssistantPart::Tool(Box::new(ToolPart {
                    id: command_id,
                    name: "run_command".to_owned(),
                    title: "Run tests".to_owned(),
                    status: ToolPartStatus::Completed,
                    detail: Some("cargo test".to_owned()),
                    output: Some("test result: ok".to_owned()),
                    elapsed_ms: Some(8),
                    approval_pending: false,
                })),
                AssistantPart::Tool(Box::new(ToolPart {
                    id: failed_command_id,
                    name: "run_command".to_owned(),
                    title: "Run tests".to_owned(),
                    status: ToolPartStatus::Failed,
                    detail: Some("cargo test".to_owned()),
                    output: Some("test result: FAILED".to_owned()),
                    elapsed_ms: Some(9),
                    approval_pending: false,
                })),
                AssistantPart::Tool(Box::new(ToolPart {
                    id: search_id,
                    name: "web_search".to_owned(),
                    title: "Web search \"needle\"".to_owned(),
                    status: ToolPartStatus::Completed,
                    detail: None,
                    output: Some(r#"{"results":[]}"#.to_owned()),
                    elapsed_ms: Some(3),
                    approval_pending: false,
                })),
            ],
            streaming: false,
        })];
        view.expanded_tools.insert(read_id);
        view.expanded_tools.insert(command_id);
        view.expanded_tools.insert(failed_command_id);
        view.expanded_tools.insert(search_id);
        // Reveal the tool blocks behind the usage summary.
        view.expanded_tool_usage.insert(0);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(("copy-tool-output", 0u64)).is_none(),
            "a successful read shows a pointer, not its contents"
        );
        assert!(
            window.try_find(("copy-tool-output", 1u64)).is_none(),
            "a successful command shows its command, not its stdout"
        );
        assert!(
            window.try_find(("copy-tool-output", 2u64)).is_none(),
            "a failed command keeps its error but not a copyable body"
        );
        assert!(
            window.try_find(("copy-tool-output", 3u64)).is_some(),
            "a web result exists nowhere else and stays visible"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn bodyless_tools_have_no_disclosure_and_failures_start_collapsed(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let read_id = ToolCallId::new();
    let failed_read_id = ToolCallId::new();
    let search_id = ToolCallId::new();
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts: vec![
                // A successful read with no range has nothing to reveal.
                AssistantPart::Tool(Box::new(ToolPart {
                    id: read_id,
                    name: "read_file".to_owned(),
                    title: "Read src/lib.rs".to_owned(),
                    status: ToolPartStatus::Completed,
                    detail: None,
                    output: Some("fn main() {}".to_owned()),
                    elapsed_ms: Some(4),
                    approval_pending: false,
                })),
                // A failed read is the same: the row reports the failure and
                // the agent decides whether it matters.
                AssistantPart::Tool(Box::new(ToolPart {
                    id: failed_read_id,
                    name: "read_file".to_owned(),
                    title: "Read missing.rs".to_owned(),
                    status: ToolPartStatus::Failed,
                    detail: None,
                    output: Some("No such file or directory".to_owned()),
                    elapsed_ms: Some(2),
                    approval_pending: false,
                })),
                // A successful result that exists nowhere else is shown, but
                // starts collapsed.
                AssistantPart::Tool(Box::new(ToolPart {
                    id: search_id,
                    name: "web_search".to_owned(),
                    title: "Web search \"needle\"".to_owned(),
                    status: ToolPartStatus::Completed,
                    detail: None,
                    output: Some(r#"{"results":[]}"#.to_owned()),
                    elapsed_ms: Some(3),
                    approval_pending: false,
                })),
            ],
            streaming: false,
        })];
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        window.click(("tool-usage-header", 0u64), cx);
        window.render_frame(cx);
        window.click(("tool-header", 0u64), cx);
        window.click(("tool-header", 1u64), cx);
        window.render_frame(cx);
        view.update(cx, |view, _| {
            assert!(
                !view.expanded_tools.contains(&read_id),
                "a tool with nothing to reveal has no disclosure to toggle"
            );
            assert!(
                !view.expanded_tools.contains(&failed_read_id),
                "a failed tool has nothing to reveal; the agent reports it"
            );
            assert!(
                !view.expanded_tools.contains(&search_id),
                "a settled result starts collapsed"
            );
        });
        assert!(window.try_find(("copy-tool-output", 2u64)).is_none());

        // A result with a body can be opened and closed again.
        window.click(("tool-header", 2u64), cx);
        window.render_frame(cx);
        view.update(cx, |view, _| {
            assert!(view.expanded_tools.contains(&search_id))
        });
        assert!(window.try_find(("copy-tool-output", 2u64)).is_some());
        window.click(("tool-header", 2u64), cx);
        window.render_frame(cx);
        view.update(cx, |view, _| {
            assert!(!view.expanded_tools.contains(&search_id))
        });
        assert!(window.try_find(("copy-tool-output", 2u64)).is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn context_events_update_usage_and_only_record_compaction(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let mut inspection = loom_protocol::ContextInspection {
            items: Vec::new(),
            total_tokens: 200,
            included_tokens: 200,
            omitted_tokens: 0,
            budget: loom_protocol::ContextBudget::new(Some(1_000), None, 100).unwrap(),
            compacted: false,
            summary: None,
        };
        let run_id = RunId::new();
        view.consume_agent_event(&loom_protocol::AgentEvent::ContextInspected {
            run_id,
            inspection: inspection.clone(),
        });
        assert!(view.timeline.is_empty());
        assert_eq!(
            view.context_inspection.as_ref().unwrap().included_tokens,
            200
        );
        inspection.compacted = true;
        inspection.omitted_tokens = 120;
        inspection.included_tokens = 80;
        view.consume_agent_event(&loom_protocol::AgentEvent::ContextInspected {
            run_id,
            inspection: inspection.clone(),
        });
        assert!(
            view.status_banner
                .as_ref()
                .is_some_and(|note| note.text.contains("Context compacted")
                    && note.text.contains("lossy excerpts"))
        );
        let count = view.timeline.len();
        inspection.compacted = false;
        view.consume_agent_event(&loom_protocol::AgentEvent::ContextInspected {
            run_id,
            inspection,
        });
        assert_eq!(view.timeline.len(), count);
        assert_eq!(
            view.context_inspection.as_ref().unwrap().included_tokens,
            80
        );
        view.reset_projection();
        assert!(view.context_inspection.is_none());
    });
}

#[gpui_kit::test]
fn completing_a_run_stops_the_streaming_cursor(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let run_id = RunId::new();
        view.active_run_id = Some(run_id);
        view.consume_agent_event(&loom_protocol::AgentEvent::AssistantMessageDelta {
            run_id,
            message_id: 1,
            text: "working".to_owned(),
        });
        let streaming = |view: &LoomView| {
            view.timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::Assistant(turn) if turn.streaming))
        };
        assert!(streaming(&view));
        view.consume_agent_event(&loom_protocol::AgentEvent::RunStateChanged {
            run_id,
            state: loom_protocol::AgentRunState::Completed,
        });
        assert!(!streaming(&view));
    });
}

#[gpui_kit::test]
fn agent_event_projection_handles_the_run_lifecycle_and_tool_fallback(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let run_id = RunId::new();
            let call = ToolCall {
                id: ToolCallId::new(),
                name: "write_file".to_owned(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
            };
            let approval_interaction_id = loom_core::InteractionId::new();
            let input_interaction_id = loom_core::InteractionId::new();
            let snapshot = loom_protocol::AgentRunSnapshot {
                id: run_id,
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: view.active_session.id,
                task: "update the app".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Executing,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: None,
                summary: None,
                evidence: Vec::new(),
            };
            let inspection = loom_protocol::ContextInspection {
                items: Vec::new(),
                total_tokens: 0,
                included_tokens: 0,
                omitted_tokens: 0,
                budget: loom_protocol::ContextBudget {
                    context_window: None,
                    requested_input_tokens: None,
                    reserved_output_tokens: 0,
                    effective_input_tokens: None,
                },
                compacted: false,
                summary: None,
            };
            for event in [
                loom_protocol::AgentEvent::RunStarted {
                    snapshot: snapshot.clone(),
                },
                loom_protocol::AgentEvent::PlanProposed {
                    run_id,
                    plan: loom_protocol::AgentPlan {
                        steps: vec![loom_protocol::AgentPlanStep {
                            id: "edit".to_owned(),
                            description: "Edit the app".to_owned(),
                        }],
                    },
                },
                loom_protocol::AgentEvent::StepStarted {
                    run_id,
                    step_id: loom_core::StepId::new(),
                    index: 0,
                },
                loom_protocol::AgentEvent::StepCompleted {
                    run_id,
                    step_id: loom_core::StepId::new(),
                    index: 0,
                },
                loom_protocol::AgentEvent::ContextInspected { run_id, inspection },
                loom_protocol::AgentEvent::UserMessage {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 1,
                    interaction_id: Some(input_interaction_id),
                    text: "new request".to_owned(),
                },
                loom_protocol::AgentEvent::AssistantMessageDelta {
                    run_id,
                    message_id: 1,
                    text: "The change ".to_owned(),
                },
                loom_protocol::AgentEvent::AssistantMessageDelta {
                    run_id,
                    message_id: 1,
                    text: "is ready.".to_owned(),
                },
                loom_protocol::AgentEvent::ToolCallRequested {
                    run_id,
                    call: call.clone(),
                },
                loom_protocol::AgentEvent::ToolApprovalRequired {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 2,
                    interaction_id: approval_interaction_id,
                    call: call.clone(),
                },
                loom_protocol::AgentEvent::ToolPolicyEvaluated {
                    run_id,
                    call: call.clone(),
                    evaluation: loom_core::PolicyEvaluation {
                        action: loom_core::ActionKind::Write,
                        decision: loom_core::PolicyDecision::RequireApproval,
                        reason: "user approval is required".to_owned(),
                    },
                },
                loom_protocol::AgentEvent::ToolCallStarted {
                    run_id,
                    call: call.clone(),
                },
                loom_protocol::AgentEvent::ToolOutputChunk {
                    run_id,
                    tool_call_id: call.id,
                    chunk: "file updated".to_owned(),
                },
                loom_protocol::AgentEvent::ToolCallCompleted {
                    run_id,
                    result: ToolResult::success(&call, "done".to_owned()),
                },
                loom_protocol::AgentEvent::ToolApprovalDecided {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 3,
                    interaction_id: approval_interaction_id,
                    tool_call_id: call.id,
                    decision: loom_protocol::ApprovalDecision::Approved,
                },
                loom_protocol::AgentEvent::NeedsInput {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 4,
                    interaction_id: input_interaction_id,
                    prompt: "Which branch?".to_owned(),
                },
                loom_protocol::AgentEvent::RunUsage {
                    run_id,
                    usage: Default::default(),
                },
                loom_protocol::AgentEvent::RunUsageUpdated {
                    run_id,
                    usage: Default::default(),
                },
                loom_protocol::AgentEvent::RunLimitReached {
                    run_id,
                    status: loom_core::LimitStatus::new(
                        loom_core::SessionLimits::default(),
                        loom_core::UsageSnapshot::default(),
                    ),
                },
                loom_protocol::AgentEvent::RecoveryRequired {
                    run_id,
                    reason: "resume the session".to_owned(),
                },
                loom_protocol::AgentEvent::RunStateChanged {
                    run_id,
                    state: loom_protocol::AgentRunState::Paused,
                },
                loom_protocol::AgentEvent::ProviderError {
                    run_id,
                    error: loom_core::LoomError::new(ErrorCode::Internal, "provider failed", true),
                },
                loom_protocol::AgentEvent::ContextError {
                    run_id,
                    error: loom_core::LoomError::new(ErrorCode::Internal, "context failed", false),
                },
                loom_protocol::AgentEvent::RunCompleted {
                    snapshot: loom_protocol::AgentRunSnapshot {
                        state: loom_protocol::AgentRunState::Completed,
                        summary: Some("Finished the app update".to_owned()),
                        ..snapshot
                    },
                },
            ] {
                view.consume_agent_event(&event);
            }
            assert_eq!(
                view.run_state,
                Some(loom_protocol::AgentRunState::Completed)
            );
            assert!(view.pending_approval.is_none());
            assert_eq!(view.pending_input.as_deref(), Some("Which branch?"));
            assert!(view.timeline.iter().any(|item| matches!(
                item,
                TimelineItem::Assistant(turn) if turn.parts.iter().any(|part| matches!(part, AssistantPart::Text(text) if text == "Finished the app update"))
            )));

            view.consume_agent_event(&loom_protocol::AgentEvent::ToolCallRequested {
                run_id,
                call: call.clone(),
            });
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolCallStarted { run_id, call });
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolOutputChunk {
                run_id,
                tool_call_id: ToolCallId::new(),
                chunk: "suppressed fallback".to_owned(),
            });
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolCallCompleted {
                run_id,
                result: ToolResult::success(
                    &ToolCall {
                        id: ToolCallId::new(),
                        name: "read_file".to_owned(),
                        arguments: serde_json::Value::Null,
                    },
                    "done".to_owned(),
                ),
            });
            view
        });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn startup_session_load_restores_snapshot_and_source_lists(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.refresh_models();
        assert!(!view.default_models.is_empty());
        let workspace =
            crate::connection::create_workspace(&view.connection, "Loaded workspace").unwrap();
        let session = crate::connection::create_session_in_workspace(
            &view.connection,
            workspace.id,
            "Loaded session",
        )
        .unwrap();
        let started = view
            .connection
            .request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::StartSessionAgentRun {
                    session_id: session.id,
                    task: "startup transcript page".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    system_instructions: None,
                    repository_instructions: None,
                },
            )));
        let run_id = match started.result.unwrap() {
            ServerResponse::Run(RunResponse::AgentRunStarted(run)) => run.id,
            response => panic!("unexpected run start response: {response:?}"),
        };
        view.workspace_id = workspace.id;
        view.workspaces.push(workspace);
        view.refresh_sessions().unwrap();
        assert_eq!(view.sessions.len(), 1);
        view.load_session(session.clone());
        assert_eq!(view.active_session.id, session.id);
        assert_eq!(view.active_session.name, "Loaded session");
        assert!(view.after_sequence.is_some());
        assert!(view.event_stream_epoch.is_some());
        assert_eq!(view.active_run_id, Some(run_id));
        assert_eq!(view.transcript_before_ordinal, Some(0));
        assert!(view.timeline.iter().any(
            |item| matches!(item, TimelineItem::User(task) if task == "startup transcript page")
        ));
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn async_session_load_falls_back_to_run_projection_and_ignores_stale_responses(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let session_id = view.active_session.id;
            let run = loom_protocol::AgentRunSnapshot {
                id: RunId::new(),
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id,
                task: "recover the transcript".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Completed,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: Some(Timestamp::from_unix_millis(2)),
                summary: Some("Recovered run".to_owned()),
                evidence: Vec::new(),
            };
            let projection = loom_protocol::AgentRunSnapshotProjection {
                run: run.clone(),
                plan: vec![
                    loom_protocol::AgentPlanStep {
                        id: "step-1".to_owned(),
                        description: "Recover the transcript".to_owned(),
                    },
                    loom_protocol::AgentPlanStep {
                        id: "step-2".to_owned(),
                        description: "Resume the run".to_owned(),
                    },
                ],
                plan_progress: loom_protocol::AgentPlanProgress {
                    completed: vec![0],
                    active: Some(1),
                },
                messages: vec![loom_model::ModelMessage::new(
                    loom_model::MessageRole::User,
                    "recover the transcript",
                )],
                pending_approval: None,
                pending_input: None,
                usage: Default::default(),
                activities: Vec::new(),
                message_timeline_ordinals: vec![0],
            };
            let snapshot = loom_protocol::AgentSessionSnapshotProjection {
                session: view.active_session.clone(),
                active_run: Some(projection),
                latest_sequence: loom_core::EventSequence::new(7),
                approval_policy: Default::default(),
                auto_approve_actions: false,
            };
            view.finish_async_session_load(
                session_id,
                loom_protocol::ResponseEnvelope::success(
                    loom_core::RequestId::new(),
                    loom_protocol::ServerResponse::Session(SessionResponse::AgentSessionSnapshot(snapshot)),
                ),
                loom_protocol::ResponseEnvelope::success(
                    loom_core::RequestId::new(),
                    loom_protocol::ServerResponse::Events(EventsResponse::SessionEvents{
                        events: Vec::new(),
                        stream_epoch: None,
                    }),
                ),
                cx,
            );
            assert_eq!(view.active_run_id, Some(run.id));
            assert!(!view.auto_approve_actions);
            assert!(view.timeline.iter().any(|item| matches!(
                item,
                TimelineItem::Assistant(turn) if turn.parts.iter().any(|part| matches!(part, AssistantPart::Text(text) if text == "Recovered run"))
            )));
            // Reopening the session restores the plan's progress, not just its steps.
            let plan = view.plan.as_ref().expect("plan restored on reopen");
            assert_eq!(plan.total(), 2);
            assert_eq!(plan.done_count(), 1);
            assert_eq!(plan.active_step(), Some("Resume the run"));

            let old_timeline_len = view.timeline.len();
            view.finish_async_session_load(
                AgentSessionId::new(),
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::Internal, "stale", false),
                ),
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::Internal, "stale", false),
                ),
                cx,
            );
            assert_eq!(view.timeline.len(), old_timeline_len);
            view
        });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn composer_commands_and_failed_run_responses_are_projected(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.session_node_ids
            .insert(view.active_session.id, view.default_backend_node_id.clone());
        view.submit_composer(cx);
        view.run_slash_command("/help", cx);
        view.run_slash_command("/unknown", cx);
        view.run_slash_command("/repo", cx);
        assert!(view.source_dialog.is_some());
        view.source_dialog = None;
        view.run_slash_command("/review", cx);
        assert!(view.review.open);
        view.approve_pending_action(cx);
        view.reject_pending_action(cx);

        view.model = ModelId::new("worker/uncached-model");
        view.model_catalog_node_id = None;
        view.send_message("uncached model task".to_owned(), cx);
        assert!(view.status_banner.as_ref().is_some_and(|note| {
            note.tone == SystemTone::Error
                && note
                    .heading
                    .as_deref()
                    .is_some_and(|heading| heading.starts_with("start run"))
                && note.text.contains("has not been refreshed")
        }));

        view.model = ModelId::new("deterministic/demo");
        view.send_message("try a task".to_owned(), cx);
        assert!(!view.sending_message);
        assert!(
            !view
                .timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(_)))
        );
        view.sending_message = true;
        view.finish_send_response(
            loom_protocol::ResponseEnvelope::failure(
                loom_core::RequestId::new(),
                loom_core::LoomError::new(ErrorCode::ProviderUnavailable, "offline", true),
            ),
            cx,
        );
        assert!(!view.sending_message);
        view.approval_request_in_flight = true;
        view.finish_approval_response(
            loom_protocol::ResponseEnvelope::failure(
                loom_core::RequestId::new(),
                loom_core::LoomError::new(ErrorCode::InvalidState, "approval expired", false),
            ),
            cx,
        );
        assert!(!view.approval_request_in_flight);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn server_event_projection_updates_session_and_ignores_service_streams(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let snapshot = view.active_session.clone();
        let session_id = snapshot.id;
        view.consume_event(&loom_protocol::ServerEvent::AgentSessionCreated {
            snapshot: snapshot.clone(),
        });
        view.consume_event(&loom_protocol::ServerEvent::AgentSessionForked {
            source_session_id: session_id,
            snapshot: snapshot.clone(),
        });
        view.consume_event(&loom_protocol::ServerEvent::AgentSessionStateChanged {
            previous: loom_core::AgentSessionState::Idle,
            current: loom_core::AgentSessionState::Executing,
        });
        view.consume_event(&loom_protocol::ServerEvent::AgentSessionRenamed {
            session_id,
            name: "Renamed from event".to_owned(),
        });
        view.consume_event(&loom_protocol::ServerEvent::AgentSessionArchived { session_id });
        view.consume_event(&loom_protocol::ServerEvent::SessionFilesystemChanged {
            change: SessionFilesystemChange {
                sequence: loom_core::EventSequence::new(1),
                session_id,
                path: "README.md".to_owned(),
                kind: WorkspaceChangeKind::Modified,
                revision: None,
            },
        });
        view.consume_event(&loom_protocol::ServerEvent::Terminal {
            event: loom_protocol::TerminalEventRecord {
                sequence: loom_core::EventSequence::new(1),
                terminal_id: loom_core::TerminalId::new(),
                event: loom_protocol::TerminalEvent::StateChanged {
                    status: loom_protocol::TerminalStatus::Exited,
                },
            },
        });
        view.consume_event(&loom_protocol::ServerEvent::Task {
            event: loom_protocol::TaskEventRecord {
                sequence: loom_core::EventSequence::new(1),
                task_id: loom_core::TaskId::new(),
                event: loom_protocol::TaskEvent::StateChanged {
                    status: loom_protocol::TaskStatus::Completed,
                },
            },
        });
        view.consume_event(&loom_protocol::ServerEvent::ProviderHealthChanged {
            provider_id: loom_model::ProviderId::new("test-provider"),
            health: ProviderHealth::default(),
        });
        // Accepted project messages are orchestration traffic and carry no
        // user-facing projection, so consuming one must not disturb state.
        let timeline_before = view.timeline.len();
        view.consume_event(&loom_protocol::ServerEvent::ProjectAgentMessageAccepted {
            message: loom_core::AgentMessageRecord {
                message_id: loom_core::AgentMessageId::new(),
                project_id: loom_core::ProjectId::new(),
                task_id: None,
                sender_session_id: session_id,
                target_session_id: view.active_session.id,
                kind: loom_core::AgentMessageKind::Result,
                project_sequence: 1,
                accepted_at: loom_core::Timestamp::from_unix_millis(1),
                body: "child result".to_owned(),
            },
        });
        assert_eq!(view.timeline.len(), timeline_before);
        assert_eq!(view.session_state, loom_core::AgentSessionState::Archived);
        assert!(view.status_banner.is_some());
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn settings_and_provider_dialogs_render_configured_entries(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |view| {
        view.settings_open = true;
        view.browser_startup_error = Some("Workspace setup failed".to_owned());
        view.worker_nodes.push(WorkerNodeEntry {
            id: 1,
            status: WorkerNodeStatus {
                node_id: "remote-worker".to_owned(),
                name: "Remote worker".to_owned(),
                online: false,
                capabilities: CapabilitySet::default(),
                resources: WorkerNodeResources {
                    cpu_count: 0,
                    cpu_usage_percent: None,
                    memory_usage_percent: None,
                    memory_total_bytes: None,
                    memory_available_bytes: None,
                    disk_total_bytes: None,
                    disk_available_bytes: None,
                },
            },
            is_local: false,
            url: Some("wss://example.test/ws".to_owned()),
            connection: None,
            connection_state: WorkerConnectionState::Failed,
            connection_detail: Some("Connection timed out".to_owned()),
            severe_load_streak: 0,
        });
        for (id, state, online) in [
            (2, WorkerConnectionState::Disconnected, false),
            (3, WorkerConnectionState::Connecting, false),
            (4, WorkerConnectionState::Connected, true),
            (5, WorkerConnectionState::Connected, false),
        ] {
            view.worker_nodes.push(WorkerNodeEntry {
                id,
                status: WorkerNodeStatus {
                    node_id: format!("worker-{id}"),
                    name: format!("Worker {id}"),
                    online,
                    capabilities: CapabilitySet::default(),
                    resources: WorkerNodeResources {
                        cpu_count: 2,
                        cpu_usage_percent: Some(50),
                        memory_usage_percent: Some(75),
                        memory_total_bytes: Some(8 * 1024 * 1024),
                        memory_available_bytes: Some(2 * 1024 * 1024),
                        disk_total_bytes: None,
                        disk_available_bytes: None,
                    },
                },
                is_local: false,
                url: Some(format!("wss://worker-{id}.example.test/ws")),
                connection: None,
                connection_state: state,
                connection_detail: None,
                severe_load_streak: 0,
            });
        }
    });

    render_scenario(cx, |view| {
        let local_provider_id = loom_model::ProviderId::new("company-gateway");
        let github_provider_id = loom_model::ProviderId::new("github-copilot");
        view.settings_open = true;
        view.settings_section = SettingsSection::Providers;
        view.github_connected = true;
        view.providers = vec![
            ProviderSummary {
                id: local_provider_id.clone(),
                kind: ProviderKind::OpenAiCompatible,
                display_name: "Company gateway".to_owned(),
                models: vec![ModelDescriptor {
                    id: ModelId::new("gateway/model"),
                    provider: local_provider_id,
                    display_name: "Gateway model".to_owned(),
                    context_window: Some(32_000),
                    max_input_tokens: None,
                    max_output_tokens: None,
                    capabilities: ModelCapabilities::default(),
                }],
                credential_id: Some("gateway-key".to_owned()),
                api_key_configurable: true,
                health: ProviderHealth::default(),
            },
            ProviderSummary {
                id: github_provider_id,
                kind: ProviderKind::GitHubCopilot,
                display_name: "GitHub Copilot".to_owned(),
                models: Vec::new(),
                credential_id: None,
                api_key_configurable: false,
                health: ProviderHealth::default(),
            },
            ProviderSummary {
                id: loom_model::ProviderId::new("empty-ollama"),
                kind: ProviderKind::Ollama,
                display_name: "Ollama".to_owned(),
                models: Vec::new(),
                credential_id: None,
                api_key_configurable: false,
                health: ProviderHealth::default(),
            },
        ];
    });
}

#[gpui_kit::test]
fn github_login_states_and_phone_session_drawer_render(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    for state in [
        GitHubLoginState::Starting,
        GitHubLoginState::Awaiting {
            verification_uri: "https://github.com/login/device".to_owned(),
            user_code: "ABCD-EFGH".to_owned(),
            expires_in: 600,
        },
        GitHubLoginState::Success,
        GitHubLoginState::Error("Unable to connect".to_owned()),
    ] {
        render_scenario(cx, |view| view.github_login = Some(state));
    }
    render_scenario_at(cx, size(px(390.), px(844.)), |view| {
        view.session_drawer_open = true;
        view.sessions = vec![view.active_session.clone()];
    });
}

#[gpui_kit::test]
fn phone_drawer_and_review_sidebar_controls_toggle_panels(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("open-session-drawer", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let drawer = window.find("mobile-session-drawer");
        assert!(drawer.visible());
        let drawer_width = drawer.bounds().size.width;
        assert_eq!(
            drawer_width,
            window.bounds().size.width,
            "phone drawer should fill the screen"
        );
        let row = window
            .within("mobile-session-drawer")
            .find(("session-tree-root", 0usize));
        assert!(row.visible());
        assert!(
            row.bounds().size.height >= px(44.),
            "project row is below the touch target: {:?}",
            row.bounds()
        );
    })
    .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.session_drawer_open = false;
                cx.notify();
            });
        window.render_frame(cx);
        assert!(window.try_find("mobile-session-drawer").is_none());
        window.click("toggle-review-sidebar", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("close-inspector").visible());
        window.click("close-inspector", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("close-inspector").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn phone_session_drawer_close_button_dismisses_the_drawer(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.session_drawer_open = true;
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("mobile-session-drawer").visible());
        assert!(window.find("close-session-drawer").visible());
        window.click("close-session-drawer", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("mobile-session-drawer").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn phone_composer_and_settings_render_at_mobile_width(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("send-message").visible());
        assert!(window.find("open-command-palette").visible());
        // The send action shares the pickers' row instead of wrapping onto a
        // second, mostly empty row.
        let send = window.find("send-message").bounds();
        let palette = window.find("open-command-palette").bounds();
        assert!(
            send.top() < palette.bottom() && palette.top() < send.bottom(),
            "send button should sit on the composer action row: {send:?} vs {palette:?}"
        );
        assert!(send.right() <= window.viewport_size().width);
        // Phone composer controls use finger-sized targets.
        assert_eq!(send.size.height, px(44.));
        assert_eq!(palette.size.height, px(44.));
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.settings_open = true;
                cx.notify();
            });
        window.render_frame(cx);
        for index in 0..SETTINGS_SECTIONS.len() {
            window
                .within("settings-dialog")
                .click(("settings-section", index), cx);
            window.render_frame(cx);
            assert!(
                window
                    .within("settings-dialog")
                    .find(("settings-section", index))
                    .visible()
            );
            assert!(
                window
                    .within("settings-dialog")
                    .find("settings-content")
                    .visible()
            );
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn phone_header_keeps_actions_visible_with_a_long_session_name(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.active_session.name =
            "A very long project and session name that must not push the header actions away"
                .to_owned();
        view.sessions = vec![view.active_session.clone()];
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        let viewport = window.viewport_size();
        for id in ["session-sources", "toggle-review-sidebar"] {
            let action = window.find(id);
            assert!(
                action.visible(),
                "{id} should stay visible on a phone header"
            );
            assert!(
                action.bounds().right() <= viewport.width,
                "{id} ran off the right edge: {:?}",
                action.bounds()
            );
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn phone_composer_reserves_space_for_the_on_screen_keyboard(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view
    });
    let window_handle = handle.into();
    // The layout viewport stays 844px tall while the keyboard shrinks the
    // visual viewport to 544px, matching the browser's default resize policy.
    cx.simulate_window_visual_viewport_change(
        window_handle,
        gpui_kit::Bounds::new(gpui_kit::point(px(0.), px(0.)), size(px(390.), px(544.))),
    );
    cx.update_window(window_handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        let visible = window.fully_visible_bounds();
        let composer = window.find("composer-input-box").bounds();
        assert!(
            composer.bottom() <= visible.bottom(),
            "composer is hidden behind the keyboard: {composer:?} vs {visible:?}"
        );
        let send = window.find("send-message").bounds();
        assert!(
            send.bottom() <= visible.bottom(),
            "send action is hidden behind the keyboard: {send:?} vs {visible:?}"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
async fn session_creation_uses_worker_model_catalog_and_selects_the_created_session(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let workspace =
            crate::connection::create_workspace(&view.connection, "Session creation").unwrap();
        view.workspace_id = workspace.id;
        view.workspaces.push(workspace);
        view
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.create_session_on_node_with_source(
                    view.default_backend_node_id.clone(),
                    "Created session".to_owned(),
                    None,
                    cx,
                );
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window
            .root::<LoomView>()
            .flatten()
            .is_some_and(|view| view.read(cx).sessions.len() == 1)
    })
    .await;
}

#[gpui_kit::test]
async fn asynchronous_model_refresh_updates_the_active_worker_catalog(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.session_node_ids
            .insert(view.active_session.id, view.default_backend_node_id.clone());
        view
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.refresh_models_for_node_async(view.default_backend_node_id.clone(), cx);
                view.refresh_models_for_node_async("missing-node".to_owned(), cx);
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window.root::<LoomView>().flatten().is_some_and(|view| {
            let view = view.read(cx);
            view.model_catalog_node_id.as_deref() == Some("test-node")
                && !view.models.is_empty()
                && view.model_refreshes_in_flight.is_empty()
        })
    })
    .await;
}

#[gpui_kit::test]
async fn on_demand_model_refresh_is_throttled_by_staleness(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.session_node_ids
            .insert(view.active_session.id, view.default_backend_node_id.clone());
        view
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.refresh_models_on_demand(cx);
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window.root::<LoomView>().flatten().is_some_and(|view| {
            let view = view.read(cx);
            !view.models.is_empty()
                && view.model_refreshes_in_flight.is_empty()
                && view.model_catalog_refreshed_at.contains_key("test-node")
        })
    })
    .await;

    // Opening the model selection again inside the staleness window must
    // reuse the cached catalog instead of starting another discovery.
    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.refresh_models_on_demand(cx);
                assert!(view.model_refreshes_in_flight.is_empty());
            });
    })
    .unwrap();
}

#[gpui_kit::test]
async fn session_creation_attaches_a_local_source_before_selecting_it(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let source = std::env::temp_dir().join(format!("loom-ui-source-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("README.md"), "local source").unwrap();
    let source_path = source.to_string_lossy().to_string();
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let workspace =
            crate::connection::create_workspace(&view.connection, "Local source").unwrap();
        view.workspace_id = workspace.id;
        view.workspaces.push(workspace);
        view
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.create_session_on_node_with_source(
                    view.default_backend_node_id.clone(),
                    "Source session".to_owned(),
                    Some(super::SessionCreationSource::LocalDirectory(
                        source_path.clone(),
                    )),
                    cx,
                );
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window
            .root::<LoomView>()
            .flatten()
            .is_some_and(|view| view.read(cx).sessions.len() == 1)
    })
    .await;

    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.refresh_review(cx);
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window.root::<LoomView>().flatten().is_some_and(|view| {
            let view = view.read(cx);
            view.review.repositories_loaded && view.session_directories.len() == 1
        })
    })
    .await;

    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            let path = format!("{}/README.md", view.session_directories[0].path);
            view.open_review_file(path, cx);
        });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window.root::<LoomView>().flatten().is_some_and(|view| {
            view.read(cx)
                .review
                .selected_file
                .as_ref()
                .is_some_and(|file| file.content == "local source")
        })
    })
    .await;

    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.add_source_to_active_session(
                    super::SessionCreationSource::LocalDirectory(source_path.clone()),
                    cx,
                );
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window
            .root::<LoomView>()
            .flatten()
            .is_some_and(|view| view.read(cx).session_directories.len() == 2)
    })
    .await;

    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            assert!(
                view.session_directories[0]
                    .source
                    .contains("loom-ui-source-")
            );
            view.detach_session_directory(view.session_directories[0].path.clone(), cx);
        });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window
            .root::<LoomView>()
            .flatten()
            .is_some_and(|view| view.read(cx).session_directories.len() == 1)
    })
    .await;
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            view.detach_session_directory(view.session_directories[0].path.clone(), cx);
        });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window
            .root::<LoomView>()
            .flatten()
            .is_some_and(|view| view.read(cx).session_directories.is_empty())
    })
    .await;
    std::fs::remove_dir_all(source).unwrap();
}

#[gpui_kit::test]
async fn repository_review_loads_git_status_diff_and_detaches_the_repository(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let source = std::env::temp_dir().join(format!("loom-ui-repo-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("README.md"), "before\n").unwrap();
    for arguments in [
        vec!["init", "-q"],
        vec!["config", "user.email", "loom@example.test"],
        vec!["config", "user.name", "Loom Test"],
        vec!["add", "README.md"],
        vec!["commit", "-qm", "initial"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
    }

    let source_path = source.to_string_lossy().to_string();
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let workspace =
            crate::connection::create_workspace(&view.connection, "Repository review").unwrap();
        let session = crate::connection::create_session_in_workspace(
            &view.connection,
            workspace.id,
            "Review session",
        )
        .unwrap();
        let repository = crate::connection::attach_session_repository(
            &view.connection,
            session.id,
            &source_path,
            "repo",
        )
        .unwrap();
        let edit = view
            .connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::ApplySessionFilesystemEdit {
                    session_id: session.id,
                    edit: loom_protocol::WorkspaceEdit {
                        path: "repo/README.md".to_owned(),
                        old_text: "before".to_owned(),
                        new_text: "after".to_owned(),
                        expected_revision: None,
                    },
                },
            )));
        assert!(matches!(
            edit.result,
            Ok(ServerResponse::Filesystem(
                FilesystemResponse::WorkspaceEditApplied(_)
            ))
        ));
        view.workspace_id = workspace.id;
        view.workspaces.push(workspace);
        view.active_session = session.clone();
        view.sessions.push(session.clone());
        view.session_node_ids
            .insert(session.id, view.default_backend_node_id.clone());
        view.session_repositories.push(repository.clone());
        view.selected_repository_id = Some(repository.id);
        view
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                let repository_id = view.selected_repository_id.unwrap();
                view.select_session_repository(repository_id, cx);
                view.open_review_diff("README.md".to_owned(), false, cx);
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window.root::<LoomView>().flatten().is_some_and(|view| {
            let view = view.read(cx);
            view.review.vcs.is_some()
                && view
                    .review
                    .selected_diff
                    .as_ref()
                    .is_some_and(|diff| !diff.hunks.is_empty())
        })
    })
    .await;
    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.jump_review_hunk(true, cx);
                view.jump_review_hunk(false, cx);
                view.detach_session_repository(view.selected_repository_id.unwrap(), cx);
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window
            .root::<LoomView>()
            .flatten()
            .is_some_and(|view| view.read(cx).session_repositories.is_empty())
    })
    .await;
    std::fs::remove_dir_all(source).unwrap();
}

#[gpui_kit::test]
async fn confirming_an_existing_clone_creates_a_session_from_it(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let source = std::env::temp_dir().join(format!("loom-ui-cache-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("README.md"), "cached\n").unwrap();
    for arguments in [
        vec!["init", "-q"],
        vec!["config", "user.email", "loom@example.test"],
        vec!["config", "user.name", "Loom Test"],
        vec!["add", "README.md"],
        vec!["commit", "-qm", "initial"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
    }
    let source_path = source.to_string_lossy().to_string();
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let workspace =
            crate::connection::create_workspace(&view.connection, "Cached clone").unwrap();
        view.workspace_id = workspace.id;
        view.workspaces.push(workspace);
        view.session_node_ids
            .insert(view.active_session.id, view.default_backend_node_id.clone());
        view
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<LoomView>()
            .unwrap()
            .unwrap()
            .update(cx, |view, cx| {
                view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
                view.choose_source(SessionSourceChoice::GitHub, cx);
                // Offer a repository that is already cloned on the node and
                // confirm it, exercising the reuse path (no GitHub search).
                view.cloned_repositories = vec![ClonedRepository {
                    full_name: "owner/cached".to_owned(),
                    clone_url: source_path.clone(),
                    branch: Some("main".to_owned()),
                    last_used_at: Timestamp::from_unix_millis(1),
                }];
                if let Some(dialog) = &mut view.source_dialog {
                    dialog.selected_repository = Some("owner/cached".to_owned());
                }
                // A short filter is handled locally and never reaches the worker.
                view.on_repository_search_changed(cx);
                view.confirm_source_dialog(cx);
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(10), |window, cx| {
        window
            .root::<LoomView>()
            .flatten()
            .is_some_and(|view| view.read(cx).sessions.len() == 1)
    })
    .await;
    std::fs::remove_dir_all(&source).unwrap();
}

#[gpui_kit::test]
async fn github_source_selection_reports_unconfigured_provider_and_requires_a_repository(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.session_node_ids
                .insert(view.active_session.id, view.default_backend_node_id.clone());
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<gpui_kit::component::Root>()
            .unwrap()
            .unwrap()
            .update(cx, |root, cx| {
                root.view()
                    .clone()
                    .downcast::<LoomView>()
                    .unwrap()
                    .update(cx, |view, cx| {
                        view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
                        view.choose_source(SessionSourceChoice::GitHub, cx);
                        // Searching GitHub with no credentials configured must
                        // surface the provider error in the dialog.
                        view.search_github_repositories("loom".to_owned(), cx);
                    });
            });
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
        window
            .root::<gpui_kit::component::Root>()
            .flatten()
            .is_some_and(|root| {
                root.read(cx)
                    .view()
                    .clone()
                    .downcast::<LoomView>()
                    .ok()
                    .is_some_and(|view| {
                        view.read(cx).source_dialog.as_ref().is_some_and(|dialog| {
                            !dialog.repositories_loading && dialog.error.is_some()
                        })
                    })
            })
    })
    .await;
    cx.update_window(handle.into(), |_, window, cx| {
        window
            .root::<gpui_kit::component::Root>()
            .unwrap()
            .unwrap()
            .update(cx, |root, cx| {
                root.view()
                    .clone()
                    .downcast::<LoomView>()
                    .unwrap()
                    .update(cx, |view, cx| {
                        view.confirm_source_dialog(cx);
                        assert!(view.source_dialog.is_some());
                        assert!(
                            view.status_banner
                                .as_ref()
                                .is_some_and(|note| note.text == "Choose a GitHub repository")
                        );
                    });
            });
    })
    .unwrap();
}

#[gpui_kit::test]
fn rename_and_source_dialogs_render(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |view| {
        view.rename_dialog = Some(RenameDialogState {
            session: view.active_session.clone(),
            input: "Renamed session".to_owned(),
            is_project: true,
        });
    });
    render_scenario(cx, |view| {
        view.source_dialog = Some(SessionSourceDialog {
            purpose: SessionSourceDialogPurpose::StartSession,
            choice: SessionSourceChoice::Empty,
            local_directory_available: true,
            filter_subscription: None,
            repositories: Vec::new(),
            selected_repository: None,
            repositories_loading: false,
            error: None,
        });
    });
    render_scenario(cx, |view| {
        view.source_dialog = Some(SessionSourceDialog {
            purpose: SessionSourceDialogPurpose::AddToSession,
            choice: SessionSourceChoice::LocalDirectory,
            local_directory_available: true,
            filter_subscription: None,
            repositories: Vec::new(),
            selected_repository: None,
            repositories_loading: false,
            error: Some("directory does not exist".to_owned()),
        });
    });
    render_scenario(cx, |view| {
        view.source_dialog = Some(SessionSourceDialog {
            purpose: SessionSourceDialogPurpose::StartSession,
            choice: SessionSourceChoice::GitHub,
            local_directory_available: false,
            filter_subscription: None,
            repositories: vec![GitHubRepository {
                full_name: "owner/project".to_owned(),
                description: Some("example repository".to_owned()),
                clone_url: "https://github.com/owner/project.git".to_owned(),
                private: false,
                default_branch: "main".to_owned(),
            }],
            selected_repository: Some("owner/project".to_owned()),
            repositories_loading: false,
            error: None,
        });
    });
    render_scenario(cx, |view| {
        view.source_dialog = Some(SessionSourceDialog {
            purpose: SessionSourceDialogPurpose::StartSession,
            choice: SessionSourceChoice::GitHub,
            local_directory_available: true,
            filter_subscription: None,
            repositories: Vec::new(),
            selected_repository: None,
            repositories_loading: true,
            error: None,
        });
    });
    render_scenario(cx, |view| {
        view.source_dialog = Some(SessionSourceDialog {
            purpose: SessionSourceDialogPurpose::AddToSession,
            choice: SessionSourceChoice::GitHub,
            local_directory_available: false,
            filter_subscription: None,
            repositories: Vec::new(),
            selected_repository: None,
            repositories_loading: false,
            error: Some("GitHub authentication is required".to_owned()),
        });
    });
    render_scenario(cx, |view| {
        view.source_dialog = Some(SessionSourceDialog {
            purpose: SessionSourceDialogPurpose::StartSession,
            choice: SessionSourceChoice::GitHub,
            local_directory_available: true,
            filter_subscription: None,
            repositories: Vec::new(),
            selected_repository: None,
            repositories_loading: false,
            error: None,
        });
    });
    // A node with existing clones pre-selects one and renders the reuse action.
    render_scenario(cx, |view| {
        view.cloned_repositories = vec![ClonedRepository {
            full_name: "owner/project".to_owned(),
            clone_url: "https://github.com/owner/project.git".to_owned(),
            branch: Some("main".to_owned()),
            last_used_at: Timestamp::from_unix_millis(0),
        }];
        view.source_dialog = Some(SessionSourceDialog {
            purpose: SessionSourceDialogPurpose::StartSession,
            choice: SessionSourceChoice::GitHub,
            local_directory_available: true,
            filter_subscription: None,
            repositories: Vec::new(),
            selected_repository: Some("owner/project".to_owned()),
            repositories_loading: false,
            error: None,
        });
    });
}

#[gpui_kit::test]
fn review_panel_renders_workspace_and_git_changes(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.repositories_loaded = true;
        view.review.changes = vec![SessionFilesystemChange {
            sequence: loom_core::EventSequence::new(1),
            session_id: view.active_session.id,
            path: "src/new.rs".to_owned(),
            kind: WorkspaceChangeKind::Created,
            revision: Some("revision-1".to_owned()),
        }];
        view.review.vcs = Some(GitRepositoryStatus {
            root: "/workspace".to_owned(),
            branch: Some("main".to_owned()),
            head: Some("abc123".to_owned()),
            files: vec![GitFileStatus {
                path: "src/lib.rs".to_owned(),
                original_path: None,
                index: GitFileStatusKind::Modified,
                worktree: GitFileStatusKind::Modified,
                conflicted: false,
                index_additions: 1,
                index_deletions: 0,
                worktree_additions: 2,
                worktree_deletions: 1,
            }],
            conflicts: Vec::new(),
            clean: false,
            captured_at: Timestamp::from_unix_millis(0),
        });
        view.review.selected_path = Some("src/lib.rs".to_owned());
        view.review.selected_diff = Some(GitDiff {
            path: Some("src/lib.rs".to_owned()),
            staged: false,
            patch: String::new(),
            binary: false,
            hunks: vec![GitDiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 2,
                lines: vec![GitDiffLine {
                    kind: GitDiffLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                    content: "new line".to_owned(),
                }],
            }],
            truncated: false,
        });
        view.review.rows = vec![
            ReviewRow::Hunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 2,
            },
            ReviewRow::Line(GitDiffLine {
                kind: GitDiffLineKind::Added,
                old_line: None,
                new_line: Some(1),
                content: "new line".to_owned(),
            }),
        ];
        view.review.hunk_rows = vec![0];
    });
}

#[gpui_kit::test]
fn review_panel_renders_loading_file_and_binary_diff_states(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.repositories_loaded = false;
    });
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.repositories_loaded = true;
        view.review.selected_path = Some("README.md".to_owned());
        view.review.selected_file = Some(SessionFilesystemFile {
            session_id: view.active_session.id,
            path: "README.md".to_owned(),
            content: "Workspace file contents".to_owned(),
            revision: "revision-2".to_owned(),
        });
    });
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.repositories_loaded = true;
        view.review.selected_path = Some("assets/image.png".to_owned());
        view.review.selected_diff = Some(GitDiff {
            path: Some("assets/image.png".to_owned()),
            staged: false,
            patch: String::new(),
            binary: true,
            hunks: Vec::new(),
            truncated: false,
        });
    });
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.repositories_loaded = true;
        view.review.selected_path = Some("src/large.rs".to_owned());
        view.review.selected_diff = Some(GitDiff {
            path: Some("src/large.rs".to_owned()),
            staged: false,
            patch: String::new(),
            binary: false,
            hunks: Vec::new(),
            truncated: true,
        });
    });
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.repositories_loaded = true;
        view.session_repositories = vec![
            SessionRepository {
                id: loom_core::RepositoryId::new(),
                source: "https://github.com/owner/first".to_owned(),
                path: "/workspace/first".to_owned(),
                revision: None,
                attached_at: Timestamp::from_unix_millis(1),
            },
            SessionRepository {
                id: loom_core::RepositoryId::new(),
                source: "https://github.com/owner/second/".to_owned(),
                path: "/workspace/second".to_owned(),
                revision: None,
                attached_at: Timestamp::from_unix_millis(2),
            },
        ];
        view.selected_repository_id = view.session_repositories.first().map(|repo| repo.id);
        view.review.changes = vec![SessionFilesystemChange {
            sequence: loom_core::EventSequence::new(2),
            session_id: view.active_session.id,
            path: "notes/todo.md".to_owned(),
            kind: WorkspaceChangeKind::Modified,
            revision: None,
        }];
    });
}

#[gpui_kit::test]
fn inspector_renders_agent_context_and_files_tabs(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.tab = InspectorTab::Agent;
        view.active_run = Some(loom_protocol::AgentRunSnapshot {
            id: RunId::new(),
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id: view.active_session.id,
            task: "review the right pane".to_owned(),
            model: ModelId::new("deterministic/demo"),
            state: loom_protocol::AgentRunState::Executing,
            started_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(2),
            completed_at: None,
            summary: None,
            evidence: vec![loom_core::EvidenceLink {
                label: "pr".to_owned(),
                uri: "https://example.com/pr/1".to_owned(),
            }],
        });
        view.review.usage.session = Some(UsageSnapshot {
            input_tokens: 1_200,
            output_tokens: 400,
            cached_input_tokens: 100,
            tool_calls: 3,
            cost_micros: 12_500,
            elapsed_ms: 65_000,
        });
        view.review.usage.session_provider = Some(ProviderUsageSummary {
            requests: 2,
            cost_micros: 12_500,
            ..ProviderUsageSummary::default()
        });
        view.plan = Some(crate::state::PlanState {
            steps: vec!["Inspect".to_owned(), "Edit".to_owned()],
            completed: BTreeSet::from([0]),
            active: Some(1),
        });
        view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
            parts: vec![AssistantPart::Tool(Box::new(ToolPart {
                id: ToolCallId::new(),
                name: "read_file".to_owned(),
                title: "Read src/lib.rs".to_owned(),
                status: ToolPartStatus::Completed,
                detail: Some("src/lib.rs".to_owned()),
                output: Some("contents".to_owned()),
                elapsed_ms: Some(12),
                approval_pending: false,
            }))],
            streaming: false,
        })];
    });
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.tab = InspectorTab::Context;
        view.context_inspection = Some(ContextInspection {
            items: vec![
                ContextItem {
                    kind: ContextItemKind::Task,
                    label: "Task".to_owned(),
                    estimated_tokens: 10,
                    included: true,
                    omission_reason: None,
                },
                ContextItem {
                    kind: ContextItemKind::Conversation,
                    label: "Conversation".to_owned(),
                    estimated_tokens: 90,
                    included: false,
                    omission_reason: Some("over budget".to_owned()),
                },
            ],
            total_tokens: 100,
            included_tokens: 120,
            omitted_tokens: 90,
            budget: ContextBudget::new(Some(100), None, 50).unwrap(),
            compacted: true,
            summary: Some(ContextSummary {
                text: "Earlier turns were summarised.".to_owned(),
                source_message_count: 4,
                projection_version: 1,
                source_digest: "digest".to_owned(),
                created_at: Timestamp::from_unix_millis(3),
            }),
        });
        view.review.usage.run = Some(UsageSnapshot {
            input_tokens: 30,
            output_tokens: 10,
            ..UsageSnapshot::default()
        });
    });
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.tab = InspectorTab::Files;
        view.review.files.loaded = true;
        view.review.files.entries = vec![
            WorkspaceEntry {
                path: "src".to_owned(),
                kind: WorkspaceEntryKind::Directory,
                size: 0,
                modified_at: None,
                revision: "dir".to_owned(),
            },
            WorkspaceEntry {
                path: "src/main.rs".to_owned(),
                kind: WorkspaceEntryKind::File,
                size: 42,
                modified_at: None,
                revision: "rev".to_owned(),
            },
        ];
        view.review.files.selected_path = Some("src/main.rs".to_owned());
        view.review.files.selected_file = Some(SessionFilesystemFile {
            session_id: view.active_session.id,
            path: "src/main.rs".to_owned(),
            content: "fn main() {}\n".to_owned(),
            revision: "rev".to_owned(),
        });
    });
    render_scenario(cx, |view| {
        view.review.open = true;
        view.sessions = vec![view.active_session.clone()];
        view.review.repositories_loaded = true;
        view.review.wrap_lines = true;
        view.review.selected_path = Some("README.md".to_owned());
        view.review.selected_file = Some(SessionFilesystemFile {
            session_id: view.active_session.id,
            path: "README.md".to_owned(),
            content: "line one\nline two".to_owned(),
            revision: "rev".to_owned(),
        });
    });
}

#[gpui_kit::test]
fn run_usage_events_update_inspector_usage(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let run_id = RunId::new();
        view.consume_agent_event(&loom_protocol::AgentEvent::RunUsage {
            run_id,
            usage: loom_model::TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cached_input_tokens: 10,
            },
        });
        let run = view.review.usage.run.as_ref().unwrap();
        assert_eq!(run.input_tokens, 100);
        assert_eq!(run.output_tokens, 50);
        view.consume_agent_event(&loom_protocol::AgentEvent::RunUsageUpdated {
            run_id,
            usage: UsageSnapshot {
                input_tokens: 200,
                output_tokens: 60,
                cached_input_tokens: 10,
                tool_calls: 2,
                cost_micros: 5_000,
                elapsed_ms: 1_000,
            },
        });
        let run = view.review.usage.run.as_ref().unwrap();
        assert_eq!(run.tool_calls, 2);
        assert_eq!(run.cost_micros, 5_000);
    });
}

#[gpui_kit::test]
fn plan_progress_tracks_steps_and_resets_with_a_new_run(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let run_id = RunId::new();
        view.consume_agent_event(&loom_protocol::AgentEvent::PlanProposed {
            run_id,
            plan: loom_protocol::AgentPlan {
                steps: vec![
                    loom_protocol::AgentPlanStep {
                        id: "one".to_owned(),
                        description: "First".to_owned(),
                    },
                    loom_protocol::AgentPlanStep {
                        id: "two".to_owned(),
                        description: "Second".to_owned(),
                    },
                ],
            },
        });
        let plan = view.plan.as_ref().unwrap();
        assert_eq!(plan.total(), 2);
        assert_eq!(plan.done_count(), 0);

        view.consume_agent_event(&loom_protocol::AgentEvent::StepStarted {
            run_id,
            step_id: loom_core::StepId::new(),
            index: 1,
        });
        assert_eq!(view.plan.as_ref().unwrap().active_step(), Some("Second"));

        view.consume_agent_event(&loom_protocol::AgentEvent::StepCompleted {
            run_id,
            step_id: loom_core::StepId::new(),
            index: 1,
        });
        let plan = view.plan.as_ref().unwrap();
        assert_eq!(plan.done_count(), 1);
        assert_eq!(plan.active_step(), None);

        // A plan belongs to one run, so a new run starts without one.
        view.consume_agent_event(&loom_protocol::AgentEvent::RunStarted {
            snapshot: loom_protocol::AgentRunSnapshot {
                id: RunId::new(),
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: view.active_session.id,
                task: "next".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Planning,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(1),
                completed_at: None,
                summary: None,
                evidence: Vec::new(),
            },
        });
        assert!(view.plan.is_none());
    });
}

#[gpui_kit::test]
fn plan_renders_as_pinned_banner_and_inspector_summary(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.review.open = true;
        view.review.tab = InspectorTab::Agent;
        view.active_run_id = Some(RunId::new());
        view.plan = Some(crate::state::PlanState {
            steps: vec!["Inspect".to_owned(), "Edit".to_owned()],
            completed: BTreeSet::from([0]),
            active: Some(1),
        });
        view.timeline = vec![
            TimelineItem::User("do the work".to_owned()),
            TimelineItem::Assistant(AssistantTurn::text("working")),
        ];
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        assert!(window.find("plan-banner").visible());
        assert!(window.find(("plan-step", 0usize)).visible());
        assert!(window.find("run-plan-summary").visible());

        window.click("toggle-plan-banner", cx);
        window.render_frame(cx);
        view.update(cx, |view, _| assert!(view.plan_collapsed));
        assert!(window.find("plan-banner").visible());
        assert!(window.try_find(("plan-step", 0usize)).is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn transcript_renders_all_message_and_activity_variants(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let call = ToolCall {
        id: loom_core::ToolCallId::new(),
        name: "write_file".to_owned(),
        arguments: serde_json::json!({"path":"src/lib.rs"}),
    };
    let activity = AgentActivityRecord {
        id: ActivityId::new(),
        run_id: RunId::new(),
        timeline_ordinal: 0,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::File,
        status: AgentActivityStatus::Completed,
        started_at: Timestamp::from_unix_millis(0),
        completed_at: None,
        elapsed_ms: Some(1500),
        data: AgentActivityData::File {
            call: call.clone(),
            operation: FileActivityOperation::Write,
            path: Some("src/lib.rs".to_owned()),
            result: Some(ToolResult::success(&call, "updated file".to_owned())),
        },
    };
    render_scenario(cx, |view| {
        view.expanded_tools.insert(call.id);
        view.activity_records.insert(activity.id, activity);
        view.plan = Some(crate::state::PlanState {
            steps: vec!["Inspect".to_owned(), "Edit".to_owned()],
            completed: BTreeSet::from([0]),
            active: Some(1),
        });
        view.timeline = vec![
            TimelineItem::User("Please update the file".to_owned()),
            TimelineItem::Assistant(AssistantTurn {
                parts: vec![
                    AssistantPart::Reasoning("The file needs a small edit.".to_owned()),
                    AssistantPart::Text("# Done\nThe file is updated.".to_owned()),
                    AssistantPart::Tool(Box::new(ToolPart {
                        id: call.id,
                        name: "write_file".to_owned(),
                        title: "Edit src/lib.rs".to_owned(),
                        status: ToolPartStatus::Completed,
                        detail: Some("src/lib.rs".to_owned()),
                        output: Some("updated file".to_owned()),
                        elapsed_ms: Some(1500),
                        approval_pending: false,
                    })),
                    AssistantPart::Evidence(vec![EvidenceText {
                        label: "src/lib.rs".to_owned(),
                        uri: "https://example.com/diff".to_owned(),
                    }]),
                ],
                streaming: true,
            }),
            TimelineItem::System(SystemNote::status("Working".to_owned())),
            TimelineItem::System(SystemNote {
                tone: SystemTone::Error,
                heading: Some("save · persistence".to_owned()),
                text: "write failed".to_owned(),
                retryable: true,
            }),
            TimelineItem::System(SystemNote {
                tone: SystemTone::Input,
                heading: Some("Agent needs input".to_owned()),
                text: "Which branch should I use?".to_owned(),
                retryable: false,
            }),
        ];
        view.pending_input = Some("Which branch should I use?".to_owned());
        view.pending_approval = Some(call);
        view.model = ModelId::new("deterministic/demo");
    });
}

#[gpui_kit::test]
fn reasoning_visibility_follows_the_settings_toggle(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let key = super::tool_element_id(0, 0);

    for (show_reasoning, expected) in [(false, false), (true, true)] {
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                view.show_reasoning = show_reasoning;
                view.timeline = vec![TimelineItem::Assistant(AssistantTurn {
                    parts: vec![
                        AssistantPart::Reasoning("weighed the options".to_owned()),
                        AssistantPart::Text("the answer".to_owned()),
                    ],
                    streaming: false,
                })];
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.try_find(("reasoning", key)).is_some(),
                expected,
                "reasoning visibility should be {expected}"
            );
        })
        .unwrap();
    }
}
