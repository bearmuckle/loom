use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

#[gpui_kit::test]
fn project_actions_update_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.refresh_active_project_snapshot(cx);
        let _ = view.project_root_is_active();
        let _ = view.project_has_live_children();
        let manager = AgentSessionId::new();
        view.control_project_child_from_ui(
            manager,
            loom_core::ProjectId::new(),
            loom_core::TaskId::new(),
            loom_protocol::ProjectChildControlAction::Continue,
            cx,
        );
        view.review_project_child_from_ui(
            manager,
            loom_core::ProjectId::new(),
            loom_core::TaskId::new(),
            cx,
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[cfg(not(target_family = "wasm"))]
mod refresh_tests {
    use super::*;
    use crate::backend_host::{BackendWorker, Job};
    use loom_core::ProjectSnapshot;

    fn setup(cx: &mut TestAppContext) -> (Entity<LoomView>, std::sync::mpsc::Receiver<Job>) {
        cx.update(gpui_kit::init);
        let (backend, requests) = BackendWorker::deferred_for_test();
        let view = cx.new(|cx| {
            let mut view =
                super::super::loom_view_render_tests::nested_project_view(cx.focus_handle());
            view.project_snapshot = None;
            view.project_tree_snapshots.clear();
            view.node_backends.clear();
            view.node_backends.insert("test".into(), backend);
            for session in &view.sessions {
                view.session_node_ids.insert(session.id, "test".into());
            }
            view
        });
        (view, requests)
    }

    fn snapshot(session_id: AgentSessionId) -> ProjectSnapshot {
        ProjectSnapshot {
            project_id: loom_core::ProjectId::from_uuid(*session_id.as_uuid()),
            root_session_id: session_id,
            agents: Vec::new(),
            tasks: Vec::new(),
            worktrees: Vec::new(),
        }
    }

    fn reply(job: Job, snapshot: ProjectSnapshot) {
        job.reply
            .send(ResponseEnvelope::success(
                job.request.request_id,
                ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)),
            ))
            .unwrap();
    }

    fn fail(job: Job, code: ErrorCode) {
        job.reply
            .send(ResponseEnvelope::failure(
                job.request.request_id,
                LoomError::new(code, "delayed failure", false),
            ))
            .unwrap();
    }

    #[gpui_kit::test]
    fn active_project_refresh_coalesces_dirty_events_and_finishes_fresh(cx: &mut TestAppContext) {
        let (view, requests) = setup(cx);
        let session_id = view.read_with(cx, |view, _| view.active_session.id);
        view.update(cx, |view, cx| view.refresh_active_project_snapshot(cx));
        let first = requests.try_recv().unwrap();
        for _ in 0..20 {
            view.update(cx, |view, cx| {
                view.project_snapshot_stale = true;
                view.refresh_active_project_snapshot(cx);
            });
        }
        assert!(requests.try_recv().is_err());
        let old = snapshot(session_id);
        reply(first, old.clone());
        cx.run_until_parked();
        let second = requests.try_recv().unwrap();
        assert!(requests.try_recv().is_err());
        let mut fresh = old;
        // Distinguish the later authoritative response without adding live
        // tasks (which would schedule a poll in this focused test).
        fresh.project_id = loom_core::ProjectId::new();
        reply(second, fresh.clone());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.project_snapshot.as_ref(), Some(&fresh));
            assert!(!view.project_snapshot_stale);
            assert!(view.project_snapshot_refreshing.is_none());
        });
        assert!(requests.try_recv().is_err());
    }

    #[gpui_kit::test]
    fn active_project_refresh_tracks_latest_selection(cx: &mut TestAppContext) {
        let (view, requests) = setup(cx);
        let old_id = view.read_with(cx, |view, _| view.active_session.id);
        view.update(cx, |view, cx| view.refresh_active_project_snapshot(cx));
        let first = requests.try_recv().unwrap();
        let new_id = view.update(cx, |view, cx| {
            view.active_session = view.sessions[1].clone();
            view.refresh_active_project_snapshot(cx);
            view.active_session.id
        });
        assert!(requests.try_recv().is_err());
        reply(first, snapshot(old_id));
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.project_snapshot.is_none()));
        let second = requests.try_recv().unwrap();
        assert!(matches!(second.request.request,
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession { session_id })
                if session_id == new_id));
        reply(second, snapshot(new_id));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.project_snapshot.as_ref().unwrap().root_session_id,
                new_id
            );
            assert!(view.project_snapshot_refreshing.is_none());
        });
        assert!(requests.try_recv().is_err());
    }

    #[gpui_kit::test]
    fn active_project_refresh_error_has_no_retry_loop(cx: &mut TestAppContext) {
        let (view, requests) = setup(cx);
        view.update(cx, |view, cx| view.refresh_active_project_snapshot(cx));
        let first = requests.try_recv().unwrap();
        // An invalidation during a failing request still gets one follow-up.
        view.update(cx, |view, cx| view.refresh_active_project_snapshot(cx));
        fail(first, ErrorCode::Internal);
        cx.run_until_parked();
        fail(requests.try_recv().unwrap(), ErrorCode::Internal);
        cx.run_until_parked();
        assert!(requests.try_recv().is_err());
        view.read_with(cx, |view, _| {
            assert!(view.project_snapshot_refreshing.is_none());
            assert!(!view.project_snapshot_stale);
        });
        // A later explicit refresh is still possible after failure/NotFound.
        view.update(cx, |view, cx| view.refresh_active_project_snapshot(cx));
        fail(requests.try_recv().unwrap(), ErrorCode::NotFound);
        cx.run_until_parked();
        assert!(requests.try_recv().is_err());
        view.read_with(cx, |view, _| assert!(view.project_snapshot.is_none()));
    }

    #[gpui_kit::test]
    fn missing_project_scan_coalesces_reloads_and_discovers_new_sessions(cx: &mut TestAppContext) {
        let (view, requests) = setup(cx);
        view.update(cx, |view, cx| view.refresh_missing_project_snapshots(cx));
        cx.run_until_parked();
        let first = requests.try_recv().unwrap();
        // Newly loaded rows must be scanned too, but overlapping reloads must
        // not rescan the standalone/error rows already attempted in this burst.
        let new_id = view.update(cx, |view, cx| {
            let mut session = view.sessions[0].clone();
            session.id = AgentSessionId::new();
            let id = session.id;
            view.sessions.push(session);
            view.session_node_ids.insert(id, "test".into());
            for _ in 0..20 {
                view.refresh_missing_project_snapshots(cx);
            }
            id
        });
        assert!(requests.try_recv().is_err());
        fail(first, ErrorCode::NotFound);
        cx.run_until_parked();
        let second = requests.try_recv().unwrap();
        for _ in 0..20 {
            view.update(cx, |view, cx| view.refresh_missing_project_snapshots(cx));
        }
        fail(second, ErrorCode::Internal);
        cx.run_until_parked();
        let third = requests.try_recv().unwrap();
        // The remaining original session is scanned before the new row.
        let third_id = match third.request.request {
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession { session_id }) => {
                session_id
            }
            _ => panic!("unexpected request"),
        };
        reply(third, snapshot(third_id));
        cx.run_until_parked();
        let fourth = requests.try_recv().unwrap();
        assert!(matches!(fourth.request.request,
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession { session_id })
                if session_id == new_id));
        let fresh = snapshot(new_id);
        reply(fourth, fresh.clone());
        cx.run_until_parked();
        assert!(requests.try_recv().is_err());
        view.read_with(cx, |view, _| {
            assert!(view.project_tree_snapshots.contains(&fresh));
            assert!(!view.missing_project_snapshots_refreshing);
        });
    }
}
