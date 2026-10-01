use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

use super::*;

#[gpui_kit::test]
fn async_session_load_and_creation_paths(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let session = empty_session_snapshot(view.workspace_id);
        view.activate_session(session.clone());
        view.create_session_on_node_with_source(
            "test-node".to_owned(),
            "New session".to_owned(),
            Some(SessionCreationSource::LocalDirectory("/tmp".to_owned())),
            cx,
        );
        let session_id = view.active_session.id;
        let failure = loom_protocol::ResponseEnvelope::failure(
            loom_core::RequestId::new(),
            loom_core::LoomError::new(loom_core::ErrorCode::Internal, "stale", false),
        );
        view.finish_async_session_load(session_id, failure.clone(), failure, cx);
        view.begin_transcript_page(Some(5), cx);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
