use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::CapabilitySet;

fn remote_node(id: u64, url: &str, state: WorkerConnectionState) -> WorkerNodeEntry {
    WorkerNodeEntry {
        id,
        status: WorkerNodeStatus {
            node_id: format!("worker-{id}"),
            name: format!("Worker {id}"),
            online: false,
            capabilities: CapabilitySet::default(),
            resources: WorkerNodeResources {
                cpu_count: 2,
                cpu_usage_percent: Some(50),
                memory_usage_percent: Some(75),
                memory_total_bytes: Some(8),
                memory_available_bytes: Some(2),
                disk_total_bytes: None,
                disk_available_bytes: None,
            },
        },
        is_local: false,
        url: Some(url.to_owned()),
        connection: None,
        connection_state: state,
        connection_detail: None,
        severe_load_streak: 0,
    }
}

#[gpui_kit::test]
fn worker_failures_and_removal_update_nodes(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.worker_nodes.push(remote_node(
            1,
            "wss://one.example/ws",
            WorkerConnectionState::Connecting,
        ));
        view.worker_nodes.push(remote_node(
            2,
            "wss://two.example/ws",
            WorkerConnectionState::Connected,
        ));
        view.set_worker_node_connection_failure(1, "wss://one.example/ws", "boom".to_owned());
        assert_eq!(
            view.worker_nodes[0].connection_state,
            WorkerConnectionState::Failed
        );
        let error = loom_core::LoomError::new(
            loom_core::ErrorCode::ProviderUnavailable,
            "connection refused",
            true,
        );
        view.fail_worker_node_connection(
            2,
            "wss://two.example/ws",
            WorkerConnectionStage::Transport,
            &error,
            Some("secret-token"),
            false,
        );
        assert!(view.worker_nodes[1].connection_detail.is_some());
        view.remove_worker_node(1, cx);
        assert!(view.worker_nodes.iter().all(|node| node.id != 1));
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
