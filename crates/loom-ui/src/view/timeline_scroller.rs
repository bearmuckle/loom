//! The conversation's virtual message list.
//!
//! The transcript scrolls through Loom's own scroller rather than
//! `gpui_kit::component::message_scroller`, because that component hands every
//! platform scroll packet to a GPUI list unchanged and decides "the reader is
//! at the live edge" with an exact offset comparison. Two reported behaviours
//! follow from that:
//!
//! - A mouse wheel notch in the browser client arrives as roughly a hundred
//!   pixels, and a fast flick as several hundred, so the list skips over a
//!   whole message instead of scrolling through it. Here one packet moves at
//!   most [`WHEEL_STEP_LIMIT`], and a wheel line counts as one line of body
//!   text.
//! - The list can stop a fraction of a pixel, or its own bottom padding, short
//!   of its maximum offset, and it measures rows lazily, so its content height
//!   leaves out every row it has not rendered. Either way the "jump to latest"
//!   button and the bottom fade can hover over the newest message for good.
//!   Here the live edge is "the newest row is fully in the viewport", and a
//!   wheel packet that runs past it snaps the tail flush.

use std::{ops::Range, time::Duration};

use gpui_kit::base::motion::{Transition, transition};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, button::Button, button::ButtonVariants as _,
    scroll::ScrollableElement as _,
};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, FollowMode, Hsla, InteractiveElement as _,
    IntoElement, ListAlignment, ListState, ParentElement as _, Pixels, RenderOnce, Role,
    ScrollWheelEvent, SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    linear_color_stop, linear_gradient, list, prelude::FluentBuilder as _, px, rems,
};

/// Rows kept rendered beyond the viewport, so a wheel step does not have to
/// measure what it reveals.
const LIST_OVERDRAW: Pixels = px(400.);
/// Horizontal padding around a row.
const ROW_INSET: Pixels = px(12.);
/// Space after every row, including the newest one.
///
/// The gap is uniform on purpose: a row whose "last row" status changes would
/// otherwise change its measured height on every append, and the list would
/// re-anchor the scroll on heights it has just invalidated.
const ROW_GAP: Pixels = px(32.);
/// Vertical padding of the list itself.
const LIST_PADDING: Pixels = px(8.);
/// One wheel line, which is one line of body text.
const WHEEL_LINE: Pixels = px(16.);
/// The most a single scroll packet may move the transcript, about three lines.
const WHEEL_STEP_LIMIT: Pixels = px(48.);
/// Distance of the jump button from the scroller's bottom edge.
const JUMP_BUTTON_INSET: Pixels = px(8.);
const JUMP_BUTTON_TRANSITION: Duration = Duration::from_millis(200);
const BOTTOM_FADE_TRANSITION: Duration = Duration::from_millis(200);
const BOTTOM_FADE_HEIGHT: f32 = 3.;

/// The entity-owned scrolling state for a [`TimelineScroller`].
///
/// The state owns only GPUI's virtual-list bookkeeping. Message data stays with
/// the caller and is read by the row renderer passed to
/// [`TimelineScroller::new`].
pub(crate) struct TimelineScrollerState {
    list_state: ListState,
}

impl TimelineScrollerState {
    /// Create a state for `item_count` rows and follow the newest one.
    ///
    /// The constructor receives the entity context so the list's scroll
    /// handler can safely defer its entity update until GPUI has released the
    /// list's internal borrow.
    pub(crate) fn new(item_count: usize, cx: &mut Context<Self>) -> Self {
        let list_state = ListState::new(item_count, ListAlignment::Top, LIST_OVERDRAW);
        list_state.set_follow_mode(FollowMode::Tail);

        let weak_state = cx.weak_entity();
        list_state.set_scroll_handler(move |_, _, cx| {
            let weak_state = weak_state.clone();

            cx.defer(move |cx| {
                let _ = weak_state.update(cx, |_, cx| cx.notify());
            });
        });

        Self { list_state }
    }

    /// Return the number of rows the virtual list knows about.
    pub(crate) fn item_count(&self) -> usize {
        self.list_state.item_count()
    }

    /// Return whether the reader has scrolled away from the newest row.
    ///
    /// The row renderer uses this to load the next older page when the reader
    /// reaches the oldest loaded row, so a list that sits at the live edge must
    /// report `false`.
    pub(crate) fn is_scrolled_up(&self) -> bool {
        !self.list_state.is_following_tail() && !self.newest_row_is_on_screen()
    }

    /// Return whether any part of the newest row is on screen, which is what
    /// "at the live edge" means for the transcript.
    ///
    /// Comparing scroll offsets cannot answer this: the virtual list measures
    /// rows lazily, so its content height leaves out every row it has never
    /// rendered, and the list can stop a fraction of a pixel short of its own
    /// maximum. Asking whether the newest row is still below the viewport
    /// needs only the rows that are on screen.
    pub(crate) fn newest_row_is_on_screen(&self) -> bool {
        let Some(newest) = self.item_count().checked_sub(1) else {
            return true;
        };
        // A newest row that is not rendered is not on screen, so it counts as
        // below the viewport.
        !self
            .list_state
            .item_is_below_viewport(newest)
            .unwrap_or(true)
    }

    /// Reset the list to `item_count` rows and follow the newest one.
    pub(crate) fn reset(&mut self, item_count: usize, cx: &mut Context<Self>) {
        self.list_state.reset(item_count);
        self.list_state.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    /// Append `count` rows to the end of the list.
    pub(crate) fn append(&mut self, count: usize, cx: &mut Context<Self>) -> bool {
        // A reader who is looking at the newest row keeps following it when
        // rows arrive, even if the list never re-engaged tail following on
        // its own.
        if !self.list_state.is_following_tail() && self.newest_row_is_on_screen() {
            self.list_state.set_follow_mode(FollowMode::Tail);
        }

        let item_count = self.list_state.item_count();
        self.splice(item_count..item_count, count, cx)
    }

    /// Prepend `count` rows while preserving the reader's scroll anchor.
    pub(crate) fn prepend(&mut self, count: usize, cx: &mut Context<Self>) -> bool {
        self.splice(0..0, count, cx)
    }

    /// Mark all rows for remeasurement while preserving a proportional anchor.
    pub(crate) fn remeasure(&mut self, cx: &mut Context<Self>) {
        self.list_state.remeasure();
        cx.notify();
    }

    /// Scroll by `distance`, where a positive distance moves toward the newest
    /// row.
    pub(crate) fn scroll_by(&mut self, distance: Pixels, cx: &mut Context<Self>) {
        if distance == px(0.) {
            return;
        }

        self.list_state.scroll_by(distance);
        // A packet that runs past the newest row settles on it: resume tail
        // following so the tail snaps flush instead of stopping short of the
        // bottom edge, which is what left the jump control on screen.
        if distance > px(0.) && self.scrolled_past_the_end() {
            self.list_state.set_follow_mode(FollowMode::Tail);
        }
        cx.notify();
    }

    /// Whether the list is anchored past its last row.
    fn scrolled_past_the_end(&self) -> bool {
        self.list_state.logical_scroll_top().item_ix >= self.item_count()
    }

    /// Resume tail following and scroll to the newest row.
    pub(crate) fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// The row at the top of the viewport and how far into it the list is
    /// scrolled.
    #[cfg(test)]
    pub(crate) fn logical_scroll_top(&self) -> gpui_kit::ListOffset {
        self.list_state.logical_scroll_top()
    }

    /// Whether the list is actively following its newest row.
    #[cfg(test)]
    pub(crate) fn is_following_tail(&self) -> bool {
        self.list_state.is_following_tail()
    }

    fn splice(&mut self, old_range: Range<usize>, count: usize, cx: &mut Context<Self>) -> bool {
        if !self.valid_range(&old_range) {
            return false;
        }

        self.list_state.splice(old_range, count);
        cx.notify();
        true
    }

    fn valid_range(&self, range: &Range<usize>) -> bool {
        range.start <= range.end && range.end <= self.list_state.item_count()
    }
}

/// Renders one transcript row.
type RowRenderer = Box<dyn FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static>;

/// A virtualized transcript list with a scrollbar, a bottom fade, and a
/// jump-to-latest control.
#[derive(IntoElement)]
pub(crate) struct TimelineScroller {
    id: ElementId,
    state: Entity<TimelineScrollerState>,
    renderer: RowRenderer,
    jump_button_label: SharedString,
    bottom_fade: Option<Hsla>,
}

impl TimelineScroller {
    /// Create a transcript scroller with a renderer for each row.
    pub(crate) fn new<E>(
        id: impl Into<ElementId>,
        state: Entity<TimelineScrollerState>,
        renderer: impl FnMut(usize, &mut Window, &mut App) -> E + 'static,
    ) -> Self
    where
        E: IntoElement,
    {
        let mut renderer = renderer;
        Self {
            id: id.into(),
            state,
            renderer: Box::new(move |index, window, cx| {
                renderer(index, window, cx).into_any_element()
            }),
            jump_button_label: "Jump to latest".into(),
            bottom_fade: None,
        }
    }

    /// Set the label used by the jump-to-latest control.
    pub(crate) fn with_jump_button_label(mut self, label: impl Into<SharedString>) -> Self {
        self.jump_button_label = label.into();
        self
    }

    /// Fade the transcript's bottom edge into `color`.
    ///
    /// The fade shows only while the reader is away from the live edge, where
    /// a partially visible row would otherwise clip mid-line.
    pub(crate) fn with_bottom_fade(mut self, color: impl Into<Hsla>) -> Self {
        self.bottom_fade = Some(color.into());
        self
    }
}

impl RenderOnce for TimelineScroller {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id: root_id,
            state,
            mut renderer,
            jump_button_label,
            bottom_fade,
        } = self;
        let (list_state, scrolled_up) = {
            let state = state.read(cx);
            (state.list_state.clone(), state.is_scrolled_up())
        };
        let jump_button_visibility = transition(
            (root_id.clone(), "jump-button-visibility"),
            if scrolled_up { 1. } else { 0. },
            Transition::new(JUMP_BUTTON_TRANSITION),
            window,
            cx,
        );
        // At the live edge nothing is clipped below, so a visible fade would
        // suggest content that is not there.
        let bottom_fade_visibility = if bottom_fade.is_some() {
            transition(
                (root_id.clone(), "bottom-fade-visibility"),
                if scrolled_up { 1. } else { 0. },
                Transition::new(BOTTOM_FADE_TRANSITION),
                window,
                cx,
            )
        } else {
            0.
        };
        let tokens = cx.theme().semantic_tokens();
        let jump_button_radius = cx.theme().radius_full();

        let row_state = state.downgrade();
        let list = list(list_state.clone(), move |index, window, cx| {
            div()
                .w_full()
                .min_w_0()
                .px(ROW_INSET)
                .pb(ROW_GAP)
                // A row is where a scroll packet lands, and a row's handler
                // runs before the list's in the bubble phase, so Loom's bounded
                // step can take the packet and stop the list from applying the
                // platform's raw delta.
                .on_scroll_wheel({
                    let state = row_state.clone();
                    move |event, _, cx| {
                        let Some(distance) = wheel_scroll_distance(event) else {
                            return;
                        };

                        // The packet is Loom's even when it is a zoom gesture
                        // that moves nothing: the list must not scroll with the
                        // raw delta either way.
                        cx.stop_propagation();
                        if distance == px(0.) {
                            return;
                        }

                        let _ = state.update(cx, |state, cx| state.scroll_by(distance, cx));
                    }
                })
                .child(renderer(index, window, cx))
                .into_any_element()
        })
        .size_full()
        .min_h_0()
        .py(LIST_PADDING);

        let viewport = div()
            .id((root_id.clone(), "viewport"))
            // Announce appended rows as a log region, like the transcript
            // content of a web chat client.
            .role(Role::Log)
            .size_full()
            .min_h_0()
            .min_w_0()
            // Loom moves the transcript itself from the row handlers above, so
            // the platform overscroll wrapper is deliberately not used: it
            // takes every wheel packet before a row sees it and moves the list
            // by the raw delta.
            .child(list)
            .when_some(
                bottom_fade.filter(|_| bottom_fade_visibility > 0.),
                |this, color| {
                    this.child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(rems(BOTTOM_FADE_HEIGHT))
                            .opacity(bottom_fade_visibility)
                            .bg(linear_gradient(
                                180.,
                                linear_color_stop(color.opacity(0.), 0.),
                                linear_color_stop(color, 1.),
                            )),
                    )
                },
            )
            .vertical_scrollbar(&list_state);

        div()
            .id(root_id.clone())
            .relative()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(viewport)
            .when(jump_button_visibility > 0., |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(JUMP_BUTTON_INSET)
                        .flex()
                        .justify_center()
                        .opacity(jump_button_visibility)
                        .child(
                            Button::new((root_id, "jump-to-latest"))
                                .secondary()
                                .icon(IconName::ArrowDown)
                                .tooltip(jump_button_label)
                                .rounded(jump_button_radius)
                                .border_1()
                                .border_color(tokens.colors.border)
                                .bg(tokens.colors.background)
                                .text_color(tokens.colors.foreground)
                                .on_click({
                                    let state = state.clone();
                                    move |_, _, cx| {
                                        state.update(cx, |state, cx| state.scroll_to_end(cx));
                                    }
                                })
                                .when(!scrolled_up, |button| button.disabled(true)),
                        ),
                )
            })
    }
}

/// The distance one scroll packet moves the transcript, or `None` when the
/// packet does not ask for vertical movement at all.
///
/// A positive distance moves toward the newest row. A zoom gesture (control on
/// Windows and Linux, command on macOS) is not a scroll and returns zero.
fn wheel_scroll_distance(event: &ScrollWheelEvent) -> Option<Pixels> {
    let vertical = event.delta.pixel_delta(WHEEL_LINE).y;
    if vertical == px(0.) {
        return None;
    }

    if event.modifiers.control || event.modifiers.platform {
        return Some(px(0.));
    }

    Some((-vertical).max(-WHEEL_STEP_LIMIT).min(WHEEL_STEP_LIMIT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Modifiers, Render, ScrollDelta, TestAppContext,
        VisualTestContext, point, size,
    };

    /// A transcript whose rows have a fixed height, so the tests can predict
    /// where a wheel packet leaves the scroll offset.
    struct ScrollerHarness {
        state: Entity<TimelineScrollerState>,
    }

    impl Render for ScrollerHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                TimelineScroller::new("test-scroller", self.state.clone(), |_, _, _| {
                    div().h(px(48.)).w_full()
                })
                .with_jump_button_label("Jump to latest"),
            )
        }
    }

    /// Rows are 48px tall plus the scroller's 32px gap, so 40 of them overflow
    /// a 320px viewport by a wide margin.
    const ROW_HEIGHT: f32 = 80.;
    const ROW_COUNT: usize = 40;
    const VIEWPORT_HEIGHT: f32 = 320.;

    fn wheel(delta: Pixels, modifiers: Modifiers) -> ScrollWheelEvent {
        ScrollWheelEvent {
            position: point(px(210.), px(VIEWPORT_HEIGHT / 2.)),
            delta: ScrollDelta::Pixels(point(px(0.), delta)),
            modifiers,
            ..Default::default()
        }
    }

    fn open_harness(cx: &mut TestAppContext) -> (Entity<TimelineScrollerState>, AnyWindowHandle) {
        cx.update(gpui_kit::init);
        let state = cx.new(|cx| TimelineScrollerState::new(ROW_COUNT, cx));
        let window = cx.open_window(size(px(420.), px(VIEWPORT_HEIGHT)), {
            let state = state.clone();
            move |_, _| ScrollerHarness { state }
        });
        (state, window.into())
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    fn scrolled(cx: &mut VisualTestContext, state: &Entity<TimelineScrollerState>) -> Pixels {
        let top = cx.update(|_, cx| state.read(cx).logical_scroll_top());
        px(ROW_HEIGHT) * top.item_ix as f32 + top.offset_in_item
    }

    #[test]
    fn a_wheel_packet_is_bounded_to_a_step_of_text() {
        let notch = wheel(px(-100.), Modifiers::default());
        assert_eq!(wheel_scroll_distance(&notch), Some(WHEEL_STEP_LIMIT));

        let flick = wheel(px(-900.), Modifiers::default());
        assert_eq!(wheel_scroll_distance(&flick), Some(WHEEL_STEP_LIMIT));

        let back = wheel(px(120.), Modifiers::default());
        assert_eq!(wheel_scroll_distance(&back), Some(-WHEEL_STEP_LIMIT));

        let lines = ScrollWheelEvent {
            delta: ScrollDelta::Lines(point(0., -3.)),
            ..Default::default()
        };
        assert_eq!(
            wheel_scroll_distance(&lines),
            Some(WHEEL_LINE + WHEEL_LINE + WHEEL_LINE)
        );
    }

    #[test]
    fn a_zoom_gesture_is_not_a_scroll() {
        let zoom = wheel(
            px(-120.),
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        assert_eq!(wheel_scroll_distance(&zoom), Some(px(0.)));

        let horizontal = ScrollWheelEvent {
            delta: ScrollDelta::Pixels(point(px(-60.), px(0.))),
            ..Default::default()
        };
        assert_eq!(wheel_scroll_distance(&horizontal), None);
    }

    #[gpui_kit::test]
    fn one_wheel_notch_moves_one_step_instead_of_skipping(cx: &mut TestAppContext) {
        let (state, window) = open_harness(cx);
        let mut cx = VisualTestContext::from_window(window, cx);
        draw(&mut cx);

        // Leave the live edge first: following the tail clamps every downward
        // packet, so the measurement has to start from a scrolled-up list.
        for _ in 0..4 {
            cx.simulate_event(wheel(px(900.), Modifiers::default()));
        }
        draw(&mut cx);

        // A browser's pixel-mode notch is about a hundred pixels, and a fast
        // flick is several hundred. Each packet moves the transcript by one
        // bounded step instead, which is less than a single 48+32px row.
        for _ in 0..4 {
            let before = scrolled(&mut cx, &state);
            cx.simulate_event(wheel(px(-900.), Modifiers::default()));
            draw(&mut cx);
            let moved = scrolled(&mut cx, &state) - before;
            assert!(
                moved > px(0.) && moved <= WHEEL_STEP_LIMIT,
                "one packet moved {moved:?}"
            );
        }
    }

    #[gpui_kit::test]
    fn scrolling_to_the_newest_row_retires_the_jump_control(cx: &mut TestAppContext) {
        let (state, window) = open_harness(cx);
        let mut cx = VisualTestContext::from_window(window, cx);
        draw(&mut cx);

        // A couple of packets up, so the newest row is off screen and the jump
        // control is showing the way a reader leaves it.
        for _ in 0..4 {
            cx.simulate_event(wheel(px(900.), Modifiers::default()));
        }
        draw(&mut cx);
        assert!(cx.update(|_, cx| state.read(cx).is_scrolled_up()));

        // Then wheel back down. The transcript must end with the newest row in
        // view rather than a few pixels short of it, which is what left the
        // control on screen over the newest message.
        for _ in 0..ROW_COUNT {
            cx.simulate_event(wheel(px(-900.), Modifiers::default()));
        }
        draw(&mut cx);

        let (scrolled_up, newest_visible) = cx.update(|_, cx| {
            let state = state.read(cx);
            (state.is_scrolled_up(), state.newest_row_is_on_screen())
        });
        assert!(newest_visible);
        assert!(!scrolled_up);

        // A row that arrives while the reader sits on the newest row keeps the
        // reader at the live edge instead of leaving the transcript behind.
        cx.update(|_, cx| {
            state.update(cx, |state, cx| {
                state.append(1, cx);
            });
        });
        draw(&mut cx);
        let (scrolled_up, following) = cx.update(|_, cx| {
            let state = state.read(cx);
            (state.is_scrolled_up(), state.is_following_tail())
        });
        assert!(following);
        assert!(!scrolled_up);
    }
}
