#![cfg_attr(target_family = "wasm", no_main)]

//! Native Loom client.
//!
//! The client is split into a protocol client (`connection`), client-side
//! projections (`state`), the GPUI view (`view`), input handling
//! (`text_input`), window chrome (`theme`), and native platform adapters
//! (`platform`).

#[cfg(target_family = "wasm")]
mod browser;
mod connection;
#[cfg(not(target_family = "wasm"))]
mod platform;
mod state;
mod text_input;
mod theme;
mod view;

#[cfg(target_family = "wasm")]
use crate::{browser::BrowserOptions, view::LoomView};
use gpui::{App, KeyBinding, prelude::*};
#[cfg(not(target_family = "wasm"))]
use gpui::{
    Bounds, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowDecorations,
    WindowOptions, point, px, size,
};

use crate::text_input::{
    Backspace, Copy, Delete, End, Home, Left, Paste, Right, SelectAll, Submit,
};
#[cfg(not(target_family = "wasm"))]
use crate::{platform::UiOptions, state::ThemeChoice, view::LoomView};

pub(crate) const MAX_TIMELINE_OUTPUT: usize = 32 * 1024;
pub(crate) const MAX_REVIEW_CHANGES: usize = 80;
pub(crate) const MAX_REVIEW_DIFF: usize = 48 * 1024;

/// Registers the composer's key bindings. Shared by native `main` and the
/// browser's `start`, since the same [`crate::text_input::TextInputElement`]
/// handles typing on both platforms.
fn bind_composer_keys(cx: &mut App) {
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
}

#[cfg(not(target_family = "wasm"))]
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
            bind_composer_keys(cx);
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

// The browser has no native event loop for `Platform::run` to block on, so
// `Application::run` returns immediately after launching and would drop the
// app (and everything it owns) before any frame is ever painted. We keep the
// app alive for the lifetime of the page by stashing the `ApplicationHandle`
// returned by `run_embedded` in a thread-local; dropping it is unnecessary
// since the wasm module lives as long as the page does.
#[cfg(target_family = "wasm")]
thread_local! {
    static APPLICATION: std::cell::RefCell<Option<gpui::ApplicationHandle>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(target_family = "wasm")]
fn log_error(context: &str, error: impl std::fmt::Display) {
    web_sys::console::error_1(&format!("Loom UI: {context}: {error}").into());
}

/// Connects to the remote backend and opens the same `LoomView` the native
/// client uses, once bootstrap has resolved a project/session/model. This
/// runs as a foreground task (not `background_spawn`) since it drives the
/// `!Send` browser WebSocket transport directly.
#[cfg(target_family = "wasm")]
async fn start_browser_client(cx: &mut gpui::AsyncApp) {
    let options = match BrowserOptions::from_location() {
        Ok(options) => options,
        Err(error) => return log_error("could not read startup options", error),
    };
    let (composer_focus_handle, rename_focus_handle) =
        cx.update(|cx| (cx.focus_handle(), cx.focus_handle()));
    let view = match LoomView::try_new_browser(&options, composer_focus_handle, rename_focus_handle)
        .await
    {
        Ok(view) => view,
        Err(error) => return log_error("could not connect to the backend", error),
    };
    let active_session = view.active_session.clone();
    let window = match cx.open_window(Default::default(), |_, cx| cx.new(|_| view)) {
        Ok(window) => window,
        Err(error) => return log_error("failed to open Loom window", error),
    };
    let updated = window.update(cx, |view, window, cx| {
        view.composer_focus_handle.focus(window, cx);
        view.reload_sessions(cx);
        view.refresh_models_async(cx);
        view.select_session(active_session, cx);
        cx.activate(true);
    });
    if let Err(error) = updated {
        log_error("failed to finish initializing the Loom window", error);
    }
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    let application =
        gpui_platform::application_with_web_backend(gpui_platform::WebBackendPreference::WebGl)
            .with_assets(gpui_kit_assets::Assets::default())
            .run_embedded(|cx: &mut App| {
                // The web platform starts with an empty font database; without this the
                // text system panics as soon as it tries to shape any text.
                cx.text_system()
                    .add_fonts(vec![std::borrow::Cow::Borrowed(
                        include_bytes!("../assets/fonts/DejaVuSans.ttf").as_slice(),
                    )])
                    .expect("failed to load embedded font");
                gpui_component::init(cx);
                gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
                bind_composer_keys(cx);
                cx.spawn(async move |cx| start_browser_client(cx).await)
                    .detach();
            });
    APPLICATION.with(|slot| *slot.borrow_mut() = Some(application));
}
