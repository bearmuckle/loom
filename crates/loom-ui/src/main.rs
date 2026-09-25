#![cfg_attr(target_family = "wasm", no_main)]

//! Native Loom client.
//!
//! The client is split into a protocol client (`connection`), client-side
//! projections (`state`), the GPUI view (`view`), input handling
//! (`text_input`), window chrome (`theme`), and native platform adapters
//! (`platform`).

#[cfg(target_family = "wasm")]
mod assets;
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
use gpui_kit::{App, KeyBinding, prelude::*};
#[cfg(not(target_family = "wasm"))]
use gpui_kit::{
    Bounds, TitlebarOptions, WindowAppearance, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowOptions, point, px, size,
};
#[cfg(not(target_family = "wasm"))]
use log::{error, info};

#[cfg(not(target_family = "wasm"))]
use crate::connection::describe_startup_connection_error;
#[cfg(not(target_family = "wasm"))]
use crate::text_input::Paste;
use crate::text_input::{Backspace, Copy, Delete, End, Home, Left, Right, SelectAll, Submit};
#[cfg(not(target_family = "wasm"))]
use crate::{platform::UiOptions, view::LoomView};

#[cfg(not(target_family = "wasm"))]
fn init_logging() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("loom_ui=info"))
        .format_timestamp_millis()
        .init();
}

#[cfg(target_family = "wasm")]
fn init_logging() {
    console_log::init_with_level(log::Level::Info).expect("could not initialize browser logger");
}

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
        // The web backend handles browser paste events directly; its
        // synchronous clipboard read API is unavailable on wasm.
        #[cfg(not(target_family = "wasm"))]
        KeyBinding::new("cmd-v", Paste, Some("Composer")),
        #[cfg(not(target_family = "wasm"))]
        KeyBinding::new("ctrl-v", Paste, Some("Composer")),
        KeyBinding::new("cmd-c", Copy, Some("Composer")),
        KeyBinding::new("ctrl-c", Copy, Some("Composer")),
        KeyBinding::new("enter", Submit, Some("Composer")),
    ]);
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    init_logging();
    let options = match UiOptions::parse(std::env::args()) {
        Ok(options) => options,
        Err(error) => {
            error!("could not parse Loom UI arguments: {error}");
            std::process::exit(1);
        }
    };
    info!(
        "starting with {} backend, model '{}'",
        if options.remote.is_some() {
            "remote"
        } else if options.demo {
            "demo"
        } else {
            "local"
        },
        options.model.as_str()
    );
    gpui_kit::platform::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx: &mut App| {
            info!("initializing GPUI components");
            gpui_kit::init(cx);
            crate::theme::apply_theme(WindowAppearance::Dark, cx);
            bind_composer_keys(cx);
            let view = match LoomView::try_new(
                &options,
                cx.focus_handle(),
                cx.focus_handle(),
                cx.focus_handle(),
            ) {
                Ok(view) => view,
                Err(error) => {
                    error!(
                        "could not initialize Loom UI: {}",
                        describe_startup_connection_error(
                            &error,
                            options.token.as_deref().unwrap_or_default()
                        )
                    );
                    cx.quit();
                    return;
                }
            };
            info!("Loom view initialized");

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
                    error!("failed to open Loom window: {error}");
                    cx.quit();
                    return;
                }
            };
            info!("Loom window opened");
            if let Err(error) = window.update(cx, |view, window, cx| {
                view.reconnect_configured_worker_nodes(cx);
                view.observe_system_appearance(window, cx);
                view.select_theme(crate::state::ThemeChoice::System, window, cx);
                view.composer_focus_handle.focus(window, cx);
                cx.activate(true);
            }) {
                error!("failed to focus Loom composer: {error}");
                cx.quit();
            } else {
                info!("startup complete");
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
    static APPLICATION: std::cell::RefCell<Option<gpui_kit::ApplicationHandle>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(target_family = "wasm")]
fn log_error(context: &str, error: impl std::fmt::Display) {
    log::error!("{context}: {error}");
}

/// Opens the browser workspace before any worker is connected. Initial
/// connection settings are submitted through the app's regular Settings UI.
#[cfg(target_family = "wasm")]
fn start_browser_client(cx: &mut App) {
    let (options, startup_error) = match BrowserOptions::from_location() {
        Ok(options) => (options, None),
        Err(error) => {
            log_error("could not read saved worker settings", &error);
            (BrowserOptions::empty(), Some(error.to_string()))
        }
    };
    let focus_handle = cx.focus_handle();
    let node_focus_handle = cx.focus_handle();
    let rename_focus_handle = cx.focus_handle();
    let auto_connect = options.is_configured();
    let view = LoomView::new_browser_disconnected(
        &options,
        startup_error,
        focus_handle,
        node_focus_handle,
        rename_focus_handle,
    );
    let window = match cx.open_window(Default::default(), |_, cx| cx.new(|_| view)) {
        Ok(window) => window,
        Err(error) => {
            return log_error("failed to open Loom window", error);
        }
    };
    if let Err(error) = window.update(cx, |view, window, cx| {
        view.observe_system_appearance(window, cx);
        view.composer_focus_handle.focus(window, cx);
        view.select_theme(crate::state::ThemeChoice::System, window, cx);
        cx.activate(true);
        if auto_connect {
            view.connect_worker_node(cx);
        }
    }) {
        log_error("failed to finish initializing the Loom window", error);
    }
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    init_logging();
    gpui_kit::platform::web_init();
    let application = gpui_kit::platform::application_with_web_backend(
        gpui_kit::platform::WebBackendPreference::WebGl,
    )
    .with_assets(crate::assets::LoomAssets)
    .run_embedded(|cx: &mut App| {
        // The web platform starts with an empty font database. Bundle the Noto
        // Sans faces used by the native Linux system-font fallback so browser
        // text has the same family and weight variants.
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/NotoSans-Regular.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/NotoSans-Bold.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/NotoSans-Italic.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/NotoSans-BoldItalic.ttf").as_slice(),
                ),
            ])
            .expect("failed to load embedded font");
        gpui_kit::init(cx);
        crate::theme::apply_theme(gpui_kit::WindowAppearance::Dark, cx);
        bind_composer_keys(cx);
        start_browser_client(cx);
    });
    APPLICATION.with(|slot| *slot.borrow_mut() = Some(application));
}
