use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

fn render_with(cx: &mut TestAppContext, configure: impl FnOnce(&mut LoomView)) {
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            configure(&mut view);
            view
        });
        gpui_kit::component::Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn settings_sections_and_dialogs_render(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    for section in [
        SettingsSection::Agents,
        SettingsSection::Providers,
        SettingsSection::Workers,
        SettingsSection::Appearance,
    ] {
        render_with(cx, move |view| {
            view.settings_open = true;
            view.settings_section = section;
        });
    }
    render_with(cx, |view| {
        view.settings_open = true;
        view.settings_section = SettingsSection::Providers;
    });
    render_with(cx, |view| {
        view.settings_open = true;
        view.settings_section = SettingsSection::About;
    });
    render_with(cx, |view| {
        view.command_palette_open = true;
    });
    render_with(cx, |view| {
        view.review.open = true;
    });
    render_with(cx, |view| {
        view.github_login = Some(GitHubLoginState::Starting);
    });
    render_with(cx, |view| {
        view.github_login = Some(GitHubLoginState::Awaiting {
            verification_uri: "https://github.com/login/device".to_owned(),
            user_code: "ABCD-1234".to_owned(),
            expires_in: 900,
        });
    });
    render_with(cx, |view| {
        view.github_login = Some(GitHubLoginState::Error("failed".to_owned()));
    });
    render_with(cx, |view| {
        view.github_login = Some(GitHubLoginState::Success);
    });
}
