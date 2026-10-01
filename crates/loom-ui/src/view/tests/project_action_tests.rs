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
