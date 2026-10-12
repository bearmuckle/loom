//! Archived-session management: project grouping, deletion, and the
//! archive-retention policy, which the surface can edit on a worker that stores
//! one.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::Capability;
use loom_protocol::{ArchiveRetentionPolicy, SessionRequest, SessionResponse};

use crate::view::archive::{
    ArchivedSessionEntry, ArchivedSessionsState, RETENTION_WINDOW_PRESETS, RetentionPolicyState,
    RetentionPolicyText, RetentionWindowChoice, archived_age_label, archived_session_groups,
    delete_archived_session_request, retention_editable_state, retention_policy_state,
    retention_window_choice, retention_window_choices, retention_window_label,
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
