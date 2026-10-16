//! The covering-surface invariant: at most one surface covers the conversation.
//!
//! Opening any surface must replace whatever covered the conversation before it,
//! which is a property of the view's single covering surface rather than a
//! convention each opener follows. These tests fail when a new surface is added
//! that neither lists itself in [`CoveringSurface::ALL`] nor is drawn by the one
//! place the renderer adds a surface.

use super::*;
use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext, Entity, TestAppContext, WindowHandle, px, size};
use std::cell::RefCell;
use std::rc::Rc;

/// A window over a configured view.
#[allow(clippy::type_complexity)]
fn open_view_window(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<LoomView>) {
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.active_session.name = "loom".to_owned();
            view.sessions = vec![view.active_session.clone()];
            let session_id = view.active_session.id;
            view.session_node_ids
                .insert(session_id, view.default_backend_node_id.clone());
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = rendered
        .borrow_mut()
        .take()
        .expect("the window built the view");
    (handle, view)
}

/// Brings `surface` on screen the way the app opens it.
fn open_surface(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    view: &Entity<LoomView>,
    surface: CoveringSurface,
) {
    let view = view.clone();
    cx.update_window(handle.into(), move |_, window, cx| {
        view.update(cx, |view, cx| match surface {
            CoveringSurface::Settings(section) => view.open_settings_section(section, cx),
            CoveringSurface::ArchivedSessions => view.open_archived_sessions(cx),
            CoveringSurface::NewProject => {
                view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
            }
            CoveringSurface::GitHubLogin => {
                view.show_github_login(GitHubLoginState::Starting);
            }
            CoveringSurface::Rename => {
                let session = view.active_session.clone();
                view.begin_session_rename(session, false, window, cx);
            }
        });
    })
    .unwrap();
}

/// Renders a frame, then reports which covering surfaces it painted and which
/// one the view names. The two must agree: the value is what decides.
fn rendered_surfaces(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    view: &Entity<LoomView>,
) -> (Vec<&'static str>, Option<CoveringSurface>) {
    let view = view.clone();
    let shown = cx
        .update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let mut shown = Vec::new();
            for surface in CoveringSurface::ALL {
                if window
                    .try_find(surface.id())
                    .is_some_and(|element| element.visible())
                {
                    shown.push(surface.id());
                }
            }
            shown
        })
        .unwrap();
    let named = cx.update(|cx| view.read(cx).covering_surface());
    (shown, named)
}

/// Asserts that exactly `expected` is on screen, and that the one surface the
/// renderer painted is the one the view names.
fn assert_on_screen(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    view: &Entity<LoomView>,
    expected: Option<CoveringSurface>,
) {
    let (shown, named) = rendered_surfaces(cx, handle, view);
    assert_eq!(
        shown,
        named
            .map(CoveringSurface::id)
            .into_iter()
            .collect::<Vec<_>>(),
        "the surface on screen must be the one the view names"
    );
    assert_eq!(
        named, expected,
        "expected {expected:?} on screen, the view names {named:?}"
    );
    assert!(
        shown.len() <= 1,
        "at most one covering surface may be on screen, found {shown:?}"
    );
}

/// Every ordered pair of covering surfaces leaves exactly the second one on
/// screen, so no opener can leave the surface it replaced painted over it.
#[gpui_kit::test]
fn every_ordered_pair_leaves_exactly_one_surface_on_screen(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    for first in CoveringSurface::ALL {
        let (handle, view) = open_view_window(cx);
        open_surface(cx, handle, &view, first);
        assert_on_screen(cx, handle, &view, Some(first));
        for second in CoveringSurface::ALL {
            open_surface(cx, handle, &view, second);
            assert_on_screen(cx, handle, &view, Some(second));
        }
    }
}

/// About and Providers are the settings dialog with their own section: every
/// section control opens that one surface, and the close control closes it.
#[gpui_kit::test]
fn the_settings_sections_open_and_close_as_one_surface(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view_window(cx);
    view.update(cx, |view, cx| view.open_settings_from_menu(cx));
    assert_on_screen(
        cx,
        handle,
        &view,
        Some(CoveringSurface::Settings(SettingsSection::Agents)),
    );
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        for (index, (section, _)) in SETTINGS_SECTIONS.into_iter().enumerate() {
            window
                .within("settings-dialog")
                .click(("settings-section", index), cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).settings_section(),
                Some(section),
                "the section control for {section:?} must show it"
            );
            assert!(
                window
                    .within("settings-dialog")
                    .find("settings-content")
                    .visible()
            );
        }
    })
    .unwrap();
    // About and Providers are the same surface with their own section, and the
    // close control closes it whichever section it shows.
    view.update(cx, |view, cx| view.open_about_from_menu(cx));
    assert_on_screen(
        cx,
        handle,
        &view,
        Some(CoveringSurface::Settings(SettingsSection::About)),
    );
    view.update(cx, |view, cx| view.open_providers_from_menu(cx));
    assert_on_screen(
        cx,
        handle,
        &view,
        Some(CoveringSurface::Settings(SettingsSection::Providers)),
    );
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.within("settings-dialog").click("close-settings", cx);
    })
    .unwrap();
    assert_on_screen(cx, handle, &view, None);
    assert!(!cx.update(|cx| view.read(cx).settings_open()));
}

/// Escape closes the surface the user can see, the archived-session surface
/// included.
#[gpui_kit::test]
fn escape_closes_the_surface_on_screen(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    for surface in CoveringSurface::ALL {
        let (handle, view) = open_view_window(cx);
        open_surface(cx, handle, &view, surface);
        assert_on_screen(cx, handle, &view, Some(surface));
        let consumed = view.update(cx, |view, cx| view.handle_escape(cx));
        assert!(consumed, "closing {surface:?} must consume the keystroke");
        assert_on_screen(cx, handle, &view, None);
    }
}

/// Every surface that carries its own close control keeps closing from it: the
/// archived listing, the GitHub login pane, the rename dialog, and the
/// new-project pane.
#[gpui_kit::test]
fn each_surface_closes_from_its_own_control(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    for (surface, control, pane) in [
        (
            CoveringSurface::ArchivedSessions,
            "close-archived-sessions",
            "archived-sessions-dialog",
        ),
        (
            CoveringSurface::GitHubLogin,
            "close-github-login",
            "github-login-dialog",
        ),
        (CoveringSurface::Rename, "cancel-rename", "rename-dialog"),
        (
            CoveringSurface::NewProject,
            "close-source-dialog",
            "source-dialog-pane",
        ),
    ] {
        let (handle, view) = open_view_window(cx);
        open_surface(cx, handle, &view, surface);
        assert_on_screen(cx, handle, &view, Some(surface));
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.within(pane).click(control, cx);
        })
        .unwrap();
        assert_on_screen(cx, handle, &view, None);
    }
}

/// The archived-sessions surface is also dismissed by selecting a project,
/// which is the navigation path a user takes back to the conversation.
#[gpui_kit::test]
fn selecting_a_project_dismisses_the_archived_surface(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view_window(cx);
    open_surface(cx, handle, &view, CoveringSurface::ArchivedSessions);
    assert_on_screen(cx, handle, &view, Some(CoveringSurface::ArchivedSessions));
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(("session-tree-root", 0usize), cx);
    })
    .unwrap();
    assert_on_screen(cx, handle, &view, None);
}

/// A close control that fires after its own surface was replaced only drops the
/// state it held: it must not close the surface the user is looking at.
#[gpui_kit::test]
fn a_stale_close_control_does_not_close_the_surface_on_screen(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view_window(cx);
    open_surface(cx, handle, &view, CoveringSurface::ArchivedSessions);
    view.update(cx, |view, cx| {
        view.close_rename_dialog(cx);
        view.close_github_login(cx);
        view.close_source_dialog(cx);
    });
    assert_on_screen(cx, handle, &view, Some(CoveringSurface::ArchivedSessions));
}

/// A submitted rename steps its dialog aside while the request is in flight, so
/// the conversation is not covered by a dialog that has nothing left to do.
#[gpui_kit::test]
fn a_submitted_rename_steps_its_dialog_aside(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view_window(cx);
    open_surface(cx, handle, &view, CoveringSurface::Rename);
    assert_on_screen(cx, handle, &view, Some(CoveringSurface::Rename));
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let input = view
            .read(cx)
            .rename_input_state
            .clone()
            .expect("the rename dialog renders its input");
        input.update(cx, |state, cx| state.set_value("renamed", window, cx));
    })
    .unwrap();
    view.update(cx, |view, cx| view.confirm_rename(cx));
    assert_on_screen(cx, handle, &view, None);
}

/// The phone's session drawer and the review column are secondary chrome: a
/// covering surface replaces both, instead of every opener clearing whichever
/// of them it remembered.
#[gpui_kit::test]
fn opening_a_surface_puts_the_secondary_chrome_away(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    for surface in CoveringSurface::ALL {
        let (handle, view) = open_view_window(cx);
        view.update(cx, |view, cx| {
            view.session_drawer_open = true;
            view.review.open = true;
            cx.notify();
        });
        open_surface(cx, handle, &view, surface);
        assert_on_screen(cx, handle, &view, Some(surface));
        assert!(!cx.update(|cx| view.read(cx).session_drawer_open));
        assert!(!cx.update(|cx| view.read(cx).review.open));
    }
}
