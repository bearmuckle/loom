//! Render tests for the browser's startup and reconnect card.
//!
//! `render_startup_screen` runs in the browser, where these tests cannot go, so
//! the screen is compiled on native test builds too and rendered here through a
//! `LoomView` in the disconnected state. The card is the whole screen: a
//! sidebar or an empty workspace pane beside it is what made startup read as a
//! half-loaded client, so the card has to stay centred and its controls have to
//! stay inside the viewport on a phone.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{Context, Entity, Render, TestAppContext, Window, WindowHandle, px, size};

/// A close reason long enough to overflow the card when the copy cannot wrap.
const LONG_CLOSE_REASON: &str = "the transport reported an unexpected end of stream while the worker was draining its queue and no close frame arrived 0123456789abcdefghijklmnopqrstuvwxyz";

/// The transport failure a lost worker reports.
fn closed_connection() -> LoomError {
    LoomError::new(ErrorCode::RequestCancelled, LONG_CLOSE_REASON, true)
}

/// A window that renders only the screen under test, so the workspace behind it
/// cannot satisfy or break the assertions.
struct StatusScreenHost {
    view: Entity<LoomView>,
}

impl Render for StatusScreenHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.view
            .update(cx, |view, cx| view.render_startup_screen(window, cx))
    }
}

/// Opens a window on a view that lost its worker, with an automatic reconnect
/// in flight.
fn open_status_window(
    cx: &mut TestAppContext,
    width: f32,
    height: f32,
    configure: impl FnOnce(&mut LoomView),
) -> WindowHandle<StatusScreenHost> {
    cx.open_window(size(px(width), px(height)), |_, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.browser_connection_lost = Some(LONG_CLOSE_REASON.to_owned());
            view.reconnecting = Some(Reconnecting::new(
                ConnectivityFailure::ConnectionLost,
                &closed_connection(),
            ));
            configure(&mut view);
            view
        });
        StatusScreenHost { view }
    })
}

/// Whether `element` is painted inside `card`.
fn inside(
    element: gpui_kit::Bounds<gpui_kit::Pixels>,
    card: gpui_kit::Bounds<gpui_kit::Pixels>,
) -> bool {
    element.left() >= card.left()
        && element.right() <= card.right()
        && element.top() >= card.top()
        && element.bottom() <= card.bottom()
}

#[gpui_kit::test]
fn the_reconnect_screen_is_one_centred_card(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = open_status_window(cx, 1280., 800., |_| {});
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        let card = window.find("workspace-status-card").bounds();
        // Centred on both axes: a sidebar or an empty workspace pane beside the
        // card would push it off centre, which is what made startup read as a
        // half-loaded client.
        let centre_x = card.left() + card.size.width * 0.5;
        let centre_y = card.top() + card.size.height * 0.5;
        assert!(
            (centre_x - viewport.width * 0.5).abs() <= px(1.),
            "the card is not horizontally centred: {card:?} in {viewport:?}"
        );
        assert!(
            (centre_y - viewport.height * 0.5).abs() <= px(1.),
            "the card is not vertically centred: {card:?} in {viewport:?}"
        );
        // One bounded card, so the copy and the controls keep a readable width
        // instead of stretching across a desktop window.
        assert_eq!(card.size.width, px(420.));
        assert!(card.size.height < viewport.height * 0.5, "{card:?}");

        // The spinner, the heading, the one detail line and the one progress
        // line all live inside the card, in that order.
        let spinner = window.find("workspace-status-spinner").bounds();
        let heading = window.find("workspace-status-heading").bounds();
        let detail = window.find("workspace-status-detail").bounds();
        let progress = window.find("workspace-status-progress").bounds();
        for (id, element) in [
            ("workspace-status-spinner", spinner),
            ("workspace-status-heading", heading),
            ("workspace-status-detail", detail),
            ("workspace-status-progress", progress),
        ] {
            assert!(inside(element, card), "{id} escaped the card: {element:?}");
        }
        assert!(
            spinner.bottom() <= heading.top(),
            "the spinner follows the heading"
        );
        assert!(
            heading.bottom() <= detail.top(),
            "the detail follows the heading"
        );
        assert!(
            detail.bottom() <= progress.top(),
            "the progress follows the detail"
        );

        // Retry now and Settings sit under the copy, inside the card.
        let retry = window.find("workspace-status-primary");
        let settings = window.find("workspace-status-settings");
        assert!(retry.visible(), "the retry control is missing");
        assert!(settings.visible(), "the settings control is missing");
        let retry = retry.bounds();
        let settings = settings.bounds();
        assert!(
            inside(retry, card),
            "the retry control escaped the card: {retry:?}"
        );
        assert!(
            inside(settings, card),
            "the settings control escaped the card: {settings:?}"
        );
        assert!(
            progress.bottom() <= retry.top(),
            "the controls follow the progress line"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn phone_reconnect_controls_stay_inside_the_card_and_the_viewport(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = open_status_window(cx, 390., 844., |_| {});
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        let card = window.find("workspace-status-card").bounds();
        let retry = window.find("workspace-status-primary").bounds();
        let settings = window.find("workspace-status-settings").bounds();
        for (id, element) in [
            ("workspace-status-card", card),
            ("workspace-status-primary", retry),
            ("workspace-status-settings", settings),
        ] {
            assert!(
                element.size.width > px(0.),
                "{id} collapsed to nothing: {element:?}"
            );
            assert!(
                element.right() <= viewport.width,
                "{id} ran off the right edge: {element:?}"
            );
            assert!(
                element.bottom() <= viewport.height,
                "{id} ran off the bottom: {element:?}"
            );
        }
        // Phone controls use finger-sized targets.
        assert_eq!(retry.size.height, px(44.));
        assert_eq!(settings.size.width, px(44.));
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_narrow_phone_wraps_the_copy_instead_of_overflowing_the_card(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = open_status_window(cx, 280., 600., |_| {});
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        let card = window.find("workspace-status-card").bounds();
        assert!(
            card.bottom() <= viewport.height,
            "the card ran off the bottom: {card:?}"
        );
        let detail = window.find("workspace-status-detail").bounds();
        assert!(
            detail.size.height > px(30.),
            "the detail line did not wrap inside the card: {detail:?}"
        );
        assert!(
            inside(detail, card),
            "the detail escaped the card: {detail:?}"
        );
        let retry = window.find("workspace-status-primary").bounds();
        assert!(
            inside(retry, card),
            "the retry control escaped the card: {retry:?}"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_spent_reconnect_schedule_stops_the_spinner_and_keeps_the_retry(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = open_status_window(cx, 1280., 800., |view| {
        if let Some(state) = view.reconnecting.as_mut() {
            while state.take_attempt().is_some() {}
        }
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // The schedule stopped on its own, so the card stops claiming work is
        // in flight and offers the control that starts it again.
        assert!(
            window.try_find("workspace-status-spinner").is_none(),
            "a spent schedule must not keep spinning"
        );
        assert!(
            window.find("workspace-status-primary").visible(),
            "a spent schedule has to leave a way to try again"
        );
        assert!(
            window.find("workspace-status-progress").visible(),
            "the card says the automatic attempts stopped"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_startup_in_flight_shows_progress_and_offers_no_actions(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = open_status_window(cx, 1280., 800., |view| {
        // A bootstrap in flight is progress, not a failure: no reconnect, no
        // settings, and no raw details.
        view.browser_starting = true;
        view.browser_connection_lost = None;
        view.reconnecting = None;
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.find("workspace-status-spinner").visible(),
            "a startup in flight has to report that it is working"
        );
        for id in [
            "workspace-status-primary",
            "workspace-status-settings",
            "workspace-status-details",
            "workspace-status-progress",
        ] {
            assert!(
                window.try_find(id).is_none(),
                "{id} is offered while the client is still connecting"
            );
        }
    })
    .unwrap();
}
