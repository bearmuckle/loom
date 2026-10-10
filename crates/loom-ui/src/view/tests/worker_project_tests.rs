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
