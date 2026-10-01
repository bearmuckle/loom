use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

#[gpui_kit::test]
fn lifecycle_source_and_worker_actions(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let _ = view.refresh_sessions();
        view.reload_sessions(cx);
        view.reset_projection();
        view.update_session_list();
        view.refresh_models();
        view.apply_models(vec![ModelId::new("deterministic/demo")]);
        view.schedule_project_poll(cx);
        view.schedule_run_poll(cx);
        view.schedule_worker_node_poll(cx);
        view.poll_run_once(cx);

        view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
        view.choose_source(SessionSourceChoice::LocalDirectory, cx);
        view.choose_source(SessionSourceChoice::GitHub, cx);
        view.choose_source(SessionSourceChoice::Empty, cx);
        view.confirm_source_dialog(cx);
        view.set_worker_node_connection_failure(999, "wss://none", "detail".to_owned());
        view.add_source_to_active_session(
            SessionCreationSource::LocalDirectory("/tmp".to_owned()),
            cx,
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
