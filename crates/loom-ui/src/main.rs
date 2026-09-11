//! Native Loom client.
//!
//! The client is split into a protocol client (`connection`), client-side
//! projections (`state`), the GPUI view (`view`), input handling
//! (`text_input`), window chrome (`theme`), and native platform adapters
//! (`platform`).

mod connection;
mod platform;
mod state;
mod text_input;
mod theme;
mod view;

use gpui::{
    App, Bounds, KeyBinding, TitlebarOptions, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowOptions, point, prelude::*, px, size,
};

use crate::{
    platform::UiOptions,
    state::ThemeChoice,
    text_input::{Backspace, Copy, Delete, End, Home, Left, Paste, Right, SelectAll, Submit},
    view::LoomView,
};

pub(crate) const MAX_TIMELINE_OUTPUT: usize = 32 * 1024;
pub(crate) const MAX_REVIEW_CHANGES: usize = 80;
pub(crate) const MAX_REVIEW_DIFF: usize = 48 * 1024;

fn main() {
    let options = match UiOptions::parse(std::env::args()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("could not parse Loom UI arguments: {error}");
            std::process::exit(1);
        }
    };
    gpui_platform::application()
        .with_assets(gpui_kit_assets::AllAssets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
            cx.bind_keys([
                KeyBinding::new("backspace", Backspace, Some("Composer")),
                KeyBinding::new("delete", Delete, Some("Composer")),
                KeyBinding::new("left", Left, Some("Composer")),
                KeyBinding::new("right", Right, Some("Composer")),
                KeyBinding::new("cmd-a", SelectAll, Some("Composer")),
                KeyBinding::new("ctrl-a", SelectAll, Some("Composer")),
                KeyBinding::new("home", Home, Some("Composer")),
                KeyBinding::new("end", End, Some("Composer")),
                KeyBinding::new("cmd-v", Paste, Some("Composer")),
                KeyBinding::new("ctrl-v", Paste, Some("Composer")),
                KeyBinding::new("cmd-c", Copy, Some("Composer")),
                KeyBinding::new("ctrl-c", Copy, Some("Composer")),
                KeyBinding::new("enter", Submit, Some("Composer")),
            ]);
            let view = match LoomView::try_new(&options, cx.focus_handle(), cx.focus_handle()) {
                Ok(view) => view,
                Err(error) => {
                    eprintln!("could not initialize Loom UI: {error}");
                    cx.quit();
                    return;
                }
            };
            let bounds = Bounds::centered(None, size(px(1200.), px(780.)), cx);
            let window = match cx.open_window(
                WindowOptions {
                    focus: true,
                    titlebar: Some(TitlebarOptions {
                        title: None,
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(9.), px(9.))),
                    }),
                    app_owns_titlebar_drag: true,
                    window_background: WindowBackgroundAppearance::Transparent,
                    window_decorations: Some(WindowDecorations::Client),
                    is_movable: true,
                    is_resizable: true,
                    is_minimizable: true,
                    window_min_size: Some(size(px(720.), px(480.))),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| view),
            ) {
                Ok(window) => window,
                Err(error) => {
                    eprintln!("failed to open Loom window: {error}");
                    cx.quit();
                    return;
                }
            };
            if let Err(error) = window.update(cx, |view, window, cx| {
                view.composer_focus_handle.focus(window, cx);
                view.select_theme(ThemeChoice::System, window, cx);
                cx.activate(true);
            }) {
                eprintln!("failed to focus Loom composer: {error}");
                cx.quit();
            }
        });
}
