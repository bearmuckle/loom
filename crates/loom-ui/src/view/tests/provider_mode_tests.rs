use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

#[gpui_kit::test]
fn agent_modes_and_provider_selection(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.open_providers_for_node("test-node".to_owned(), cx);
        view.configure_api_key_provider(loom_model::ProviderId::new("openai"), window, cx);
        view.providers_node_id = Some("test-node".to_owned());
        view.copy_github_login_value("ABCD".to_owned(), "device code", cx);

        for mode in [
            AgentMode::Ask,
            AgentMode::Edit,
            AgentMode::Agent,
            AgentMode::AutoApprove,
        ] {
            view.select_agent_mode(mode, cx);
            view.sync_agent_mode_select_state(window, cx);
        }
        view.toggle_auto_approve_actions(cx);
        view.toggle_auto_approve_actions(cx);
        view.sync_model_select_states(window, cx);
        view.select_model(ModelId::new("deterministic/demo"), cx);
        view.select_default_model(ModelId::new("deterministic/demo"), cx);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
