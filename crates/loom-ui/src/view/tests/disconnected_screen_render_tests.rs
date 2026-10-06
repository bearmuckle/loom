//! Render tests for the browser disconnected (worker reconnect) screen.
//!
//! The screen itself is compiled only for wasm, where these tests cannot render
//! it, so they compose the layout helpers that `render_disconnected` uses and
//! assert the reconnect controls stay inside the viewport. The narrow-phone
//! case is the regression: the copy, the connection reason and the controls
//! shared one unconstrained row and pushed the controls past the right edge.

use super::{
    DisconnectedScreen, disconnected_footer, disconnected_header, disconnected_header_actions,
    disconnected_header_copy, disconnected_screen, responsive_layout,
};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{Icon, IconName, Sizable};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{Context, TestAppContext, TestSupportExt as _, Window, div, prelude::*, px, size};

/// A close reason long enough to overflow the header when the copy cannot
/// shrink or wrap.
const LONG_CLOSE_REASON: &str = "the transport reported an unexpected end of stream while the worker was draining its queue and no close frame arrived 0123456789abcdefghijklmnopqrstuvwxyz";

/// The header and the status bar of the disconnected screen, composed the way
/// `render_disconnected` composes them.
struct DisconnectedChrome {
    screen: DisconnectedScreen,
}

impl DisconnectedChrome {
    fn with_long_close_reason() -> Self {
        let mut screen = disconnected_screen(Some(LONG_CLOSE_REASON));
        screen.footer = "Disconnected  ·  a deliberately long status line that has to wrap instead of spilling outside its bar";
        Self { screen }
    }
}

impl Render for DisconnectedChrome {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let layout = responsive_layout(window.bounds().size.width);
        let screen = &self.screen;
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                disconnected_header(layout)
                    .child(
                        disconnected_header_copy(layout)
                            .id("disconnected-header-copy")
                            .test_support()
                            .child(screen.heading)
                            .child(div().text_xs().child(screen.detail.clone())),
                    )
                    .child(
                        disconnected_header_actions()
                            .child(
                                Button::new("disconnected-reconnect")
                                    .label("Reconnect")
                                    .small()
                                    .when(layout.phone, |button| button.h(layout.control_size())),
                            )
                            .child(
                                Button::new("disconnected-open-settings")
                                    .icon(Icon::new(IconName::Settings))
                                    .ghost()
                                    .when(layout.phone, |button| {
                                        button
                                            .large()
                                            .h(layout.control_size())
                                            .w(layout.control_size())
                                    })
                                    .when(!layout.phone, |button| button.xsmall()),
                            ),
                    ),
            )
            .child(div().flex_1())
            .child(
                disconnected_footer(layout)
                    .id("disconnected-footer")
                    .test_support()
                    .child(screen.footer),
            )
    }
}

#[gpui_kit::test]
fn phone_reconnect_controls_stack_below_the_copy_inside_the_viewport(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(390.), px(844.)), |_, _| {
        DisconnectedChrome::with_long_close_reason()
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        let copy = window.find("disconnected-header-copy").bounds();
        let reconnect = window.find("disconnected-reconnect");
        let settings = window.find("disconnected-open-settings");
        assert!(
            reconnect.visible(),
            "the reconnect button must be reachable on a phone"
        );
        assert!(
            settings.visible(),
            "the worker settings button must be reachable on a phone"
        );
        let reconnect_bounds = reconnect.bounds();
        assert!(
            reconnect_bounds.top() >= copy.bottom(),
            "the controls should move below the copy on a phone: {reconnect_bounds:?} vs {copy:?}"
        );
        for (id, bounds) in [
            ("disconnected-reconnect", reconnect_bounds),
            ("disconnected-open-settings", settings.bounds()),
        ] {
            assert!(
                bounds.size.width > px(0.),
                "{id} collapsed to nothing: {bounds:?}"
            );
            assert!(
                bounds.right() <= viewport.width,
                "{id} ran off the right edge: {bounds:?}"
            );
            assert!(
                bounds.bottom() <= viewport.height,
                "{id} ran off the bottom: {bounds:?}"
            );
        }
        // Phone controls use finger-sized targets.
        assert_eq!(reconnect_bounds.size.height, px(44.));
        assert_eq!(settings.bounds().size.width, px(44.));
    })
    .unwrap();
}

#[gpui_kit::test]
fn narrow_phone_reconnect_controls_stay_inside_the_viewport(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(280.), px(600.)), |_, _| {
        DisconnectedChrome::with_long_close_reason()
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        for id in ["disconnected-reconnect", "disconnected-open-settings"] {
            let bounds = window.find(id).bounds();
            assert!(
                bounds.size.width > px(0.),
                "{id} collapsed to nothing: {bounds:?}"
            );
            assert!(
                bounds.right() <= viewport.width,
                "{id} ran off the right edge: {bounds:?}"
            );
        }
        // The status bar grows to fit a wrapped line rather than spilling.
        let footer = window.find("disconnected-footer").bounds();
        assert!(
            footer.size.height >= px(24.),
            "the status bar collapsed: {footer:?}"
        );
        assert!(
            footer.bottom() <= viewport.height,
            "the status bar ran off the bottom: {footer:?}"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn desktop_reconnect_controls_share_the_header_row(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, _| {
        DisconnectedChrome::with_long_close_reason()
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        let copy = window.find("disconnected-header-copy").bounds();
        let reconnect = window.find("disconnected-reconnect").bounds();
        assert!(
            reconnect.top() < copy.bottom(),
            "the controls should sit beside the copy on a wide layout: {reconnect:?} vs {copy:?}"
        );
        let settings = window.find("disconnected-open-settings").bounds();
        assert!(
            reconnect.right() <= viewport.width,
            "the reconnect button ran off the right edge: {reconnect:?}"
        );
        assert!(
            settings.right() <= viewport.width,
            "the settings button ran off the right edge: {settings:?}"
        );
        assert!(
            settings.right() > reconnect.right(),
            "the settings button should follow the reconnect button: {settings:?} vs {reconnect:?}"
        );
    })
    .unwrap();
}
