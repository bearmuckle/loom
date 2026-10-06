//! Visual regression tests that rasterize the Loom view through GPUI's
//! cross-platform headless renderer.
//!
//! The state, focus and layout suites drive `TestAppContext`, which mocks
//! rendering and therefore cannot notice a control that paints invisibly or an
//! icon whose asset is missing. These tests render real off-screen frames and
//! inspect the captured pixels, following `gpui-kit`'s own `rendering` target.
//!
//! On Linux the renderer is backed by wgpu, so a Vulkan or GL adapter must be
//! available. CI installs Mesa's lavapipe software driver for this. Platforms
//! without a `PlatformHeadlessRenderer` (Windows, wasm) skip the module
//! entirely.
#![cfg(target_os = "linux")]

use super::*;
use std::borrow::Cow;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use crate::state::{AssistantTurn, TimelineItem};
use gpui_kit::platform::{current_headless_renderer, current_platform};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AssetSource, Bounds, HeadlessAppContext, PlatformTextSystem, SharedString, Size, WindowBounds,
    WindowOptions, px, size,
};

fn view_size() -> Size<Pixels> {
    size(px(1280.), px(800.))
}

/// One real text system for the whole test binary. Building a platform just to
/// take its text system leaks the platform's worker and timer threads on
/// Linux, which `gpui` itself avoids the same way for benchmarks.
fn text_system() -> Arc<dyn PlatformTextSystem> {
    static TEXT_SYSTEM: OnceLock<Arc<dyn PlatformTextSystem>> = OnceLock::new();
    TEXT_SYSTEM
        .get_or_init(|| current_platform(true).text_system())
        .clone()
}

/// All icons replaced with valid but empty SVGs, so the harness can prove it
/// notices a missing icon asset. Everything else delegates to the real bundle.
struct BlankIcons;

impl AssetSource for BlankIcons {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if path.ends_with(".svg") {
            Ok(Some(Cow::Borrowed(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24"/>"#,
            )))
        } else {
            gpui_kit::assets::Assets.load(path)
        }
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

/// A captured frame as raw RGBA bytes.
struct Frame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Frame {
    /// The number of distinct RGBA colors; a flat fill yields one.
    fn distinct_colors(&self) -> usize {
        self.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| u32::from_be_bytes(*pixel))
            .collect::<HashSet<_>>()
            .len()
    }

    /// A cheap digest for comparisons, so a failure does not print the whole
    /// frame to the test log.
    fn fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.width.hash(&mut hasher);
        self.height.hash(&mut hasher);
        self.rgba.hash(&mut hasher);
        hasher.finish()
    }

    /// GPUI renders at the platform scale factor, so only the aspect ratio of
    /// the logical window is fixed.
    fn has_aspect_ratio(&self, logical_width: u32, logical_height: u32) -> bool {
        self.width * logical_height == self.height * logical_width
    }
}

/// Opens the Loom view in a headless window, renders one frame and returns its
/// pixels. `configure` mutates the view before the first frame, matching the
/// `render_scenario` helper the interaction suites use.
fn capture_with(
    window_size: Size<Pixels>,
    assets: Arc<dyn AssetSource>,
    configure: impl FnOnce(&mut LoomView),
) -> Frame {
    let mut cx =
        HeadlessAppContext::with_platform(text_system(), assets, current_headless_renderer);
    cx.update(gpui_kit::init);

    let (handle, _view) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Default::default(),
                        size: window_size,
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |_window, cx| {
                    cx.new(|cx| {
                        let mut view = LoomView::new_for_test(cx.focus_handle());
                        configure(&mut view);
                        view
                    })
                },
            )
        })
        .expect("open headless window");

    cx.update_window(handle, |_, window, cx| window.render_frame(cx))
        .expect("render frame");

    let image = cx
        .capture_screenshot(handle)
        .expect("GPUI headless renderer must be available");
    Frame {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

fn capture(configure: impl FnOnce(&mut LoomView)) -> Frame {
    capture_with(view_size(), Arc::new(gpui_kit::assets::Assets), configure)
}

#[test]
fn headless_renderer_captures_the_empty_view() {
    let frame = capture(|_| {});
    assert!(
        frame.width > 0 && frame.height > 0,
        "the captured frame must not be empty"
    );
    assert!(
        frame.has_aspect_ratio(1280, 800),
        "the captured frame should keep the 1280x800 window aspect ratio"
    );
    assert!(
        frame.distinct_colors() > 8,
        "the empty view should paint chrome and text, not a flat fill"
    );
}

#[test]
fn headless_rendering_is_deterministic_across_contexts() {
    let first = capture(|_| {});
    let second = capture(|_| {});
    assert_eq!(
        first.fingerprint(),
        second.fingerprint(),
        "identical views must render identical frames"
    );
}

#[test]
fn headless_renderer_detects_a_visual_state_change() {
    let baseline = capture(|_| {});
    let settings = capture(|view| view.settings_open = true);
    assert_ne!(
        baseline.fingerprint(),
        settings.fingerprint(),
        "opening settings should change the rendered frame"
    );
}

#[test]
fn every_settings_section_paints_over_the_empty_view() {
    let empty = capture(|_| {});
    for section in [
        SettingsSection::Agents,
        SettingsSection::Providers,
        SettingsSection::Workers,
        SettingsSection::Appearance,
        SettingsSection::About,
    ] {
        let frame = capture(move |view| {
            view.settings_open = true;
            view.settings_section = section;
        });
        assert_ne!(
            frame.fingerprint(),
            empty.fingerprint(),
            "settings section {section:?} did not paint over the empty view"
        );
    }
}

#[test]
fn transcript_messages_change_the_rendered_frame() {
    let empty = capture(|_| {});
    let short = capture(|view| {
        view.sessions = vec![view.active_session.clone()];
        view.timeline = vec![TimelineItem::User("Run the tests".to_owned())];
    });
    let long = capture(|view| {
        view.sessions = vec![view.active_session.clone()];
        view.timeline = vec![
            TimelineItem::User("Run the tests".to_owned()),
            TimelineItem::Assistant(AssistantTurn::text(
                "The parser test fails because the fixture omits a required field.",
            )),
        ];
    });
    assert_ne!(
        short.fingerprint(),
        empty.fingerprint(),
        "a user message did not paint"
    );
    assert_ne!(
        long.fingerprint(),
        short.fingerprint(),
        "an assistant reply did not paint"
    );
}

#[test]
fn review_drawer_and_command_palette_change_the_frame() {
    let empty = capture(|_| {});
    let session = capture(|view| view.sessions = vec![view.active_session.clone()]);
    let review = capture(|view| {
        view.sessions = vec![view.active_session.clone()];
        view.review.open = true;
    });
    let palette = capture(|view| view.command_palette_open = true);
    assert_ne!(
        session.fingerprint(),
        empty.fingerprint(),
        "selecting a session did not paint"
    );
    assert_ne!(
        review.fingerprint(),
        session.fingerprint(),
        "the review drawer did not paint"
    );
    assert_ne!(
        palette.fingerprint(),
        empty.fingerprint(),
        "the command palette did not paint"
    );
}

#[test]
fn missing_icon_assets_change_the_frame() {
    let real = capture(|_| {});
    let blank = capture_with(view_size(), Arc::new(BlankIcons), |_| {});
    assert_ne!(
        real.fingerprint(),
        blank.fingerprint(),
        "replacing every icon with an empty SVG must change the frame"
    );
}

#[test]
fn phone_layout_renders_a_distinct_frame() {
    let desktop = capture(|_| {});
    let phone = capture_with(
        size(px(420.), px(800.)),
        Arc::new(gpui_kit::assets::Assets),
        |_| {},
    );
    assert!(
        phone.has_aspect_ratio(420, 800),
        "the phone frame should keep the 420x800 aspect ratio"
    );
    assert_ne!(
        desktop.fingerprint(),
        phone.fingerprint(),
        "the phone layout should differ from the desktop layout"
    );
}
