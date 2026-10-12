//! Archived-session management: project grouping, deletion, and the
//! archive-retention policy, which the surface can edit on a worker that stores
//! one.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::Capability;
use loom_protocol::{ArchiveRetentionPolicy, SessionRequest, SessionResponse};

use crate::view::archive::{
    ArchivedSessionEntry, ArchivedSessionsState, PROJECT_DELETE_CANCELLATION,
    RETENTION_WINDOW_PRESETS, RetentionPolicyState, RetentionPolicyText, RetentionWindowChoice,
    archived_age_label, archived_session_groups, delete_archived_session_request, is_project_tree,
    project_deletion_consequence, project_row_consequence, retention_editable_state,
    retention_policy_state, retention_window_choice, retention_window_choices,
    retention_window_label,
};

/// An archived session whose archive timestamp is `archived_at`.
fn archived_session(id: AgentSessionId, name: &str, archived_at: u64) -> AgentSessionSnapshot {
    AgentSessionSnapshot {
        id,
        workspace_id: WorkspaceId::new(),
        name: name.to_owned(),
        state: AgentSessionState::Archived,
        created_at: Timestamp::from_unix_millis(archived_at),
        updated_at: Timestamp::from_unix_millis(archived_at),
    }
}

fn entry(session: AgentSessionSnapshot) -> ArchivedSessionEntry {
    ArchivedSessionEntry {
        node_id: "test-node".to_owned(),
        session,
    }
}

/// A project projection with `root` at depth 1 and every other id as a direct
/// child at depth 2.
fn project_snapshot(
    root: AgentSessionId,
    children: &[AgentSessionId],
) -> loom_core::ProjectSnapshot {
    let project_id = loom_core::ProjectId::from_uuid(*root.as_uuid());
    let agent = |session_id: AgentSessionId, depth: u8, parent: Option<AgentSessionId>| {
        loom_core::ProjectAgentRecord {
            session_id,
            project_id,
            parent_session_id: parent,
            depth,
            state: AgentSessionState::Archived,
            task_summary: None,
            output_cursor: EventSequence::default(),
            updated_at: Timestamp::from_unix_millis(1),
        }
    };
    loom_core::ProjectSnapshot {
        project_id,
        root_session_id: root,
        agents: std::iter::once(agent(root, 1, None))
            .chain(children.iter().map(|child| agent(*child, 2, Some(root))))
            .collect(),
        tasks: Vec::new(),
        worktrees: Vec::new(),
    }
}

#[test]
fn archived_sessions_list_one_row_per_project_with_the_tree_size() {
    let now = 1_700_000_000_000u64;
    let day = 86_400_000u64;
    let hour = 3_600_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let grandchild = AgentSessionId::new();
    let loose_new = AgentSessionId::new();
    let loose_old = AgentSessionId::new();
    // The projection also covers a session that is not archived, which is
    // deleted with the project even though it has no row of its own.
    let running = AgentSessionId::new();
    let entries = vec![
        entry(archived_session(root, "Checkout flow", now - 2 * day)),
        entry(archived_session(child, "Researcher", now - 3 * hour)),
        entry(archived_session(grandchild, "Analyst", now - 1_800_000)),
        entry(archived_session(loose_new, "Standalone", now - hour)),
        entry(archived_session(
            loose_old,
            "Old standalone",
            now - 604_800_000,
        )),
    ];
    let mut project = project_snapshot(root, &[child, grandchild, running]);
    // The grandchild hangs off the child, so the tree is three levels deep.
    for agent in &mut project.agents {
        if agent.session_id == grandchild {
            agent.depth = 3;
            agent.parent_session_id = Some(child);
        }
    }

    let groups = archived_session_groups(&entries, &[project], &BTreeMap::new(), now);

    assert_eq!(
        groups.len(),
        2,
        "one project group and one no-project group"
    );
    let project_group = &groups[0];
    assert_eq!(project_group.project_root, Some(root));
    assert_eq!(project_group.label, "Checkout flow");
    assert_eq!(
        project_group.rows.len(),
        1,
        "a project is listed as one row, never one row per sub-agent"
    );
    let row = &project_group.rows[0];
    assert_eq!(row.session_id, root);
    assert_eq!(row.label, "Checkout flow");
    assert!(row.is_project);
    // The newest archived member dates the row; the root was archived first.
    assert_eq!(row.age, "Archived 30m ago");
    // The deletion removes the whole tree, the running member included.
    assert_eq!(row.session_count - 1, 3, "three sub-agent sessions");
    assert_eq!(project_group.heading_detail(), "4 sessions in the tree");

    let loose_group = &groups[1];
    assert_eq!(loose_group.project_root, None);
    assert_eq!(loose_group.label, "No project");
    let loose = loose_group
        .rows
        .iter()
        .map(|row| (row.label.as_str(), row.is_project, row.session_count))
        .collect::<Vec<_>>();
    assert_eq!(
        loose,
        vec![("Standalone", false, 1), ("Old standalone", false, 1)]
    );
    assert_eq!(loose_group.rows[1].age, "Archived 1w ago");
    assert_eq!(loose_group.heading_detail(), "2 sessions archived");

    // A session with no project snapshot still groups under the trailing
    // heading, with no row of its own in any project.
    let without_projects = archived_session_groups(&entries, &[], &BTreeMap::new(), now);
    assert_eq!(without_projects.len(), 1);
    assert_eq!(without_projects[0].label, "No project");
    assert_eq!(without_projects[0].rows.len(), entries.len());
    assert!(without_projects[0].rows.iter().all(|row| !row.is_project));
}

#[test]
fn a_project_whose_archived_member_is_a_descendant_is_still_one_row() {
    let now = 1_700_000_000_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    // Only the descendant is archived; the root is not, so the row takes its
    // name from the sessions the client lists.
    let entries = vec![entry(archived_session(child, "Researcher", now))];
    let names = BTreeMap::from([(root, "Checkout flow".to_owned())]);

    let groups =
        archived_session_groups(&entries, &[project_snapshot(root, &[child])], &names, now);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].project_root, Some(root));
    assert_eq!(groups[0].label, "Checkout flow");
    assert_eq!(groups[0].rows.len(), 1);
    let row = &groups[0].rows[0];
    assert_eq!(
        row.session_id, root,
        "the row deletes the tree's root, not the archived descendant"
    );
    assert!(row.is_project);
    assert_eq!(row.session_count, 2);
    assert_eq!(row.age, "Archived just now");

    // With no name for the root anywhere, the row still names the tree by its
    // root session id rather than by the descendant.
    let groups = archived_session_groups(
        &entries,
        &[project_snapshot(root, &[child])],
        &BTreeMap::new(),
        now,
    );
    assert_eq!(
        groups[0].label,
        format!("Project {}", &root.to_string()[..8])
    );
    assert_eq!(groups[0].rows[0].session_id, root);
}

#[test]
fn a_session_that_only_covers_itself_is_not_a_project_row() {
    let now = 1_700_000_000_000u64;
    let lonely = AgentSessionId::new();
    let uncovered = AgentSessionId::new();
    let entries = vec![
        entry(archived_session(lonely, "Standalone", now)),
        entry(archived_session(uncovered, "No snapshot", now - 60_000)),
    ];

    // `GetProjectSnapshotForSession` answers with this one-member snapshot for
    // an ordinary session, which is not a project.
    let groups = archived_session_groups(
        &entries,
        &[project_snapshot(lonely, &[])],
        &BTreeMap::new(),
        now,
    );

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].project_root, None);
    assert_eq!(groups[0].label, "No project");
    let rows = groups[0]
        .rows
        .iter()
        .map(|row| (row.label.as_str(), row.session_id, row.is_project))
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        vec![
            ("Standalone", lonely, false),
            ("No snapshot", uncovered, false),
        ],
        "a session that only covers itself stays a plain row"
    );
}

#[test]
fn a_snapshot_is_a_project_when_it_owns_another_member_or_a_task() {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let project_id = loom_core::ProjectId::from_uuid(*root.as_uuid());
    let mut with_task = project_snapshot(root, &[]);
    with_task.tasks.push(loom_core::DelegatedTaskRecord {
        task_id: loom_core::TaskId::new(),
        project_id,
        requester_session_id: root,
        target_session_id: child,
        child_name: "Researcher".to_owned(),
        intent: "inspect the protocol".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: false,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Running,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(1),
    });

    assert!(is_project_tree(&project_snapshot(root, &[child])));
    assert!(is_project_tree(&with_task));
    assert!(!is_project_tree(&project_snapshot(root, &[])));
}

#[test]
fn project_and_plain_rows_describe_what_their_deletion_removes() {
    assert_eq!(
        project_row_consequence(4),
        "Deletes this project, 3 sub-agent sessions, and their history and worktrees (4 sessions in the tree)."
    );
    assert_eq!(
        project_row_consequence(2),
        "Deletes this project, 1 sub-agent session, and their history and worktrees (2 sessions in the tree)."
    );
    assert_eq!(
        project_row_consequence(1),
        "Deletes this project and its stored history."
    );
    let consequence = project_deletion_consequence("Checkout flow", 4);
    assert!(
        consequence.starts_with("Checkout flow is permanently removed"),
        "{consequence}"
    );
    assert!(
        consequence.contains("3 sub-agent sessions"),
        "{consequence}"
    );
    assert!(consequence.contains("worktrees"), "{consequence}");
    assert!(PROJECT_DELETE_CANCELLATION.contains("cancelled"));
}

#[test]
fn archive_age_labels_and_retention_windows_are_human_readable() {
    let now = 1_700_000_000_000u64;
    assert_eq!(archived_age_label(now, now), "Archived just now");
    assert_eq!(archived_age_label(now - 7_200_000, now), "Archived 2h ago");
    assert_eq!(
        retention_window_label(Duration::from_secs(30)),
        "under a minute"
    );
    assert_eq!(
        retention_window_label(Duration::from_secs(90 * 60)),
        "1 hour 30 minutes"
    );
    assert_eq!(
        retention_window_label(Duration::from_secs(7 * 86_400)),
        "7 days"
    );
    assert_eq!(
        retention_window_label(Duration::from_secs(36 * 3_600)),
        "1 day 12 hours"
    );
}

#[test]
fn retention_policy_text_covers_enabled_disabled_and_unsupported() {
    let enabled =
        RetentionPolicyText::of(&RetentionPolicyState::Available(ArchiveRetentionPolicy {
            retention_ms: Some(7 * 86_400_000),
            force_discard_worktrees: true,
        }));
    assert_eq!(enabled.headline, "Automatic deletion after 7 days");
    assert!(
        enabled
            .detail
            .contains("may discard dirty or locked worktrees")
    );

    let gentle =
        RetentionPolicyText::of(&RetentionPolicyState::Available(ArchiveRetentionPolicy {
            retention_ms: Some(86_400_000),
            force_discard_worktrees: false,
        }));
    assert_eq!(gentle.headline, "Automatic deletion after 1 day");
    assert!(
        gentle
            .detail
            .contains("skips worktrees that have changes or a lock")
    );

    let disabled = RetentionPolicyText::of(&RetentionPolicyState::Available(
        ArchiveRetentionPolicy::disabled(),
    ));
    assert_eq!(disabled.headline, "Automatic deletion is off");
    assert!(disabled.detail.contains("stay until someone deletes them"));

    // A worker that does not report a policy reads as unavailable, never as a
    // failure that hides the sessions.
    let unsupported = RetentionPolicyText::of(&RetentionPolicyState::Unavailable);
    assert!(unsupported.headline.contains("Unknown"));
    assert!(unsupported.detail.contains("does not report"));
}

#[test]
fn a_worker_without_a_retention_policy_reads_as_unavailable() {
    let policy = ArchiveRetentionPolicy {
        retention_ms: Some(3_600_000),
        force_discard_worktrees: false,
    };
    assert_eq!(
        retention_policy_state(Ok(ServerResponse::Control(
            ControlResponse::ArchiveRetentionPolicy(policy)
        ))),
        RetentionPolicyState::Available(policy)
    );
    // An older worker rejects the request, and one that never negotiated the
    // capability is refused; neither may block the surface.
    for code in [ErrorCode::InvalidRequest, ErrorCode::UnsupportedCapability] {
        assert_eq!(
            retention_policy_state(Err(LoomError::new(code, "no such request", false))),
            RetentionPolicyState::Unavailable
        );
    }
    assert_eq!(
        retention_policy_state(Ok(ServerResponse::Workspace(
            WorkspaceResponse::Workspaces { workspaces: vec![] }
        ))),
        RetentionPolicyState::Unavailable
    );
}

#[test]
fn retention_window_choices_offer_presets_and_a_custom_entry() {
    let preset = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: false,
    };
    let (choices, selected) = retention_window_choices(preset);
    assert_eq!(choices.len(), RETENTION_WINDOW_PRESETS.len());
    assert_eq!(choices.first().map(String::as_str), Some("Off"));
    assert_eq!(selected, "7 days");
    assert!(choices.contains(&selected));

    // Every label maps back to the window it stands for.
    assert_eq!(retention_window_choice("Off"), RetentionWindowChoice::Off);
    for window in RETENTION_WINDOW_PRESETS.iter().flatten() {
        assert_eq!(
            retention_window_choice(&retention_window_label(*window)),
            RetentionWindowChoice::Window(*window)
        );
    }

    // A stored window that matches no preset is offered as its own entry, so
    // nothing silently moves it to a preset.
    let custom = ArchiveRetentionPolicy {
        retention_ms: Some(37 * 60_000),
        force_discard_worktrees: true,
    };
    let (choices, selected) = retention_window_choices(custom);
    assert_eq!(choices.len(), RETENTION_WINDOW_PRESETS.len() + 1);
    assert_eq!(selected, "Custom (37 minutes)");
    assert_eq!(choices.last(), Some(&selected));
    assert_eq!(
        retention_window_choice(&selected),
        RetentionWindowChoice::Custom
    );
    assert_eq!(
        retention_window_choice("a window nobody offers"),
        RetentionWindowChoice::Custom
    );

    // A disabled policy selects the off entry rather than a window.
    let (_, selected) = retention_window_choices(ArchiveRetentionPolicy::disabled());
    assert_eq!(selected, "Off");
}

#[test]
fn retention_editability_follows_the_worker_capability() {
    // An answer that is not a worker-node status, and a failure, both leave the
    // card read-only rather than offering a control that cannot work.
    assert!(!retention_editable_state(Ok(ServerResponse::Workspace(
        WorkspaceResponse::Workspaces { workspaces: vec![] }
    ))));
    assert!(!retention_editable_state(Err(LoomError::new(
        ErrorCode::UnsupportedCapability,
        "no such request",
        false
    ))));
}

#[test]
fn delete_requests_carry_the_force_choice() {
    let session_id = AgentSessionId::new();
    for force in [false, true] {
        let request = delete_archived_session_request(session_id, force);
        assert!(
            matches!(
                request,
                ClientRequest::Session(SessionRequest::DeleteAgentSession {
                    session_id: requested,
                    force: requested_force,
                }) if requested == session_id && requested_force == force
            ),
            "a delete request must carry force={force}"
        );
    }
}

#[test]
fn archived_session_management_advertises_its_capabilities() {
    let capabilities = crate::connection::negotiation_capabilities();
    assert!(capabilities.contains(Capability::DeleteAgentSession));
    // The retention-policy request needs this capability.
    assert!(capabilities.contains(Capability::ReadWorkerNodeStatus));
}

/// Creates a workspace with one session on `connection` and archives it, then
/// returns the archived snapshot.
fn archived_session_on(
    connection: &ClientConnection,
    name: &str,
) -> (WorkspaceRecord, AgentSessionSnapshot) {
    use crate::connection::{create_session_in_workspace, create_workspace};

    let workspace = create_workspace(connection, name).expect("workspace");
    let session = create_session_in_workspace(connection, workspace.id, name).expect("session");
    let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::ArchiveAgentSession {
            session_id: session.id,
        },
    )));
    match archived.result {
        Ok(ServerResponse::Session(SessionResponse::AgentSessionArchived(snapshot))) => {
            (workspace, snapshot)
        }
        response => panic!("archiving the session failed: {response:?}"),
    }
}

/// The sessions one workspace lists, archived ones included.
fn listed_sessions(view: &LoomView, workspace_id: WorkspaceId) -> Vec<AgentSessionSnapshot> {
    let response = view
        .connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived: true,
            },
        )));
    match response.result {
        Ok(ServerResponse::Session(SessionResponse::AgentSessions { sessions })) => sessions,
        response => panic!("listing sessions failed: {response:?}"),
    }
}

/// A view whose default worker is the in-process backend `connection`.
fn view_over_connection(connection: &ClientConnection, cx: &mut Context<LoomView>) -> LoomView {
    let mut view = LoomView::new_for_test(cx.focus_handle());
    let node_id = view.default_backend_node_id.clone();
    view.connection = connection.clone();
    view.backend = BackendWorker::spawn(connection.clone());
    view.node_backends = BTreeMap::from([(node_id, view.backend.clone())]);
    view
}

#[gpui_kit::test]
fn deleting_without_force_reports_the_refusal_and_keeps_the_row(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let (workspace, archived) = archived_session_on(&view.connection, "old project");
        view.workspace_id = workspace.id;
        view.workspaces = vec![workspace];
        view.session_node_ids
            .insert(archived.id, view.default_backend_node_id.clone());
        view.active_session = archived.clone();
        view.sessions = vec![archived];
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    let session_id = cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.entries.len(), 1);
        assert!(view.archived_sessions.loaded);
        view.archived_sessions.entries[0].session.id
    });

    // The in-memory worker has no durable storage, so it does not advertise the
    // delete capability and refuses the request. The surface must show that
    // reason and leave the row and the session alone.
    view.update(cx, |view, cx| view.delete_archived_session(session_id, cx));
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            view.archived_sessions.delete_in_flight.is_none(),
            "the in-flight marker is cleared for either answer"
        );
        let refusal = view
            .archived_sessions
            .refusal
            .clone()
            .expect("a refused delete is shown in the surface");
        assert!(
            refusal.contains("DeleteAgentSession"),
            "the surface shows the worker's reason: {refusal}"
        );
        assert_eq!(
            view.archived_sessions.entries.len(),
            1,
            "a refused delete keeps the row"
        );
        assert!(
            listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == session_id),
            "the refused session is still stored"
        );
    });
}

#[gpui_kit::test]
fn loading_archived_sessions_reuses_a_project_snapshot_the_client_already_has(
    cx: &mut TestAppContext,
) {
    let root = AgentSessionId::new();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let (workspace, archived) = archived_session_on(&view.connection, "cached project");
        view.workspace_id = workspace.id;
        view.workspaces = vec![workspace];
        view.session_node_ids
            .insert(archived.id, view.default_backend_node_id.clone());
        // The client already holds this session's project projection. Its root
        // is not the session itself, so a re-fetch would replace the group with
        // the worker's own single-root projection.
        view.project_tree_snapshots = vec![project_snapshot(root, &[archived.id])];
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(
            view.archived_sessions.projects.len(),
            1,
            "a project snapshot the client already has is not fetched again"
        );
        let groups = archived_session_groups(
            &view.archived_sessions.entries,
            &view.archived_sessions.projects,
            &view.known_session_names(),
            Timestamp::now().as_unix_millis(),
        );
        assert_eq!(groups.len(), 1);
        assert!(groups[0].label.starts_with("Project "), "cached root label");
        assert_eq!(
            groups[0].rows.len(),
            1,
            "the cached project still lists one row"
        );
        assert_eq!(
            groups[0].rows[0].session_id, root,
            "the archived session groups under the cached project's root"
        );
        assert!(groups[0].rows[0].is_project);
        assert_eq!(groups[0].rows[0].session_count, 2);
    });
}

#[gpui_kit::test]
fn forcing_a_delete_removes_the_session_from_a_durable_worker(cx: &mut TestAppContext) {
    // A durable worker is required: an in-memory one refuses every delete.
    let state_path = std::env::temp_dir().join(format!(
        "loom-ui-archived-sessions-{}",
        uuid::Uuid::new_v4()
    ));
    let backend = loom_local::OwnedBackend::new_persistent_with_github_copilot(state_path)
        .expect("durable backend");
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&connection).unwrap();

    let view = cx.new(|cx| {
        let mut view = view_over_connection(&connection, cx);
        let (workspace, archived) = archived_session_on(&view.connection, "delete me");
        view.workspace_id = workspace.id;
        view.workspaces = vec![workspace];
        view.session_node_ids
            .insert(archived.id, view.default_backend_node_id.clone());
        view.active_session = archived.clone();
        view.sessions = vec![archived];
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    let session_id = cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.entries.len(), 1);
        // The active session is the archived one, so the surface shows the
        // retention policy the durable worker reports.
        assert!(
            matches!(
                view.archived_sessions.retention,
                RetentionPolicyState::Available(_)
            ),
            "a durable worker reports its retention policy"
        );
        view.archived_sessions.entries[0].session.id
    });

    // Without force the request still carries the row's choice, and with it the
    // backend deletes the session.
    let (node_id, request) = cx.update(|cx| {
        let view = view.read(cx);
        view.archived_session_delete_request(session_id)
            .expect("the row is loaded")
    });
    assert_eq!(
        node_id,
        cx.update(|cx| view.read(cx).default_backend_node_id.clone())
    );
    assert!(matches!(
        request,
        ClientRequest::Session(SessionRequest::DeleteAgentSession { force: false, .. })
    ));

    view.update(cx, |view, cx| {
        view.set_archived_session_force(session_id, true, cx);
    });
    let (_, forced) = cx.update(|cx| {
        view.read(cx)
            .archived_session_delete_request(session_id)
            .expect("the row is loaded")
    });
    assert!(matches!(
        forced,
        ClientRequest::Session(SessionRequest::DeleteAgentSession { force: true, .. })
    ));

    view.update(cx, |view, cx| view.delete_archived_session(session_id, cx));
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.archived_sessions.refusal.is_none());
        assert!(
            !view
                .archived_sessions
                .entries
                .iter()
                .any(|entry| entry.session.id == session_id),
            "the deleted row is gone from the surface"
        );
        assert!(
            !view.sessions.iter().any(|session| session.id == session_id),
            "the deleted session is gone from client state"
        );
        assert!(
            !listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == session_id),
            "the durable worker no longer stores the session"
        );
        assert_ne!(
            view.active_session.id, session_id,
            "the selection moves off the deleted session"
        );
    });
    backend.shutdown().ok();
}

#[gpui_kit::test]
fn deleting_a_project_root_cascades_descendants_out_of_client_state(cx: &mut TestAppContext) {
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let grandchild = AgentSessionId::new();
    let other = AgentSessionId::new();
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let node_id = view.default_backend_node_id.clone();
        let workspace_id = view.workspace_id;
        let session = move |id, name: &str| AgentSessionSnapshot {
            id,
            workspace_id,
            name: name.to_owned(),
            state: AgentSessionState::Archived,
            created_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(1),
        };
        view.sessions = vec![
            session(root, "Project"),
            session(child, "Researcher"),
            session(grandchild, "Analyst"),
            session(other, "Kept session"),
        ];
        for id in [root, child, grandchild, other] {
            view.session_node_ids.insert(id, node_id.clone());
        }
        let mut project = project_snapshot(root, &[child, grandchild]);
        for agent in &mut project.agents {
            if agent.session_id == grandchild {
                agent.depth = 3;
                agent.parent_session_id = Some(child);
            }
        }
        view.project_tree_snapshots = vec![project.clone()];
        view.project_snapshot = Some(project.clone());
        view.archived_sessions = ArchivedSessionsState {
            open: true,
            loaded: true,
            entries: vec![
                entry(session(root, "Project")),
                entry(session(child, "Researcher")),
                entry(session(grandchild, "Analyst")),
            ],
            projects: vec![project],
            force: BTreeSet::from([child]),
            ..ArchivedSessionsState::default()
        };
        view.active_session = session(grandchild, "Analyst");
        view.session_state = view.active_session.state;
        view
    });

    // The backend answered a delete of the project root.
    view.update(cx, |view, cx| {
        view.finish_archived_session_delete(
            root,
            ResponseEnvelope::success(
                RequestEnvelope::new(ClientRequest::Session(SessionRequest::DeleteAgentSession {
                    session_id: root,
                    force: false,
                }))
                .request_id,
                ServerResponse::Session(SessionResponse::AgentSessionDeleted { session_id: root }),
            ),
            cx,
        );
    });

    cx.update(|cx| {
        let view = view.read(cx);
        let names = view
            .sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["Kept session"],
            "a deleted project root takes its descendants with it"
        );
        assert!(
            view.project_tree_snapshots.is_empty(),
            "the deleted project's cached tree is dropped"
        );
        assert!(view.project_snapshot.is_none());
        assert!(view.archived_sessions.entries.is_empty());
        assert!(
            view.archived_sessions.force.is_empty(),
            "the force choice of a removed row is dropped"
        );
        assert!(view.archived_sessions.projects.is_empty());
        assert_eq!(
            view.active_session.id, other,
            "the selection moves to the session that survived"
        );
    });
}

#[gpui_kit::test]
fn archived_sessions_surface_renders_the_policy_and_the_rows(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let workspace_id = view.workspace_id;
            let archived_at = Timestamp::now().as_unix_millis() - 3 * 3_600_000;
            let session = move |id, name: &str| AgentSessionSnapshot {
                id,
                workspace_id,
                name: name.to_owned(),
                state: AgentSessionState::Archived,
                created_at: Timestamp::from_unix_millis(archived_at),
                updated_at: Timestamp::from_unix_millis(archived_at),
            };
            view.archived_sessions = ArchivedSessionsState {
                open: true,
                loaded: true,
                entries: vec![
                    entry(session(root, "Archived project")),
                    entry(session(child, "Archived child")),
                ],
                projects: vec![project_snapshot(root, &[child])],
                retention: RetentionPolicyState::Available(ArchiveRetentionPolicy {
                    retention_ms: Some(7 * 86_400_000),
                    force_discard_worktrees: true,
                }),
                ..ArchivedSessionsState::default()
            };
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archived-sessions-dialog").visible());
        assert!(window.find("archive-retention-policy").visible());
        assert!(window.find("archive-retention-detail").visible());
        // The project is one row: the archived child has none of its own.
        assert!(window.find(("archived-session-row", 0usize)).visible());
        assert!(window.try_find(("archived-session-row", 1usize)).is_none());
        assert_eq!(
            window.find(("archived-session-delete", 0usize)).label(),
            Some("Delete project: Archived project")
        );
        assert!(window.find(("archived-session-force", 0usize)).visible());
    })
    .unwrap();
}

/// The retention policy the connection's worker reports.
fn reported_retention_policy(connection: &ClientConnection) -> ArchiveRetentionPolicy {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::GetArchiveRetentionPolicy,
    )));
    match response.result {
        Ok(ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(policy))) => policy,
        response => panic!("reading the retention policy failed: {response:?}"),
    }
}

#[gpui_kit::test]
fn applying_a_window_stores_it_and_the_card_shows_the_stored_policy(cx: &mut TestAppContext) {
    // A durable worker is required: an in-memory one refuses every change.
    let state_path = std::env::temp_dir().join(format!(
        "loom-ui-archive-retention-{}",
        uuid::Uuid::new_v4()
    ));
    let backend = loom_local::OwnedBackend::new_persistent_with_github_copilot(state_path)
        .expect("durable backend");
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&connection).unwrap();

    let view = cx.new(|cx| {
        let mut view = view_over_connection(&connection, cx);
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    let reported = cx.update(|cx| {
        let view = view.read(cx);
        let RetentionPolicyState::Available(policy) = view.archived_sessions.retention else {
            panic!("a durable worker reports its retention policy");
        };
        assert!(
            view.archived_sessions.retention_editable,
            "a worker with durable storage can store a change"
        );
        assert_eq!(view.archived_sessions.retention_draft, Some(policy));
        assert!(
            !view.archive_retention_draft_is_dirty(),
            "the untouched draft is the policy the worker reported"
        );
        policy
    });

    // Nothing is submitted while the draft still holds the stored policy.
    view.update(cx, |view, cx| view.save_archive_retention_policy(cx));
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            !view.archived_sessions.retention_saving,
            "an unchanged draft is not submitted"
        );
        assert!(!view.archived_sessions.retention_saved);
    });

    // The user picks a preset and turns the force-discard switch on.
    let (choices, selected) = retention_window_choices(reported);
    view.update(cx, |view, cx| {
        view.choose_archive_retention_window("7 days", cx);
        view.set_archive_retention_force(true, cx);
    });
    let expected = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: true,
    };
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(choices.contains(&selected), "the presets are on offer");
        assert_eq!(view.archived_sessions.retention_draft, Some(expected));
        assert!(view.archive_retention_draft_is_dirty());
    });

    view.update(cx, |view, cx| view.save_archive_retention_policy(cx));
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(!view.archived_sessions.retention_saving);
        assert!(view.archived_sessions.retention_saved);
        assert!(view.archived_sessions.retention_refusal.is_none());
        // The card adopts the answer, so what it shows is what the worker
        // stored rather than what was asked for.
        assert_eq!(
            view.archived_sessions.retention,
            RetentionPolicyState::Available(expected)
        );
        assert_eq!(view.archived_sessions.retention_draft, Some(expected));
        assert!(!view.archive_retention_draft_is_dirty());
        let (choices, selected) = retention_window_choices(expected);
        assert_eq!(selected, "7 days");
        assert!(choices.contains(&selected));
    });
    assert_eq!(
        reported_retention_policy(&connection),
        expected,
        "the worker stored the policy the card was asked to apply"
    );
    backend.shutdown().ok();
}

#[gpui_kit::test]
fn a_refused_window_change_shows_the_reason_and_keeps_the_stored_policy(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    let reported = cx.update(|cx| {
        let view = view.read(cx);
        let RetentionPolicyState::Available(policy) = view.archived_sessions.retention else {
            panic!("the in-memory worker reports a policy");
        };
        assert!(
            !view.archived_sessions.retention_editable,
            "an in-memory worker cannot store a change"
        );
        policy
    });

    // A worker that offered the editor and then refused the change: the surface
    // shows the backend's own reason and leaves the stored policy alone. The
    // draft stays as the user left it, so the change can be retried.
    let requested = ArchiveRetentionPolicy {
        retention_ms: Some(30 * 86_400_000),
        force_discard_worktrees: true,
    };
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_editable = true;
        view.archived_sessions.retention_draft = Some(requested);
        view.save_archive_retention_policy(cx);
    });
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(!view.archived_sessions.retention_saving);
        assert!(!view.archived_sessions.retention_saved);
        let refusal = view
            .archived_sessions
            .retention_refusal
            .clone()
            .expect("a refused change is shown in the surface");
        assert!(
            refusal.contains("ConfigureArchiveRetention"),
            "the surface shows the worker's reason: {refusal}"
        );
        assert_eq!(
            view.archived_sessions.retention,
            RetentionPolicyState::Available(reported),
            "a refused change leaves the previous policy in place"
        );
        assert_eq!(
            view.archived_sessions.retention_draft,
            Some(requested),
            "the draft survives a refusal"
        );
    });
    let connection = cx.update(|cx| view.read(cx).connection.clone());
    assert_eq!(
        reported_retention_policy(&connection),
        reported,
        "a refused change is not stored"
    );
}

#[gpui_kit::test]
fn an_unexpected_answer_to_a_policy_change_is_reported(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.archived_sessions.open = true;
        view.archived_sessions.loaded = true;
        view.archived_sessions.retention_editable = true;
        view.archived_sessions.retention_draft = Some(ArchiveRetentionPolicy {
            retention_ms: Some(86_400_000),
            force_discard_worktrees: false,
        });
        view
    });
    let request = RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::SetArchiveRetentionPolicy {
            policy: ArchiveRetentionPolicy {
                retention_ms: Some(86_400_000),
                force_discard_worktrees: false,
            },
        },
    ));
    view.update(cx, |view, cx| {
        view.finish_archive_retention_save(
            ResponseEnvelope::success(
                request.request_id,
                ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces: vec![] }),
            ),
            cx,
        );
    });
    cx.update(|cx| {
        let view = view.read(cx);
        let refusal = view
            .archived_sessions
            .retention_refusal
            .clone()
            .expect("an unexpected answer is reported");
        assert!(refusal.contains("unexpected"), "{refusal}");
        assert!(
            !view.archived_sessions.retention_saved,
            "an unexpected answer is no confirmation"
        );
        assert!(
            view.archived_sessions.retention_draft.is_some(),
            "the draft survives an unexpected answer"
        );
    });
}

#[gpui_kit::test]
fn choosing_a_window_edits_the_draft_without_touching_a_custom_window(cx: &mut TestAppContext) {
    let stored = ArchiveRetentionPolicy {
        retention_ms: Some(37 * 60_000),
        force_discard_worktrees: false,
    };
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.archived_sessions = ArchivedSessionsState {
            open: true,
            loaded: true,
            retention: RetentionPolicyState::Available(stored),
            retention_editable: true,
            retention_draft: Some(stored),
            ..ArchivedSessionsState::default()
        };
        view
    });

    let custom = "Custom (37 minutes)".to_owned();
    let (choices, selected) = retention_window_choices(stored);
    assert_eq!(selected, custom);
    assert!(choices.contains(&custom));

    // The custom entry means the window the worker stored: choosing it keeps
    // that window instead of moving it to a preset.
    view.update(cx, |view, cx| {
        view.choose_archive_retention_window(&custom, cx);
        // The switch ignores a value it already has.
        view.set_archive_retention_force(false, cx);
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.retention_draft, Some(stored));
        assert!(!view.archive_retention_draft_is_dirty());
    });

    // A preset replaces the custom window, and Off clears it.
    view.update(cx, |view, cx| {
        view.choose_archive_retention_window("7 days", cx)
    });
    let seven_days = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        ..stored
    };
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.retention_draft, Some(seven_days));
        assert!(view.archive_retention_draft_is_dirty());
    });

    // Choosing the window the draft already holds changes nothing.
    view.update(cx, |view, cx| {
        view.choose_archive_retention_window("7 days", cx)
    });
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).archived_sessions.retention_draft,
            Some(seven_days)
        );
    });
    view.update(cx, |view, cx| {
        view.set_archive_retention_force(true, cx);
        view.choose_archive_retention_window("Off", cx);
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(
            view.archived_sessions.retention_draft,
            Some(ArchiveRetentionPolicy {
                retention_ms: None,
                force_discard_worktrees: true,
            })
        );
        assert!(view.archive_retention_draft_is_dirty());
    });

    // A read-only card has no draft, so a choice and a save change nothing and
    // the draft checks read as clean.
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_draft = None;
        view.archived_sessions.retention_editable = false;
        view.choose_archive_retention_window("Off", cx);
        view.set_archive_retention_force(false, cx);
        view.save_archive_retention_policy(cx);
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.archived_sessions.retention_draft.is_none());
        assert!(!view.archive_retention_draft_is_dirty());
        assert!(!view.archived_sessions.retention_saving);
        assert_eq!(
            view.archived_sessions.retention,
            RetentionPolicyState::Available(stored),
            "a read-only card leaves the reported policy alone"
        );
    });

    // A change is ignored while another one is in flight.
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_draft = Some(seven_days);
        view.archived_sessions.retention_saving = true;
        view.save_archive_retention_policy(cx);
        assert!(
            view.archived_sessions.retention_saving,
            "a save in flight is not started again"
        );
        view.archived_sessions.retention_saving = false;
    });
}

#[gpui_kit::test]
fn loading_a_worker_that_reports_no_policy_keeps_the_card_read_only(cx: &mut TestAppContext) {
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));

    // A worker that reports no policy has nothing to edit, even though it would
    // accept a change.
    view.update(cx, |view, cx| {
        view.finish_archived_sessions_load(
            Vec::new(),
            Vec::new(),
            RetentionPolicyState::Unavailable,
            true,
            Vec::new(),
            cx,
        );
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(matches!(
            view.archived_sessions.retention,
            RetentionPolicyState::Unavailable
        ));
        assert!(!view.archived_sessions.retention_editable);
        assert!(view.archived_sessions.retention_draft.is_none());
        assert!(!view.archived_sessions.retention_saving);
        assert!(!view.archived_sessions.retention_saved);
        assert!(view.archived_sessions.retention_refusal.is_none());
    });
}

#[gpui_kit::test]
fn a_worker_without_durable_storage_loads_the_policy_read_only(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.open_archived_sessions(cx);
        view
    });
    cx.run_until_parked();

    // The in-memory worker reports a policy it cannot change, so the card holds
    // no draft and no request is made for one.
    view.update(cx, |view, cx| view.save_archive_retention_policy(cx));
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            matches!(
                view.archived_sessions.retention,
                RetentionPolicyState::Available(_)
            ),
            "the in-memory worker reports a policy"
        );
        assert!(!view.archived_sessions.retention_editable);
        assert!(view.archived_sessions.retention_draft.is_none());
        assert!(!view.archived_sessions.retention_saving);
        assert!(view.archived_sessions.retention_refusal.is_none());
    });
}

/// Opens the archived-sessions dialog with `state` in place.
fn open_retention_card(
    cx: &mut TestAppContext,
    state: ArchivedSessionsState,
) -> (gpui_kit::AnyWindowHandle, Entity<LoomView>) {
    let mut built = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.node_names.insert(
                view.default_backend_node_id.clone(),
                "local worker".to_owned(),
            );
            view.archived_sessions = state;
            view
        });
        built = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = built.expect("the window built the archived-sessions view");
    (handle.into(), view)
}

/// Reports whether a frame of the dialog drew the read-only retention text and
/// the editor's window Select.
fn retention_card_elements(
    cx: &mut TestAppContext,
    handle: gpui_kit::AnyWindowHandle,
) -> (bool, bool) {
    let mut read_only = false;
    let mut editor = false;
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archived-sessions-dialog").visible());
        read_only = window.try_find("archive-retention-policy").is_some();
        editor = window
            .try_find(("archive-retention-window", 0usize))
            .is_some();
    })
    .unwrap();
    (read_only, editor)
}

#[gpui_kit::test]
fn a_worker_that_cannot_change_the_policy_renders_the_card_read_only(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let reported = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: false,
    };
    // A worker that reports a policy but cannot store a change explains itself
    // instead of offering controls.
    let (handle, _) = open_retention_card(
        cx,
        ArchivedSessionsState {
            open: true,
            loaded: true,
            retention: RetentionPolicyState::Available(reported),
            ..ArchivedSessionsState::default()
        },
    );
    let (read_only, editor) = retention_card_elements(cx, handle);
    assert!(read_only);
    assert!(!editor);

    // A worker that reports no policy at all stays read-only as well, even when
    // a draft was left behind.
    let (handle, _) = open_retention_card(
        cx,
        ArchivedSessionsState {
            open: true,
            loaded: true,
            retention: RetentionPolicyState::Unavailable,
            retention_editable: true,
            retention_draft: Some(reported),
            ..ArchivedSessionsState::default()
        },
    );
    let (read_only, editor) = retention_card_elements(cx, handle);
    assert!(read_only);
    assert!(!editor);
}

#[gpui_kit::test]
fn confirming_a_window_in_the_card_edits_the_draft_and_apply_submits_it(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let stored = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: false,
    };
    let (handle, view) = open_retention_card(
        cx,
        ArchivedSessionsState {
            open: true,
            loaded: true,
            retention: RetentionPolicyState::Available(stored),
            retention_editable: true,
            retention_draft: Some(stored),
            ..ArchivedSessionsState::default()
        },
    );

    // Down from the stored window confirms the next preset, and the control
    // reports that choice to the view.
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("archive-retention-window", 0usize)).value(),
            Some("7 days")
        );
        window.click(("archive-retention-window", 0usize), cx);
        window.press("down", cx);
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let chosen = cx.update(|cx| {
        let view = view.read(cx);
        let draft = view
            .archived_sessions
            .retention_draft
            .expect("the card holds a draft");
        assert_eq!(draft.retention_ms, Some(14 * 86_400_000));
        assert!(view.archive_retention_draft_is_dirty());
        draft
    });

    // Applying submits it to the active backend, which here is an in-memory
    // worker: the surface shows its reason and keeps the stored policy.
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("archive-retention-pending").visible());
        window.click("apply-archive-retention", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(!view.archived_sessions.retention_saving);
        assert_eq!(
            view.archived_sessions.retention,
            RetentionPolicyState::Available(stored),
            "a refused apply leaves the stored policy in place"
        );
        assert_eq!(view.archived_sessions.retention_draft, Some(chosen));
        let refusal = view
            .archived_sessions
            .retention_refusal
            .clone()
            .expect("the worker's reason is shown");
        assert!(refusal.contains("ConfigureArchiveRetention"), "{refusal}");
    });
}

#[gpui_kit::test]
fn the_retention_card_renders_the_editor_for_a_worker_that_stores_a_policy(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let stored = ArchiveRetentionPolicy {
        retention_ms: Some(37 * 60_000),
        force_discard_worktrees: false,
    };
    let (handle, view) = open_retention_card(
        cx,
        ArchivedSessionsState {
            open: true,
            loaded: true,
            retention: RetentionPolicyState::Available(stored),
            retention_editable: true,
            retention_draft: Some(stored),
            ..ArchivedSessionsState::default()
        },
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archived-sessions-dialog").visible());
        // The stored window matches no preset, so the control shows it as it is.
        assert_eq!(
            window.find(("archive-retention-window", 0usize)).value(),
            Some("Custom (37 minutes)")
        );
        assert_eq!(
            window.find("archive-retention-force").checked(),
            Some(false)
        );
        assert!(window.find("apply-archive-retention").visible());
        assert!(window.find("archive-retention-worker").visible());
        assert!(window.find("archive-retention-current").visible());
        assert!(window.try_find("archive-retention-policy").is_none());
        assert!(window.try_find("archive-retention-read-only").is_none());
    })
    .unwrap();

    // A draft that differs from the stored policy, a save in flight, and a save
    // the worker confirmed each render their own status line.
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_draft = Some(ArchiveRetentionPolicy {
            retention_ms: Some(7 * 86_400_000),
            force_discard_worktrees: true,
        });
        cx.notify();
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archive-retention-pending").visible());
        assert!(window.find("apply-archive-retention").visible());
    })
    .unwrap();
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_saving = true;
        cx.notify();
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archive-retention-saving").visible());
    })
    .unwrap();
    let saved = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: true,
    };
    view.update(cx, |view, cx| {
        view.finish_archive_retention_save(
            ResponseEnvelope::success(
                RequestEnvelope::new(ClientRequest::Control(
                    ControlRequest::SetArchiveRetentionPolicy { policy: saved },
                ))
                .request_id,
                ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(saved)),
            ),
            cx,
        );
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archive-retention-saved").visible());
        assert_eq!(
            window.find(("archive-retention-window", 0usize)).value(),
            Some("7 days")
        );
        assert_eq!(window.find("archive-retention-force").checked(), Some(true));
    })
    .unwrap();

    // A refused change keeps the card usable and shows the backend's reason.
    view.update(cx, |view, cx| {
        view.archived_sessions.retention_refusal = Some("no durable storage".to_owned());
        cx.notify();
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("archive-retention-refusal").visible());
        assert!(window.find("apply-archive-retention").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn toggling_the_force_switch_in_the_card_edits_the_draft(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let stored = ArchiveRetentionPolicy {
        retention_ms: Some(7 * 86_400_000),
        force_discard_worktrees: false,
    };
    let (handle, view) = open_retention_card(
        cx,
        ArchivedSessionsState {
            open: true,
            loaded: true,
            retention: RetentionPolicyState::Available(stored),
            retention_editable: true,
            retention_draft: Some(stored),
            ..ArchivedSessionsState::default()
        },
    );

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("archive-retention-force").checked(),
            Some(false)
        );
        window.click("archive-retention-force", cx);
        window.render_frame(cx);
        assert_eq!(window.find("archive-retention-force").checked(), Some(true));
        assert!(window.find("archive-retention-pending").visible());
    })
    .unwrap();

    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(
            view.archived_sessions.retention_draft,
            Some(ArchiveRetentionPolicy {
                force_discard_worktrees: true,
                ..stored
            }),
            "the switch reports its change to the card's draft"
        );
        assert!(view.archive_retention_draft_is_dirty());
    });
}

/// Creates one workspace and `names.len()` sessions in it, archiving each.
fn archived_sessions_in_one_workspace(
    connection: &ClientConnection,
    names: &[&str],
) -> (WorkspaceRecord, Vec<AgentSessionSnapshot>) {
    use crate::connection::{create_session_in_workspace, create_workspace};

    let workspace = create_workspace(connection, names.first().copied().unwrap_or("project"))
        .expect("workspace");
    let sessions = names
        .iter()
        .map(|name| {
            let session =
                create_session_in_workspace(connection, workspace.id, name).expect("session");
            let archived = connection.request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::ArchiveAgentSession {
                    session_id: session.id,
                },
            )));
            match archived.result {
                Ok(ServerResponse::Session(SessionResponse::AgentSessionArchived(snapshot))) => {
                    snapshot
                }
                response => panic!("archiving {name} failed: {response:?}"),
            }
        })
        .collect::<Vec<_>>();
    (workspace, sessions)
}

/// A view over `connection` whose archived listing holds `sessions` as one
/// project tree rooted at `sessions[0]`, with the client's own session list.
fn view_with_an_archived_project(
    view: &mut LoomView,
    workspace: WorkspaceRecord,
    sessions: &[AgentSessionSnapshot],
) {
    let node_id = view.default_backend_node_id.clone();
    view.workspace_id = workspace.id;
    view.workspaces = vec![workspace];
    for session in sessions {
        view.session_node_ids.insert(session.id, node_id.clone());
    }
    view.sessions = sessions.to_vec();
    view.active_session = sessions
        .get(1)
        .cloned()
        .unwrap_or_else(|| sessions[0].clone());
    view.session_state = view.active_session.state;
    view.archived_sessions = ArchivedSessionsState {
        open: true,
        loaded: true,
        entries: sessions.iter().cloned().map(entry).collect(),
        projects: vec![project_snapshot(
            sessions[0].id,
            &sessions[1..]
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
        )],
        ..ArchivedSessionsState::default()
    };
}

#[gpui_kit::test]
fn a_project_confirmation_submits_nothing_until_delete(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        let (workspace, sessions) =
            archived_sessions_in_one_workspace(&view.connection, &["Checkout flow", "Researcher"]);
        view_with_an_archived_project(&mut view, workspace, &sessions);
        view
    });
    cx.run_until_parked();

    let (root_id, child_id) = cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.archived_sessions.entries.len(), 2);
        (
            view.archived_sessions.entries[0].session.id,
            view.archived_sessions.entries[1].session.id,
        )
    });

    // The row's own force choice is what the confirmation starts from, and
    // opening it submits nothing.
    view.update(cx, |view, cx| {
        view.set_archived_session_force(root_id, true, cx);
        view.begin_project_delete(root_id, cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        let dialog = view
            .project_delete_dialog
            .as_ref()
            .expect("the confirmation is open");
        assert_eq!(dialog.session_id, root_id);
        assert_eq!(dialog.name, "Checkout flow");
        assert_eq!(dialog.session_count, 2, "the whole tree, not just the row");
        assert_eq!(dialog.node_id, view.default_backend_node_id);
        assert!(
            dialog.force,
            "the confirmation starts from the row's choice"
        );
        assert!(!dialog.in_flight);
        assert!(dialog.refusal.is_none());
        assert!(
            view.archived_sessions.delete_in_flight.is_none(),
            "opening the confirmation deletes nothing"
        );
        assert!(view.archived_sessions.refusal.is_none());
    });

    // Cancel closes it without a request, and the worker still holds the tree.
    view.update(cx, |view, cx| view.cancel_project_delete(cx));
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(view.project_delete_dialog.is_none());
        assert!(view.archived_sessions.refusal.is_none());
        assert!(view.sessions.iter().any(|session| session.id == root_id));
        assert!(
            listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == root_id),
            "a cancelled confirmation leaves the project stored"
        );
    });

    // Delete submits the request the confirmation holds to the worker that owns
    // the tree. The in-memory worker receives it and refuses it, which the
    // dialog keeps as the reason for a retry.
    view.update(cx, |view, cx| view.begin_project_delete(root_id, cx));
    let (node_id, request) = cx.update(|cx| {
        view.read(cx)
            .project_delete_request()
            .expect("the confirmation is open")
    });
    assert_eq!(
        node_id,
        cx.update(|cx| view.read(cx).default_backend_node_id.clone())
    );
    assert!(
        matches!(
            request,
            ClientRequest::Session(SessionRequest::DeleteAgentSession {
                session_id,
                force: true,
            }) if session_id == root_id
        ),
        "the confirmed delete carries the row's force choice"
    );

    view.update(cx, |view, cx| view.confirm_project_delete(cx));
    cx.run_until_parked();
    cx.update(|cx| {
        let view = view.read(cx);
        let dialog = view
            .project_delete_dialog
            .as_ref()
            .expect("a refused deletion keeps the confirmation open");
        assert!(!dialog.in_flight, "the request is no longer in flight");
        let refusal = dialog
            .refusal
            .clone()
            .expect("the dialog shows the backend's reason");
        assert!(
            refusal.contains("DeleteAgentSession"),
            "the worker received the delete it refused: {refusal}"
        );
        assert!(
            view.sessions.iter().any(|session| session.id == root_id)
                && view.sessions.iter().any(|session| session.id == child_id),
            "a refused deletion keeps the tree"
        );
        assert!(view.archived_sessions.refusal.is_none());
    });
}

#[gpui_kit::test]
fn a_confirmed_project_deletion_removes_the_tree_and_closes_the_dialog(cx: &mut TestAppContext) {
    // A durable worker is required: an in-memory one refuses every delete.
    let state_path =
        std::env::temp_dir().join(format!("loom-ui-project-deletion-{}", uuid::Uuid::new_v4()));
    let backend = loom_local::OwnedBackend::new_persistent_with_github_copilot(state_path)
        .expect("durable backend");
    let connection = ClientConnection::InProcess(Box::new(backend.connect()));
    crate::connection::negotiate(&connection).unwrap();

    let view = cx.new(|cx| {
        let mut view = view_over_connection(&connection, cx);
        let (workspace, sessions) = archived_sessions_in_one_workspace(
            &view.connection,
            &["Checkout flow", "Researcher", "Standalone"],
        );
        view_with_an_archived_project(&mut view, workspace, &sessions);
        // The third session is not part of the project.
        view.archived_sessions.projects = vec![project_snapshot(sessions[0].id, &[sessions[1].id])];
        view
    });
    cx.run_until_parked();

    let (root_id, child_id, kept_id) = cx.update(|cx| {
        let view = view.read(cx);
        let ids = view
            .archived_sessions
            .entries
            .iter()
            .map(|entry| entry.session.id)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3);
        (ids[0], ids[1], ids[2])
    });

    view.update(cx, |view, cx| {
        view.begin_project_delete(root_id, cx);
        view.set_project_delete_force(true, cx);
        view.confirm_project_delete(cx);
    });
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            view.project_delete_dialog.is_none(),
            "success closes the confirmation"
        );
        let names = view
            .sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["Standalone"],
            "the whole tree leaves the sidebar state"
        );
        assert_eq!(
            view.active_session.id, kept_id,
            "the selection moves off a deleted session"
        );
        assert!(
            !view
                .archived_sessions
                .entries
                .iter()
                .any(|entry| entry.session.id == root_id),
            "the refreshed listing no longer holds the deleted root"
        );
        let status = view.status_banner.as_ref().map(|note| note.text.clone());
        assert_eq!(status.as_deref(), Some("Project deleted"));
    });
    cx.update(|cx| {
        let view = view.read(cx);
        assert!(
            !listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == root_id),
            "the worker received the delete request"
        );
        assert!(
            listed_sessions(view, view.workspace_id)
                .iter()
                .any(|session| session.id == child_id),
            "only the requested root was submitted"
        );
    });
    backend.shutdown().ok();
}

/// Renders `build_project_session_context_menu` for `session` in a window of its
/// own and returns that window with the view the menu acts on.
fn open_project_menu(
    cx: &mut TestAppContext,
    session: AgentSessionSnapshot,
    project: Option<loom_core::ProjectSnapshot>,
) -> (gpui_kit::AnyWindowHandle, Entity<LoomView>) {
    let rendered_view = std::rc::Rc::new(std::cell::RefCell::new(None));
    let rendered_view_for_window = rendered_view.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.session_node_ids
                .insert(session.id, view.default_backend_node_id.clone());
            view.sessions = vec![session.clone()];
            view.active_session = session.clone();
            // The sidebar holds the project tree its menu is built from.
            view.project_tree_snapshots = project.iter().cloned().collect();
            view
        });
        let menu_view = view.clone();
        let menu_session = session.clone();
        let menu = gpui_kit::component::menu::PopupMenu::build(window, cx, move |menu, _, _| {
            LoomView::build_project_session_context_menu(
                menu,
                menu_session.clone(),
                project.clone(),
                menu_view.clone(),
            )
        });
        *rendered_view_for_window.borrow_mut() = Some(view);
        gpui_kit::component::Root::new(menu, window, cx)
    });
    let view = rendered_view.borrow_mut().take().unwrap();
    (handle.into(), view)
}

#[gpui_kit::test]
fn the_project_context_menu_offers_delete_project_only_for_a_project_root(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let workspace_id = WorkspaceId::new();
    let session = |id, name: &str| AgentSessionSnapshot {
        id,
        workspace_id,
        name: name.to_owned(),
        state: AgentSessionState::Idle,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(1),
    };
    let project = project_snapshot(root, &[child]);

    let (root_window, root_view) =
        open_project_menu(cx, session(root, "Checkout flow"), Some(project.clone()));
    cx.update_window(root_window, |_, window, cx| {
        window.render_frame(cx);
        let mut menu = window.within("popup-menu");
        assert_eq!(menu.find(0usize).label(), Some("Rename"));
        assert_eq!(menu.find(1usize).label(), Some("Archive project"));
        assert_eq!(menu.find(2usize).label(), Some("Delete project"));
        menu.click(2usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let view = root_view.read(cx);
        let dialog = view
            .project_delete_dialog
            .as_ref()
            .expect("the menu item opens the confirmation");
        assert_eq!(dialog.session_id, root);
        assert_eq!(dialog.name, "Checkout flow");
        assert_eq!(dialog.session_count, 2);
    });

    // A member session is deleted with its project, never on its own, so it
    // gets no Delete project item.
    let (child_window, child_view) =
        open_project_menu(cx, session(child, "Researcher"), Some(project.clone()));
    cx.update_window(child_window, |_, window, cx| {
        window.render_frame(cx);
        let menu = window.within("popup-menu");
        assert_eq!(menu.find(0usize).label(), Some("Rename"));
        assert_eq!(menu.find(1usize).label(), Some("Archive"));
        assert!(
            menu.try_find(2usize).is_none(),
            "only a project root offers Delete project"
        );
    })
    .unwrap();
    cx.update(|cx| {
        assert!(child_view.read(cx).project_delete_dialog.is_none());
    });

    // Neither does a session that belongs to no project at all.
    let (plain_window, plain_view) = open_project_menu(cx, session(child, "Standalone"), None);
    cx.update_window(plain_window, |_, window, cx| {
        window.render_frame(cx);
        let menu = window.within("popup-menu");
        assert_eq!(menu.find(1usize).label(), Some("Archive"));
        assert!(menu.try_find(2usize).is_none());
    })
    .unwrap();
    cx.update(|cx| {
        assert!(plain_view.read(cx).project_delete_dialog.is_none());
    });
}

#[gpui_kit::test]
fn the_project_deletion_confirmation_renders_its_consequences(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let root = AgentSessionId::new();
    let mut built = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.archived_sessions.open = true;
            view.archived_sessions.loaded = true;
            view.project_delete_dialog = Some(ProjectDeleteDialogState {
                session_id: root,
                name: "Checkout flow".to_owned(),
                session_count: 3,
                node_id: view.default_backend_node_id.clone(),
                force: false,
                in_flight: false,
                refusal: None,
            });
            view
        });
        built = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = built.expect("the window built the view");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        // The confirmation is over the full-screen archived-sessions surface.
        assert!(window.find("archived-sessions-dialog").visible());
        assert!(window.find("project-delete-dialog").visible());
        assert_eq!(
            window.find("project-delete-title").label(),
            Some("Delete project")
        );
        assert_eq!(
            window.find("project-delete-project").label(),
            Some("Checkout flow")
        );
        let consequence = window
            .find("project-delete-consequence")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(
            consequence.contains("2 sub-agent sessions"),
            "{consequence}"
        );
        assert!(consequence.contains("worktrees"), "{consequence}");
        let cancellation = window
            .find("project-delete-cancellation")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(cancellation.contains("cancelled"), "{cancellation}");
        assert_eq!(window.find("project-delete-force").checked(), Some(false));
        assert_eq!(window.find("project-delete-cancel").label(), Some("Cancel"));
        assert_eq!(
            window.find("project-delete-confirm").label(),
            Some("Delete")
        );
        window.click("project-delete-cancel", cx);
    })
    .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(view.read(cx).project_delete_dialog.is_none());
        assert!(window.try_find("project-delete-dialog").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_refused_project_deletion_renders_the_reason_in_the_dialog(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let root = AgentSessionId::new();
    let mut built = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.project_delete_dialog = Some(ProjectDeleteDialogState {
                session_id: root,
                name: "Checkout flow".to_owned(),
                session_count: 1,
                node_id: view.default_backend_node_id.clone(),
                force: true,
                in_flight: false,
                refusal: Some("deleting archived sessions requires durable storage".to_owned()),
            });
            view
        });
        built = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = built.expect("the window built the view");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        let refusal = window
            .find("project-delete-refusal")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(refusal.contains("requires durable storage"), "{refusal}");
        assert_eq!(window.find("project-delete-force").checked(), Some(true));
        assert!(
            !window
                .find("project-delete-confirm")
                .disabled()
                .unwrap_or(false),
            "a refusal leaves the deletion available for a retry"
        );
        assert!(
            !window
                .find("project-delete-cancel")
                .disabled()
                .unwrap_or(false)
        );
    })
    .unwrap();
    cx.update(|cx| {
        assert!(view.read(cx).project_delete_dialog.is_some());
    });
}
