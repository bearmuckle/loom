use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

#[gpui_kit::test]
fn review_and_project_child_actions(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.refresh_review(cx);
        view.open_review_file("src/lib.rs".to_owned(), cx);
        view.open_review_diff("src/lib.rs".to_owned(), false, cx);
        view.jump_review_hunk(true, cx);
        view.jump_review_hunk(false, cx);
        let manager = AgentSessionId::new();
        let project = loom_core::ProjectId::new();
        let task = loom_core::TaskId::new();
        view.integrate_project_child_from_ui(
            manager,
            project,
            task,
            "parent".to_owned(),
            "child".to_owned(),
            cx,
        );
        view.cleanup_project_child_from_ui(
            manager,
            project,
            task,
            loom_core::ProjectWorktreeCleanupDisposition::RemoveClean,
            cx,
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
