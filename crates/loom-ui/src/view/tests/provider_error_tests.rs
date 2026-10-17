use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

use super::*;

#[gpui_kit::test]
fn github_login_error_paths(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        // The device flow runs on the worker, so a login against a worker that
        // is not connected fails before any request is sent.
        view.providers_node_id = Some("missing-node".to_owned());
        view.start_github_login_flow(cx);
        assert!(matches!(
            &view.github_login,
            Some(GitHubLoginState::Error(message)) if message.contains("not connected")
        ));
        view.persist_and_distribute_workspace_config(None, cx);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
