//! Projects-pane navigation: overlay dismissal, touch targets, and project rows.

use super::{
    CoveringSurface, LoomView, SessionSourceChoice, SessionSourceDialog,
    SessionSourceDialogPurpose, SettingsSection, responsive_layout,
};
use crate::theme::BASE_FONT_SIZE;
use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext, Entity, TestAppContext, WindowHandle, point, px, size};
use std::cell::RefCell;
use std::rc::Rc;

/// Opens a 1280x800 window over a configured view and returns both.
#[allow(clippy::type_complexity)]
fn open_view_window(
    cx: &mut TestAppContext,
    configure: impl FnOnce(&mut LoomView),
) -> (WindowHandle<Root>, Entity<LoomView>) {
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
            configure(&mut view);
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

/// Opens the phone-sized window the touch-target tests need, with the session
/// drawer open so the projects pane is on screen.
#[allow(clippy::type_complexity)]
fn open_drawer_window(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<LoomView>) {
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(390.), px(844.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.active_session.name = "loom".to_owned();
            view.sessions = vec![view.active_session.clone()];
            let session_id = view.active_session.id;
            view.session_node_ids
                .insert(session_id, view.default_backend_node_id.clone());
            view.session_drawer_open = true;
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

fn source_dialog() -> SessionSourceDialog {
    SessionSourceDialog {
        purpose: SessionSourceDialogPurpose::StartSession,
        target_node_id: String::new(),
        choice: SessionSourceChoice::Empty,
        local_directory_available: true,
        filter_subscription: None,
        repositories: Vec::new(),
        selected_repository: None,
        repositories_loading: false,
        error: None,
    }
}

/// Navigating to the project already on screen must dismiss the new-project
/// pane: the pane covers the conversation, and re-selecting the open project
/// used to be a no-op that left the user to close the pane by hand.
#[gpui_kit::test]
fn selecting_the_open_project_dismisses_the_new_project_pane(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view_window(cx, |view| {
        view.show_surface(CoveringSurface::NewProject);
        view.source_dialog = Some(source_dialog());
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("source-dialog-pane").visible());
        window.click(("session-tree-root", 0usize), cx);
        window.render_frame(cx);
        assert!(
            view.read(cx).source_dialog.is_none(),
            "selecting the open project must close the new-project pane"
        );
        assert!(window.try_find("source-dialog-pane").is_none());
    })
    .unwrap();
}

/// The same click must return from Settings and from the archived-sessions
/// surface, which both cover the conversation the same way.
#[gpui_kit::test]
fn selecting_the_open_project_dismisses_settings_and_the_archived_surface(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view_window(cx, |view| {
        view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("settings-content").visible());
        window.click(("session-tree-root", 0usize), cx);
        window.render_frame(cx);
        assert!(
            !view.read(cx).settings_open(),
            "selecting the open project must close Settings"
        );
        assert!(window.try_find("settings-content").is_none());
    })
    .unwrap();
}

/// The archived-sessions surface is dismissed by the same navigation.
#[gpui_kit::test]
fn selecting_the_open_project_dismisses_the_archived_sessions_surface(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view_window(cx, |view| {
        view.show_surface(CoveringSurface::ArchivedSessions);
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("archived-sessions-dialog").visible());
        window.click(("session-tree-root", 0usize), cx);
        window.render_frame(cx);
        assert!(
            !view.read(cx).archived_sessions_open(),
            "selecting the open project must close the archived-sessions surface"
        );
        assert!(window.try_find("archived-sessions-dialog").is_none());
    })
    .unwrap();
}

/// Reaching a project that is not on screen still works through the same pane:
/// the click switches sessions and dismisses the overlay on top.
#[gpui_kit::test]
fn selecting_another_project_still_switches_sessions(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.active_session.name = "first".to_owned();
            let mut second = view.active_session.clone();
            second.id = loom_core::AgentSessionId::new();
            second.name = "second".to_owned();
            for session in [&view.active_session, &second] {
                view.session_node_ids
                    .insert(session.id, view.default_backend_node_id.clone());
            }
            view.sessions = vec![view.active_session.clone(), second];
            view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered.borrow().as_ref().unwrap().clone();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(("session-tree-root", 1usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_session.name, "second");
        assert!(!view.read(cx).settings_open());
    })
    .unwrap();
}

/// The pane's three icon actions are the only way to reach a new project, the
/// archived sessions, and settings, so their hit box is the control's square:
/// finger-sized on a phone, and never smaller than a comfortable target on a
/// layout that reports a desktop width. A tap near the edge of that square must
/// reach the action, which is what "the hitbox is off" describes.
#[gpui_kit::test]
fn pane_header_actions_are_tappable_squares_on_a_phone(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_drawer_window(cx);
    let actions = [
        ("new-session", "source-dialog-pane"),
        ("archived-sessions", "archived-sessions-dialog"),
        ("settings-button", "settings-content"),
    ];
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        for (action, surface) in actions {
            if window.try_find("mobile-session-drawer").is_none() {
                window.click("open-session-drawer", cx);
                window.render_frame(cx);
            }
            let bounds = window.find(action).bounds();
            assert_eq!(
                bounds.size.width,
                px(44.),
                "{action} is not a finger-sized square: {bounds:?}"
            );
            assert_eq!(
                bounds.size.height,
                px(44.),
                "{action} is not a finger-sized square: {bounds:?}"
            );
            // The tapped point is inside the control's own box, so it must reach
            // the action instead of the row behind it.
            window.click_at(action, point(px(1.), px(1.)), cx);
            window.render_frame(cx);
            assert!(
                window.find(surface).visible(),
                "a tap in the corner of {action} must open {surface}"
            );
            view.update(cx, |view, cx| {
                view.dismiss_surfaces();
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find(surface).is_none());
        }
    })
    .unwrap();
}

/// On a wider layout the three actions keep a comfortable square instead of
/// collapsing to the text-row size, which is what a phone in landscape or a
/// tablet gets.
#[gpui_kit::test]
fn pane_header_actions_keep_a_touchable_square_on_a_desktop_layout(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, _) = open_view_window(cx, |_| {});
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        for action in ["new-session", "archived-sessions", "settings-button"] {
            let bounds = window.find(action).bounds();
            assert_eq!(
                bounds.size.width, bounds.size.height,
                "{action} is not square: {bounds:?}"
            );
            assert!(
                bounds.size.width >= px(32.),
                "{action} is too small to tap on a wide layout: {bounds:?}"
            );
        }
    })
    .unwrap();
}

/// A project row is two lines: its name and the main source it was created
/// from, so the list says where each project lives without opening it.
#[gpui_kit::test]
fn project_rows_show_the_main_source_under_the_name(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view_window(cx, |view| {
        let session_id = view.active_session.id;
        view.session_sources
            .insert(session_id, "bearmuckle/loom".to_owned());
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let name = window.find(("session-tree-label", 0usize));
        let source = window.find(("session-tree-source", 0usize));
        assert_eq!(name.label(), Some("loom"));
        assert_eq!(source.label(), Some("bearmuckle/loom"));
        let row = window.find(("session-tree-root", 0usize)).bounds();
        // The two lines stack, and both stay inside the row.
        assert!(
            name.bounds().bottom() <= source.bounds().top() + px(1.),
            "name {name:?} and source {source:?} are not stacked"
        );
        assert!(source.bounds().bottom() <= row.bottom() + px(1.));
        assert_eq!(view.read(cx).session_sources.len(), 1);
    })
    .unwrap();
}

/// A project whose source is not known yet still reads as two lines: the second
/// line says when it was last active instead of disappearing and shifting the
/// list.
#[gpui_kit::test]
fn project_rows_without_a_known_source_keep_the_second_line(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, _) = open_view_window(cx, |_| {});
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let source = window.find(("session-tree-source", 0usize));
        assert!(source.visible());
        assert!(
            source
                .label()
                .is_some_and(|label| label.starts_with("updated ")),
            "unexpected second line: {:?}",
            source.label()
        );
    })
    .unwrap();
}

/// A delegated agent is not asked where it came from: it appears under its
/// project's own row, which carries the project's source. Standalone projects
/// are asked, and the number of lookups per refresh is bounded.
#[gpui_kit::test]
fn source_lookups_skip_delegated_agents_and_stay_bounded(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_view_window(cx, |_| {});
    cx.update_window(handle.into(), |_, _, cx| {
        view.update(cx, |view, _| {
            // A project whose snapshot lists the second session as its agent.
            let root_id = view.active_session.id;
            let child_id = loom_core::AgentSessionId::new();
            let mut child = view.active_session.clone();
            child.id = child_id;
            child.name = "agent".to_owned();
            view.sessions.push(child);
            let project_id = loom_core::ProjectId::from_uuid(*root_id.as_uuid());
            view.project_tree_snapshots = vec![loom_core::ProjectSnapshot {
                project_id,
                root_session_id: root_id,
                agents: vec![
                    agent_record(root_id, project_id, None, 1),
                    agent_record(child_id, project_id, Some(root_id), 2),
                ],
                tasks: Vec::new(),
                worktrees: Vec::new(),
            }];
            // Only the project root is looked up; the agent inherits its
            // project's source. The window may already have run a frame, so the
            // per-refresh bookkeeping starts clean here.
            view.session_sources.clear();
            view.session_source_lookups.clear();
            assert_eq!(view.session_source_lookup_candidates(), vec![root_id]);

            // A project that already has a source, or has already been asked,
            // is not asked again.
            view.session_sources
                .insert(root_id, "bearmuckle/loom".to_owned());
            assert!(view.session_source_lookup_candidates().is_empty());
            view.session_sources.clear();
            view.session_source_lookups.insert(root_id, (None, None));
            assert!(view.session_source_lookup_candidates().is_empty());

            // Standalone projects are all looked up, up to the refresh bound.
            view.project_tree_snapshots.clear();
            view.session_source_lookups.clear();
            for _ in 0..8 {
                let mut session = view.active_session.clone();
                session.id = loom_core::AgentSessionId::new();
                view.sessions.push(session);
            }
            assert_eq!(
                view.session_source_lookup_candidates().len(),
                LoomView::SESSION_SOURCE_LOOKUPS_PER_REFRESH
            );
        });
    })
    .unwrap();
}

/// One agent record of a project snapshot.
fn agent_record(
    session_id: loom_core::AgentSessionId,
    project_id: loom_core::ProjectId,
    parent_session_id: Option<loom_core::AgentSessionId>,
    depth: u8,
) -> loom_core::ProjectAgentRecord {
    loom_core::ProjectAgentRecord {
        session_id,
        project_id,
        parent_session_id,
        depth,
        state: loom_core::AgentSessionState::Idle,
        task_summary: None,
        output_cursor: loom_core::EventSequence::default(),
        updated_at: loom_core::Timestamp::from_unix_millis(1),
    }
}

/// On a phone the projects pane is a drawer that covers the conversation, so an
/// action that opens a full-surface pane has to dismiss the drawer. Otherwise
/// the pane opens behind the drawer: the tap looks like it did nothing until a
/// later tap closes the drawer and reveals the pane that was opened first.
#[gpui_kit::test]
fn pane_actions_reveal_the_surface_they_open_on_a_phone(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_drawer_window(cx);
    let actions = [
        ("new-session", "source-dialog-pane"),
        ("archived-sessions", "archived-sessions-dialog"),
        ("settings-button", "settings-content"),
    ];
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        for (action, surface) in actions {
            if window.try_find("mobile-session-drawer").is_none() {
                window.click("open-session-drawer", cx);
                window.render_frame(cx);
            }
            window.click(action, cx);
            window.render_frame(cx);
            assert!(
                window.try_find("mobile-session-drawer").is_none(),
                "tapping {action} must dismiss the drawer so {surface} is on screen"
            );
            assert!(window.find(surface).visible(), "{surface} must be shown");
            view.update(cx, |view, cx| {
                view.dismiss_surfaces();
                cx.notify();
            });
            window.render_frame(cx);
        }
    })
    .unwrap();
}

/// A width that leaves the phone layout must not leave two copies of the pane
/// actions on screen. The phone drawer and the fixed side panel carry the same
/// element ids, so while both are present a tap can resolve to the copy behind
/// the drawer and run the wrong action.
#[gpui_kit::test]
fn pane_actions_stay_targeted_after_the_width_leaves_the_phone_layout(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    // A phone in landscape is wider than the phone breakpoint, so the fixed
    // side panel renders; the drawer state carried over from portrait.
    let handle = cx.open_window(size(px(844.), px(390.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view.session_drawer_open = true;
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered
        .borrow_mut()
        .take()
        .expect("the window built the view");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("mobile-session-drawer").is_none(),
            "the phone drawer must not stay over the side panel"
        );
        window.click("settings-button", cx);
        window.render_frame(cx);
        assert!(view.read(cx).settings_open(), "settings must be the action");
        assert!(
            view.read(cx).source_dialog.is_none(),
            "a tap on settings must not open the new-project pane"
        );
    })
    .unwrap();
}

#[test]
fn the_pane_project_name_ignores_the_larger_default_base() {
    let desktop = responsive_layout(px(1280.));
    let phone = responsive_layout(px(390.));
    // The default 100% renders what the previous 105% rendered: a compact base
    // of 14 grown by 5%.
    assert!(
        (BASE_FONT_SIZE - 14.7).abs() < 0.001,
        "default base changed: {BASE_FONT_SIZE}"
    );
    // The project name stays at 0.8125rem of the base of 14 that predates that
    // change, so the larger default does not enlarge it.
    let project_name_px = desktop.project_row_font_size().0 * BASE_FONT_SIZE;
    assert!(
        (project_name_px - 11.375).abs() < 0.001,
        "project name size changed: {project_name_px}"
    );
    // Everything else keeps the larger default.
    assert!(desktop.nav_row_font_size().0 * BASE_FONT_SIZE > project_name_px);
    // The phone row is already pinned in pixels, so it needs no adjustment.
    assert_eq!(phone.project_row_font_size().0, phone.nav_row_font_size().0);
    assert!(phone.project_row_font_size().0 > desktop.project_row_font_size().0);
}
