use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

#[gpui_kit::test]
fn session_and_run_actions_update_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let session = empty_session_snapshot(view.workspace_id);
        view.activate_session(session.clone());
        view.load_session(session.clone());
        view.select_session(session.clone(), cx);
        view.begin_session_rename(session.clone(), false, window, cx);
        assert!(view.rename_dialog.is_some());
        view.confirm_rename(cx);
        let archive_id = view.active_session.id;
        view.archive_session(archive_id, cx);
        view.select_session_repository(loom_core::RepositoryId::new(), cx);
        view.detach_session_repository(loom_core::RepositoryId::new(), cx);
        view.detach_session_directory("dir".to_owned(), cx);
        view.send_message("hello".to_owned(), cx);
        view.approve_pending_action(cx);
        view.reject_pending_action(cx);
        view.interrupt_active_run(cx);
        view.begin_transcript_page(None, cx);
        view.ensure_session_task_message(view.active_session.id);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
