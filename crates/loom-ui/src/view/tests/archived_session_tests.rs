//! Archived-session management: project grouping, deletion, and the
//! read-only archive-retention policy.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::Capability;
use loom_protocol::{ArchiveRetentionPolicy, SessionRequest, SessionResponse};

use crate::view::archive::{
    ArchivedSessionEntry, ArchivedSessionsState, RetentionPolicyState, RetentionPolicyText,
    archived_age_label, archived_session_groups, delete_archived_session_request,
    retention_policy_state, retention_window_label,
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
fn archived_sessions_group_by_project_with_archive_age() {
    let now = 1_700_000_000_000u64;
    let day = 86_400_000u64;
    let hour = 3_600_000u64;
    let root = AgentSessionId::new();
    let child = AgentSessionId::new();
    let grandchild = AgentSessionId::new();
    let loose_new = AgentSessionId::new();
    let loose_old = AgentSessionId::new();
    // The projection also covers a session that is not archived, which must not
    // appear in the surface.
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
    // The grandchild hangs off the child, so the group nests deepest last.
    for agent in &mut project.agents {
        if agent.session_id == grandchild {
            agent.depth = 3;
            agent.parent_session_id = Some(child);
        }
    }

    let groups = archived_session_groups(&entries, &[project], now);

    assert_eq!(
        groups.len(),
        2,
        "one project group and one no-project group"
    );
    let project_group = &groups[0];
    assert_eq!(project_group.project_root, Some(root));
    assert_eq!(project_group.label, "Checkout flow");
    let rows = project_group
        .rows
        .iter()
        .map(|row| {
            (
                row.session.name.as_str(),
                row.is_project_root,
                row.age.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        vec![
            ("Checkout flow", true, "Archived 2d ago"),
            ("Researcher", false, "Archived 3h ago"),
            ("Analyst", false, "Archived 30m ago"),
        ]
    );
    // Deleting the root cascades to the two archived descendants.
    assert_eq!(project_group.rows[0].descendant_count, 2);
    assert_eq!(project_group.rows[1].descendant_count, 0);

    let loose_group = &groups[1];
    assert_eq!(loose_group.project_root, None);
    assert_eq!(loose_group.label, "No project");
    let loose = loose_group
        .rows
        .iter()
        .map(|row| row.session.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(loose, vec!["Standalone", "Old standalone"]);
    assert_eq!(loose_group.rows[1].age, "Archived 1w ago");

    // A session with no project snapshot still groups under its own heading.
    let without_projects = archived_session_groups(&entries, &[], now);
    assert_eq!(without_projects.len(), 1);
    assert_eq!(without_projects[0].label, "No project");
    assert_eq!(without_projects[0].rows.len(), entries.len());

    // A project whose root is not archived is labelled by its session id.
    let child_only = vec![entry(archived_session(child, "Researcher", now))];
    let groups = archived_session_groups(&child_only, &[project_snapshot(root, &[child])], now);
    assert_eq!(groups.len(), 1);
    assert!(groups[0].label.starts_with("Project "));
    assert!(!groups[0].rows[0].is_project_root);
    assert_eq!(groups[0].rows[0].descendant_count, 0);
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
            Timestamp::now().as_unix_millis(),
        );
        assert_eq!(groups.len(), 1);
        assert!(groups[0].label.starts_with("Project "), "cached root label");
        assert!(
            !groups[0].rows[0].is_project_root,
            "the archived session groups as the cached project's descendant"
        );
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
        assert!(window.find(("archived-session-row", 0usize)).visible());
        assert!(window.find(("archived-session-row", 1usize)).visible());
        assert!(window.find(("archived-session-force", 0usize)).visible());
        assert!(window.find(("archived-session-delete", 0usize)).visible());
    })
    .unwrap();
}
