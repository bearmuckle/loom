use super::{header_tooltip, session_header_actions, session_header_title};
use gpui_kit::component::button::Button;
use gpui_kit::test::{TestAppContextExt, TestWindowExt};
use gpui_kit::{Context, TestAppContext, Window, div, prelude::*, px, size};
use std::time::Duration;

struct SessionHeader;

impl Render for SessionHeader {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .child(
                session_header_title()
                    .child("A long session name that must leave room for header actions"),
            )
            .child(
                session_header_actions()
                    .child(header_tooltip(
                        "session-sources-tooltip",
                        "Session sources",
                        Button::new("session-sources").icon(gpui_kit::component::Icon::new(
                            gpui_kit::assets::IconName::ListTree,
                        )),
                    ))
                    .child(Button::new("toggle-review-sidebar").icon(
                        gpui_kit::component::Icon::new(
                            gpui_kit::component::IconName::PanelRightOpen,
                        ),
                    )),
            )
    }
}

#[gpui_kit::test]
fn header_actions_remain_visible_at_desktop_width(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(950.), px(100.)), |_, _| SessionHeader);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let mut previous_right = px(0.);
        for id in ["session-sources", "toggle-review-sidebar"] {
            let action = window.find(id);
            assert!(action.visible(), "{id} should be visible");
            assert!(action.bounds().size.width > px(0.));
            assert!(action.bounds().right() <= window.viewport_size().width);
            assert!(action.bounds().left() >= previous_right);
            previous_right = action.bounds().right();
        }
    })
    .unwrap();
}

#[gpui_kit::test]
async fn header_tooltip_appears_on_hover(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(950.), px(100.)), |_, _| SessionHeader);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.hover("session-sources-tooltip", cx);
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_millis(1000), |window, _| {
        window
            .try_find("loom-header-tooltip")
            .is_some_and(|tooltip| tooltip.visible())
    })
    .await;
}
