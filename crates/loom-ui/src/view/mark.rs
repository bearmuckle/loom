//! The Loom mark that stands in for a generic loading spinner.
//!
//! The mark is the two bars of the app icon (`pwa/icon.svg`): a vertical warp
//! thread and a horizontal weft thread. While the client waits on work it
//! weaves — the weft rides up the warp and the pair breathes — so an active
//! session reads as Loom working instead of as a rotating generic loader.
//!
//! The motion loops through [`Animation::repeat_synced`], which GPUI reduces to
//! a single static frame when the platform asks for reduced motion. Progress
//! zero, the reduced-motion and rest pose, is the icon itself: fully opaque with
//! the weft in its icon position.

use std::time::Duration;

use gpui_kit::prelude::*;
use gpui_kit::{Animation, AnimationExt as _, AnyElement, Rgba, TestSupportExt as _, div, px};

/// The square the mark draws in. It is the size of the small spinner the run
/// status line used before, so the status row keeps its height.
pub(crate) const LOOM_MARK_SIZE: f32 = 14.;

/// One weave. Slow on purpose: a run can last minutes, so the indicator must
/// read as alive without asking for attention.
const LOOM_WEAVE_PERIOD: Duration = Duration::from_millis(2_400);

/// How far the weft rides up the warp, as a fraction of the mark size.
const WEFT_TRAVEL: f32 = 0.09;

/// How much of its opacity the mark gives up at the top of the weave.
const BREATH_DEPTH: f32 = 0.35;

/// How often the weave re-renders. The motion is slow, so a low cap keeps the
/// indicator from waking the renderer every frame of a long run.
const LOOM_WEAVE_MAX_FPS: f32 = 30.;

/// A rounded bar of the mark, in pixels, relative to the mark's top-left
/// corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MarkBar {
    pub(crate) left: f32,
    pub(crate) top: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) corner: f32,
}

/// The warp (vertical) and weft (horizontal) bars for a mark drawn in a
/// `size`-px square, in icon order.
///
/// The coordinates are the app icon's, which draws both bars `88` wide with a
/// `34` corner radius in a `512` unit view box. Scaling them rather than
/// redrawing the mark keeps the indicator, the launcher icon, and the browser
/// icon the same shape.
pub(crate) fn loom_mark_bars(size: f32) -> [MarkBar; 2] {
    const VIEW_BOX: f32 = 512.;
    const CORNER: f32 = 34.;
    let scale = size / VIEW_BOX;
    let bar = |left: f32, top: f32, width: f32, height: f32| MarkBar {
        left: left * scale,
        top: top * scale,
        width: width * scale,
        height: height * scale,
        corner: CORNER * scale,
    };
    [
        // The warp runs down the left of the mark.
        bar(150., 128., 88., 252.),
        // The weft crosses it near the foot.
        bar(150., 296., 220., 88.),
    ]
}

/// One weave as a single wave that starts and ends at rest, so a looping
/// animation never jumps: `0.0` at progress `0` and `1`, `1.0` at `0.5`.
fn weave_wave(progress: f32) -> f32 {
    0.5 - 0.5 * (std::f32::consts::TAU * progress).cos()
}

/// How far above its icon position the weft sits at `progress`, in pixels.
pub(crate) fn weft_rise(size: f32, progress: f32) -> f32 {
    -size * WEFT_TRAVEL * weave_wave(progress)
}

/// The mark's opacity at `progress`: fully opaque at rest, dimming as the weft
/// lifts so the weave reads as a thread being drawn.
pub(crate) fn mark_breath(progress: f32) -> f32 {
    1.0 - BREATH_DEPTH * weave_wave(progress)
}

/// The animated Loom mark, tinted with the accent `color` of the work it
/// reports.
///
/// Only one mark is on screen at a time — the composer's run status line — so
/// the animation ids are fixed rather than indexed.
pub(crate) fn loom_mark(color: Rgba) -> AnyElement {
    let [warp, weft] = loom_mark_bars(LOOM_MARK_SIZE);
    let bar = move |bar: MarkBar| {
        div()
            .absolute()
            .left(px(bar.left))
            .top(px(bar.top))
            .w(px(bar.width))
            .h(px(bar.height))
            .rounded(px(bar.corner))
            .bg(color)
    };
    div()
        .id("loom-mark")
        .test_support()
        .relative()
        .w(px(LOOM_MARK_SIZE))
        .h(px(LOOM_MARK_SIZE))
        .flex_shrink_0()
        .child(bar(warp))
        .child(
            bar(weft).with_animation(
                "loom-mark-weft",
                Animation::new(LOOM_WEAVE_PERIOD)
                    .repeat_synced()
                    .with_max_fps(LOOM_WEAVE_MAX_FPS),
                move |element, progress| {
                    element.top(px(weft.top + weft_rise(LOOM_MARK_SIZE, progress)))
                },
            ),
        )
        .with_animation(
            "loom-mark-breath",
            Animation::new(LOOM_WEAVE_PERIOD)
                .repeat_synced()
                .with_max_fps(LOOM_WEAVE_MAX_FPS),
            |element, progress| element.opacity(mark_breath(progress)),
        )
        .into_any()
}
