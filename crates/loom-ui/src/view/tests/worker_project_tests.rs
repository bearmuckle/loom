//! Worker-scoped project navigation: grouping and the New project worker
//! picker.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, CapabilitySet, Timestamp, WorkspaceId,
};
use loom_protocol::{WorkerNodeResources, WorkerNodeStatus};
use std::collections::BTreeMap;

fn worker_node(id: u64, node_id: &str, name: &str, is_local: bool) -> WorkerNodeEntry {
    WorkerNodeEntry {
        id,
        status: WorkerNodeStatus {
            node_id: node_id.to_owned(),
            name: name.to_owned(),
            online: true,
            capabilities: CapabilitySet::default(),
            resources: WorkerNodeResources {
                cpu_count: 4,
                cpu_usage_percent: Some(10),
                memory_usage_percent: Some(20),
                memory_total_bytes: Some(8 * 1024 * 1024 * 1024),
                memory_available_bytes: Some(4 * 1024 * 1024 * 1024),
                disk_total_bytes: Some(64 * 1024 * 1024 * 1024),
                disk_available_bytes: Some(32 * 1024 * 1024 * 1024),
            },
        },
        is_local,
        url: (!is_local).then(|| format!("ws://{node_id}/ws")),
        connection: None,
        connection_state: WorkerConnectionState::Connected,
        connection_detail: None,
        severe_load_streak: 0,
    }
}

fn session(id: AgentSessionId, name: &str) -> AgentSessionSnapshot {
    let timestamp = Timestamp::from_unix_millis(0);
    AgentSessionSnapshot {
        id,
        workspace_id: WorkspaceId::new(),
        name: name.to_owned(),
        state: AgentSessionState::Idle,
        created_at: timestamp,
        updated_at: timestamp,
    }
}

#[test]
fn sessions_group_by_worker_with_unknown_owners_last() {
    let local = AgentSessionId::new();
    let peer = AgentSessionId::new();
    let removed = AgentSessionId::new();
    let untracked = AgentSessionId::new();
    let sessions = vec![
        session(local, "Local"),
        session(peer, "Peer"),
        session(removed, "Removed"),
        session(untracked, "Untracked"),
    ];
    let owners = BTreeMap::from([
        (local, "local".to_owned()),
        (peer, "peer".to_owned()),
        (removed, "removed".to_owned()),
    ]);
    let groups = group_sessions_by_worker(
        &sessions,
        &owners,
        &["local".to_owned(), "peer".to_owned()],
        "local",
    );

    let ids = groups.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>();
    assert_eq!(ids, ["local", "peer", "removed"]);
    // The unowned session falls back to the default worker.
    assert_eq!(groups[0].1.len(), 2);
    assert_eq!(groups[1].1.len(), 1);
    assert_eq!(groups[2].1.len(), 1);
    assert_eq!(groups[2].1[0].id, removed);
}

#[test]
fn connected_workers_without_sessions_keep_an_empty_group() {
    let local = AgentSessionId::new();
    let sessions = vec![session(local, "Local")];
    let owners = BTreeMap::from([(local, "local".to_owned())]);
    let groups = group_sessions_by_worker(
        &sessions,
        &owners,
        &["local".to_owned(), "empty".to_owned()],
        "local",
    );

    let ids = groups.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>();
    assert_eq!(ids, ["local", "empty"]);
    assert!(groups[1].1.is_empty());
}

#[gpui_kit::test]
fn sidebar_groups_projects_by_worker_and_collapses(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let local_node_id = view.default_backend_node_id.clone();
            view.worker_nodes = vec![
                worker_node(0, &local_node_id, "this machine", true),
                worker_node(1, "peer-node", "build-01", false),
            ];
            let local_session = view.active_session.clone();
            let mut peer_session = view.active_session.clone();
            peer_session.id = AgentSessionId::new();
            peer_session.name = "api-gateway".to_owned();
            view.sessions = vec![local_session.clone(), peer_session.clone()];
            view.session_node_ids
                .insert(local_session.id, local_node_id.clone());
            view.session_node_ids
                .insert(peer_session.id, "peer-node".to_owned());
            view.active_session = local_session;
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find(("worker-group", 0usize)).visible());
        assert!(window.find(("worker-group", 1usize)).visible());
        assert!(window.find(("session-tree-root", 0usize)).visible());
        assert!(window.find(("session-tree-root", 1_000_000usize)).visible());

        // Collapsing the first worker hides its tree but keeps the second.
        window.click(("worker-group", 0usize), cx);
        window.render_frame(cx);
        assert!(window.try_find(("session-tree-root", 0usize)).is_none());
        assert!(window.find(("session-tree-root", 1_000_000usize)).visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn reload_sessions_loads_projects_from_each_connected_worker(cx: &mut TestAppContext) {
    use crate::connection::{create_session_in_workspace, create_workspace, negotiate};

    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        negotiate(&view.connection).expect("negotiate");
        // Register a workspace and a project on the in-process backend, then
        // expose it as an online worker node so reload lists it.
        let workspace = create_workspace(&view.connection, "Peer workspace").expect("workspace");
        create_session_in_workspace(&view.connection, workspace.id, "peer-project")
            .expect("session");

        let local_node_id = view.default_backend_node_id.clone();
        let mut local = worker_node(0, &local_node_id, "this machine", true);
        local.connection = Some(view.connection.clone());
        view.worker_nodes = vec![local];

        // A second worker with no workspaces yet exercises the fallback path.
        let empty_connection =
            ClientConnection::InProcess(Box::new(loom_local::OwnedBackend::new().connect()));
        negotiate(&empty_connection).expect("negotiate");
        view.node_backends.insert(
            "empty-node".to_owned(),
            BackendWorker::spawn(empty_connection.clone()),
        );
        let mut empty = worker_node(1, "empty-node", "empty", false);
        empty.connection = Some(empty_connection);
        view.worker_nodes.push(empty);
        view
    });

    cx.update(|cx| {
        view.update(cx, |view, cx| view.reload_sessions(cx));
    });
    cx.run_until_parked();

    let sessions = cx.update(|cx| view.read(cx).sessions.clone());
    assert!(
        sessions
            .iter()
            .any(|session| session.name == "peer-project"),
        "reload should surface the assigned worker's project"
    );
}

#[gpui_kit::test]
fn creation_on_a_peer_uses_the_peers_own_workspace(cx: &mut TestAppContext) {
    use crate::connection::{create_workspace, negotiate};

    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        negotiate(&view.connection).unwrap();
        let home = create_workspace(&view.connection, "Home").unwrap();
        view.workspace_id = home.id;
        view.workspaces = vec![home];

        // A peer with its own workspace and a reachable in-process backend.
        let peer_connection =
            ClientConnection::InProcess(Box::new(loom_local::OwnedBackend::new().connect()));
        negotiate(&peer_connection).unwrap();
        create_workspace(&peer_connection, "Peer").unwrap();
        view.node_backends.insert(
            "peer-node".to_owned(),
            BackendWorker::spawn(peer_connection.clone()),
        );
        let mut peer = worker_node(1, "peer-node", "build-01", false);
        peer.connection = Some(peer_connection);
        view.worker_nodes.push(peer);
        view
    });

    view.update(cx, |view, cx| {
        view.create_session_on_node_with_source(
            "peer-node".to_owned(),
            "peer project".to_owned(),
            None,
            cx,
        );
    });
    cx.run_until_parked();

    // Refresh so the peer's workspaces are recorded, then assert creation used
    // the peer's own workspace and never shared the home workspace with it.
    view.update(cx, |view, cx| view.reload_sessions(cx));
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        assert_eq!(view.sessions.len(), 1);
        let home_id = view.workspace_id;
        let session_workspace = view.sessions[0].workspace_id;
        assert_ne!(session_workspace, home_id);
        let peer_workspaces = view
            .node_workspaces
            .get("peer-node")
            .expect("peer workspaces recorded");
        assert_eq!(peer_workspaces.len(), 1);
        assert_eq!(peer_workspaces[0].id, session_workspace);
        assert!(
            !peer_workspaces
                .iter()
                .any(|workspace| workspace.id == home_id),
            "the home workspace must never be registered on a peer"
        );
    });
}

#[gpui_kit::test]
fn creation_provisions_a_default_workspace_when_the_worker_has_none(cx: &mut TestAppContext) {
    use crate::connection::{create_workspace, negotiate};

    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        negotiate(&view.connection).unwrap();
        let home = create_workspace(&view.connection, "Home").unwrap();
        view.workspace_id = home.id;
        view.workspaces = vec![home];

        // A peer that has never had a workspace.
        let fresh_connection =
            ClientConnection::InProcess(Box::new(loom_local::OwnedBackend::new().connect()));
        negotiate(&fresh_connection).unwrap();
        view.node_backends.insert(
            "fresh-node".to_owned(),
            BackendWorker::spawn(fresh_connection.clone()),
        );
        let mut fresh = worker_node(1, "fresh-node", "fresh", false);
        fresh.connection = Some(fresh_connection);
        view.worker_nodes.push(fresh);
        view
    });

    view.update(cx, |view, cx| {
        view.create_session_on_node_with_source(
            "fresh-node".to_owned(),
            "fresh project".to_owned(),
            None,
            cx,
        );
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| view.reload_sessions(cx));
    cx.run_until_parked();
    // Exercise config distribution to each worker's own workspace.
    view.update(cx, |view, cx| {
        view.persist_and_distribute_workspace_config(None, cx)
    });
    cx.run_until_parked();

    cx.update(|cx| {
        let view = view.read(cx);
        let home_id = view.workspace_id;
        assert_eq!(view.sessions.len(), 1);
        let session_workspace = view.sessions[0].workspace_id;
        assert_ne!(session_workspace, home_id);
        let workspaces = view
            .node_workspaces
            .get("fresh-node")
            .expect("fresh workspaces recorded");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "Default");
        assert_eq!(workspaces[0].id, session_workspace);
    });
}

#[gpui_kit::test]
fn source_dialog_worker_picker_scopes_creation_to_a_worker(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let local_node_id = view.default_backend_node_id.clone();
            view.worker_nodes = vec![
                worker_node(0, &local_node_id, "this machine", true),
                worker_node(1, "peer-node", "build-01", false),
            ];
            view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
            let dialog = view.source_dialog.as_ref().expect("dialog opened");
            assert_eq!(dialog.target_node_id, local_node_id);
            assert!(dialog.local_directory_available);
            assert_eq!(
                view.source_node_id().as_deref(),
                Some(local_node_id.as_str())
            );

            view.choose_source_node("peer-node".to_owned(), cx);
            let dialog = view.source_dialog.as_ref().expect("dialog still open");
            assert_eq!(dialog.target_node_id, "peer-node");
            assert!(!dialog.local_directory_available);
            assert_eq!(view.source_node_id().as_deref(), Some("peer-node"));
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        assert!(window.find("source-worker-node-peer-node").visible());
    })
    .unwrap();
}
