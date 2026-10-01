use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

use super::*;

#[gpui_kit::test]
fn github_login_error_paths(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.providers_node_id = Some("test-node".to_owned());
        let error = loom_core::LoomError::new(
            loom_core::ErrorCode::ProviderAuthentication,
            "denied",
            false,
        );
        view.handle_github_device_code(Err(error), cx);
        assert!(matches!(
            view.github_login,
            Some(GitHubLoginState::Error(_))
        ));
        view.finish_github_login(
            Err(loom_core::LoomError::new(
                loom_core::ErrorCode::ProviderAuthentication,
                "denied",
                false,
            )),
            cx,
        );
        view.persist_and_distribute_workspace_config(None, cx);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
