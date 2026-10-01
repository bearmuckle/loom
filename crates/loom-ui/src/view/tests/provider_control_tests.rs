use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

#[gpui_kit::test]
fn provider_model_and_mode_controls_update_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sync_model_select_states(window, cx);
        view.sync_agent_mode_select_state(window, cx);
        assert!(view.model_select.is_some());
        assert!(view.agent_mode_select.is_some());

        view.select_model(ModelId::new("deterministic/demo"), cx);
        assert_eq!(view.model, ModelId::new("deterministic/demo"));
        view.select_model(ModelId::new("missing/model"), cx);

        view.select_default_model(ModelId::new("deterministic/demo"), cx);
        assert_eq!(view.default_model, ModelId::new("deterministic/demo"));
        view.select_default_model(ModelId::new("missing/model"), cx);

        view.select_agent_mode(AgentMode::Edit, cx);
        view.toggle_auto_approve_actions(cx);
        view.observe_system_appearance(window, cx);
        view.observe_system_appearance(window, cx);

        view.open_settings_from_menu(cx);
        assert!(view.settings_open);
        view.open_providers_for_node("test-node".to_owned(), cx);
        assert!(view.settings_open);
        assert_eq!(view.settings_section, SettingsSection::Providers);

        view.handle_github_provider_configured("test-node".to_owned(), cx);
        assert!(view.github_connected);

        let appearance = window.appearance();
        view.apply_appearance(appearance, window, cx);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn github_write_access_toggle_round_trips(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        assert!(!view.github_write_access);
        view.github_repository_connected = true;
        view.settings_open = true;
        view.settings_section = SettingsSection::Providers;
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window
            .within("settings-dialog")
            .click("github-write-access-toggle", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            assert!(view.github_write_access);
            view.toggle_github_write_access(false, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            assert!(!view.github_write_access);
            view.refresh_github_write_access("missing-node".to_owned(), cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn reasoning_visibility_toggle_round_trips(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        assert!(!view.show_reasoning);
        view.settings_open = true;
        view.settings_section = SettingsSection::Appearance;
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window
            .within("settings-dialog")
            .click("show-reasoning-toggle", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, cx| {
            assert!(view.show_reasoning);
            view.toggle_show_reasoning(cx);
            assert!(!view.show_reasoning);
        });
    })
    .unwrap();
}

#[test]
fn github_login_kind_secure_messages_differ() {
    assert!(
        crate::state::GitHubLoginKind::Copilot
            .secure_connection_message()
            .contains("Copilot")
    );
    assert!(
        crate::state::GitHubLoginKind::Repository
            .secure_connection_message()
            .contains("repository")
    );
}

#[gpui_kit::test]
fn begin_github_login_clears_finished_states(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.github_login = Some(GitHubLoginState::Success);
        view.begin_github_login(crate::state::GitHubLoginKind::Repository, cx);
        assert!(view.github_login.is_none());
        view.github_login = Some(GitHubLoginState::Error("failed".to_owned()));
        view.begin_github_login(crate::state::GitHubLoginKind::Copilot, cx);
        assert!(view.github_login.is_none());
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn github_repository_access_row_renders_when_disconnected(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.login_enabled = true;
        view.github_repository_connected = false;
        view.settings_open = true;
        view.settings_section = SettingsSection::Providers;
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn github_copilot_login_finishes_by_registering_the_provider(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        crate::connection::negotiate(&view.connection).unwrap();
        view.github_login_kind = crate::state::GitHubLoginKind::Copilot;
        view.finish_github_login(Ok("ghu_copilot_token".to_owned()), cx);
        view
    });
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        let view = window.root::<LoomView>().unwrap().unwrap();
        view.update(cx, |view, _| {
            assert!(view.github_connected);
            assert!(matches!(view.github_login, Some(GitHubLoginState::Success)));
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
