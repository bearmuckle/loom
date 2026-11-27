//! Touch-tap focus and virtual-keyboard tests for the composer, Settings/Workers
//! URL field, first-run setup fields, and other single-line `KitInput` controls.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    Pixels, PlatformInput, Point, TestAppContext, TouchEvent, TouchId, TouchPhase, point, px, size,
};

/// Dispatches a real touch tap (`TouchPhase::Started` followed by
/// `TouchPhase::Ended`) at `position`, matching `gpui-pre-web`'s touch path.
fn touch_tap_at(window: &mut Window, id: u64, position: Point<Pixels>, cx: &mut App) {
    window.render_frame(cx);
    window.dispatch_event(
        PlatformInput::Touch(TouchEvent {
            id: TouchId(id),
            phase: TouchPhase::Started,
            position,
            predicted_position: None,
            force: None,
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::Touch(TouchEvent {
            id: TouchId(id),
            phase: TouchPhase::Ended,
            position,
            predicted_position: None,
            force: None,
        }),
        cx,
    );
    window.render_frame(cx);
}

#[gpui_kit::test]
fn phone_touch_tap_on_composer_padding_and_text_focuses_composer(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);

        // Move focus away from the composer before testing touch taps.
        view.update(cx, |view, cx| {
            view.composer_focus_handle.focus(window, cx);
        });
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert!(!input.read(cx).focus_handle(cx).is_focused(window));
        });

        let bounds = window.find("composer-field").bounds();
        // Tap inside the `p_3` padding outside the inner `Textarea`.
        touch_tap_at(
            window,
            1,
            point(bounds.origin.x + px(4.), bounds.origin.y + px(4.)),
            cx,
        );
        assert!(window.last_input_was_touch());
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert!(
                input.read(cx).focus_handle(cx).is_focused(window),
                "a touch tap on composer padding should focus the composer"
            );
        });
        window.input("pad ", cx);

        // Tapping the wrapper padding again while already focused keeps focus
        // and exercises the keyboard re-request path without re-focusing.
        touch_tap_at(
            window,
            2,
            point(bounds.origin.x + px(4.), bounds.origin.y + px(4.)),
            cx,
        );
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
        });

        // Blur again and tap directly on the inner `Textarea`.
        view.update(cx, |view, cx| {
            view.composer_focus_handle.focus(window, cx);
        });
        window.render_frame(cx);
        touch_tap_at(window, 3, bounds.center(), cx);
        assert!(window.last_input_was_touch());
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert!(
                input.read(cx).focus_handle(cx).is_focused(window),
                "a touch tap on the composer textarea should focus the composer"
            );
        });
        window.input("text", cx);
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert_eq!(input.read(cx).value().to_string(), "pad text");
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn observer_touch_tap_on_composer_padding_does_not_focus_disabled_composer(
    cx: &mut TestAppContext,
) {
    use loom_core::{ParticipantId, ParticipantIdentity, PresenceStatus, RosterEntry, SessionRole};

    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let observer = ParticipantIdentity {
            id: ParticipantId::new(),
            display_name: "Observer".to_owned(),
        };
        view.collaboration.identity = Some(observer.clone());
        let node_id = view.default_backend_node_id.clone();
        view.unsigned_nodes.insert(node_id);
        view.collaboration.support = crate::collaboration::CollaborationSupport::Supported;
        view.collaboration.replace_roster(
            view.active_session.id,
            vec![RosterEntry {
                participant: observer,
                role: SessionRole::Observer,
                presence: PresenceStatus::Online,
                connections: 1,
                last_active_at: None,
            }],
        );
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            assert!(!view.session_permissions().can_act);
            view.composer_focus_handle.focus(window, cx);
        });
        window.render_frame(cx);

        let bounds = window.find("composer-field").bounds();
        touch_tap_at(
            window,
            1,
            point(bounds.origin.x + px(4.), bounds.origin.y + px(4.)),
            cx,
        );
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert!(
                !input.read(cx).focus_handle(cx).is_focused(window),
                "an observer's disabled composer must not focus on padding tap"
            );
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn phone_touch_tap_on_settings_workers_url_padding_and_center_focuses_input(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.show_surface(CoveringSurface::Settings(SettingsSection::Workers));
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);

        // Move focus away from the Workers URL input first.
        view.update(cx, |view, cx| {
            view.composer_focus_handle.focus(window, cx);
        });
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            let input = view.node_input_state.clone().expect("node input");
            assert!(!input.read(cx).focus_handle(cx).is_focused(window));
        });

        let bounds = window.find("worker-node-url-input").bounds();
        // Tap inside `BaseInput`'s left padding (`input_px` = 8px for small):
        // without the wrapper handler, `BaseInput` focuses its frame handle
        // instead of `InputState`.
        let left_padding = point(bounds.origin.x + px(3.), bounds.center().y);
        touch_tap_at(window, 1, left_padding, cx);
        assert!(window.last_input_was_touch());
        view.update(cx, |view, cx| {
            let input = view.node_input_state.clone().expect("node input");
            assert!(
                input.read(cx).focus_handle(cx).is_focused(window),
                "tapping the Settings/Workers URL field padding should focus the input"
            );
        });
        window.input("ws://127.0.0.1", cx);

        // Tapping the left padding again while already focused must keep focus
        // on `InputState` instead of letting `BaseInput` steal it.
        touch_tap_at(window, 2, left_padding, cx);
        view.update(cx, |view, cx| {
            let input = view.node_input_state.clone().expect("node input");
            assert!(
                input.read(cx).focus_handle(cx).is_focused(window),
                "re-tapping the padding must keep focus on the input"
            );
        });
        window.input(":8765/ws", cx);
        view.update(cx, |view, cx| {
            let input = view.node_input_state.clone().expect("node input");
            assert_eq!(input.read(cx).value().to_string(), "ws://127.0.0.1:8765/ws");
        });

        // Move focus away and tap the center of the input.
        view.update(cx, |view, cx| {
            view.composer_focus_handle.focus(window, cx);
        });
        window.render_frame(cx);
        touch_tap_at(window, 3, bounds.center(), cx);
        view.update(cx, |view, cx| {
            let input = view.node_input_state.clone().expect("node input");
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
        });
    })
    .unwrap();
}

#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn touch_tap_on_first_run_setup_url_and_token_padding_focuses_inputs(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
        let options = loom_local::UiOptions {
            project: None,
            task: "setup".to_owned(),
            model: ModelId::new("deterministic/demo"),
            remote: None,
            token: None,
            reset_state: false,
            server_binary: None,
            ca: None,
            allow_insecure_remote: false,
        };
        LoomView::new_setup(&options, None, None, cx.focus_handle())
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);

        let url_bounds = window.find("worker-setup-url").bounds();
        let token_bounds = window.find("worker-setup-token").bounds();

        // Tap left padding of the first-run URL field.
        touch_tap_at(
            window,
            1,
            point(url_bounds.origin.x + px(3.), url_bounds.center().y),
            cx,
        );
        view.update(cx, |view, cx| {
            let input = view
                .setup
                .as_ref()
                .and_then(|setup| setup.url_input.clone())
                .expect("setup url input");
            assert!(
                input.read(cx).focus_handle(cx).is_focused(window),
                "tapping the first-run URL field padding should focus it"
            );
        });
        window.input("ws://host/ws", cx);

        // Tap left padding of the first-run token field.
        touch_tap_at(
            window,
            2,
            point(token_bounds.origin.x + px(3.), token_bounds.center().y),
            cx,
        );
        view.update(cx, |view, cx| {
            let input = view
                .setup
                .as_ref()
                .and_then(|setup| setup.token_input.clone())
                .expect("setup token input");
            assert!(
                input.read(cx).focus_handle(cx).is_focused(window),
                "tapping the first-run token field padding should focus it"
            );
        });
        window.input("demo-token", cx);

        view.update(cx, |view, cx| {
            let setup = view.setup.as_ref().expect("setup state");
            assert_eq!(
                setup.url_input.as_ref().unwrap().read(cx).value().as_str(),
                "ws://host/ws"
            );
            assert_eq!(
                setup
                    .token_input
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .value()
                    .as_str(),
                "demo-token"
            );
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn desktop_mouse_click_on_settings_workers_input_padding_and_text_works(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.show_surface(CoveringSurface::Settings(SettingsSection::Workers));
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            view.composer_focus_handle.focus(window, cx);
        });
        window.render_frame(cx);

        // Desktop mouse click in the left padding focuses the input without
        // switching input modality to touch.
        window.click_at("worker-node-url-input", point(px(3.), px(12.)), cx);
        window.render_frame(cx);
        assert!(!window.last_input_was_touch());
        view.update(cx, |view, cx| {
            let input = view.node_input_state.clone().expect("node input");
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
        });
        window.input("ws://desktop", cx);
        view.update(cx, |view, cx| {
            let input = view.node_input_state.clone().expect("node input");
            assert_eq!(input.read(cx).value().to_string(), "ws://desktop");
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn touch_tap_on_command_palette_row_padding_focuses_palette_input(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.command_palette_open = true;
        view
    });
    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            view.composer_focus_handle.focus(window, cx);
        });
        window.render_frame(cx);

        let bounds = window.find("command-palette-input").bounds();
        // Tap in the row padding to the left of the input (over the search icon area).
        touch_tap_at(
            window,
            1,
            point(bounds.origin.x - px(6.), bounds.center().y),
            cx,
        );
        view.update(cx, |view, cx| {
            let input = view
                .command_palette_input
                .clone()
                .expect("command palette input");
            assert!(
                input.read(cx).focus_handle(cx).is_focused(window),
                "tapping the command palette search row should focus its input"
            );
        });
        // Tap the outer padding again while already focused (exercises the
        // `!window.default_prevented()` re-request branch).
        touch_tap_at(
            window,
            2,
            point(bounds.origin.x - px(6.), bounds.center().y),
            cx,
        );
        window.input("settings", cx);
        view.update(cx, |view, cx| {
            let input = view
                .command_palette_input
                .clone()
                .expect("command palette input");
            assert_eq!(input.read(cx).value().to_string(), "settings");
        });
    })
    .unwrap();
}
