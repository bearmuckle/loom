//! The small dialogs a header action opens must fit the viewport they are drawn
//! in.
//!
//! The rename and sub project dialogs share one surface: a floating card at a
//! fixed offset on a wider layout, and a card inset inside a full-window overlay
//! on a phone, where that offset added to the card's fixed width would put its
//! right side past the edge of the screen.

use super::loom_view_render_tests::render_scenario_at;
use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{Pixels, Size, TestAppContext, px, size};

/// One of the small dialogs, named by the ids it renders and how it is opened.
struct Dialog {
    /// The dialog's own root element.
    surface: &'static str,
    /// The card the dialog is drawn on.
    card: &'static str,
    /// The one field the dialog carries.
    field: &'static str,
    /// Cancel and confirm, in render order.
    actions: [&'static str; 2],
    /// Opens the dialog in a fresh view.
    configure: fn(&mut LoomView),
}

const DIALOGS: [Dialog; 2] = [
    Dialog {
        surface: "rename-dialog",
        card: "rename-card",
        field: "rename-session-input",
        actions: ["cancel-rename", "confirm-rename"],
        configure: |view| {
            view.show_surface(CoveringSurface::Rename);
            view.rename_dialog = Some(RenameDialogState {
                session: view.active_session.clone(),
                input: "Renamed session".to_owned(),
                is_project: false,
            });
        },
    },
    Dialog {
        surface: "sub-project-dialog",
        card: "sub-project-card",
        field: "sub-project-name-input",
        actions: ["cancel-sub-project", "confirm-sub-project"],
        configure: |view| {
            view.show_surface(CoveringSurface::SubProject);
            view.sub_project_dialog = Some(SubProjectDialogState {
                parent_session_id: view.active_session.id,
                parent_name: "loom".to_owned(),
            });
        },
    },
];

/// Renders one dialog in the shared scenario harness and returns the window it
/// was rendered into.
fn render_dialog(
    cx: &mut TestAppContext,
    dialog: &Dialog,
    window_size: Size<Pixels>,
) -> gpui_kit::AnyWindowHandle {
    cx.update(gpui_kit::init);
    let configure = dialog.configure;
    render_scenario_at(cx, window_size, configure);
    cx.windows()
        .last()
        .copied()
        .expect("the scenario opened a window")
}

/// Asserts that the card, its field, and its actions all stay inside a phone's
/// viewport, and that the actions are finger-sized targets.
fn assert_dialog_fits_phone(cx: &mut TestAppContext, dialog: &Dialog, window_size: Size<Pixels>) {
    let handle = render_dialog(cx, dialog, window_size);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        let card = window.find(dialog.card);
        assert!(card.visible(), "{} is not on screen", dialog.card);
        let bounds = card.bounds();
        assert!(
            bounds.size.width > px(0.),
            "{} has no width: {bounds:?}",
            dialog.card
        );
        assert!(
            bounds.left() >= px(0.),
            "{} runs off the left edge: {bounds:?}",
            dialog.card
        );
        assert!(
            bounds.right() <= viewport.width,
            "{} runs off the right edge: {bounds:?} in {viewport:?}",
            dialog.card
        );
        assert!(
            bounds.bottom() <= viewport.height,
            "{} runs off the bottom: {bounds:?} in {viewport:?}",
            dialog.card
        );
        assert!(
            window.find(dialog.field).visible(),
            "{} has no field",
            dialog.surface
        );
        for id in dialog.actions {
            let button = window.find(id);
            assert!(button.visible(), "{id} is not on screen");
            let bounds = button.bounds();
            assert!(
                bounds.size.height >= px(44.),
                "{id} is not a finger-sized target: {bounds:?}"
            );
            assert!(
                bounds.right() <= viewport.width,
                "{id} runs off the right edge: {bounds:?} in {viewport:?}"
            );
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_phone_insets_each_dialog_inside_the_viewport(cx: &mut TestAppContext) {
    for window_size in [size(px(390.), px(520.)), size(px(320.), px(480.))] {
        for dialog in &DIALOGS {
            assert_dialog_fits_phone(cx, dialog, window_size);
        }
    }
}

/// A layout wider than a phone keeps the floating card at the width and offset
/// it has always had, instead of insetting it in a full-window overlay the way a
/// phone has to.
#[gpui_kit::test]
fn a_wider_layout_floats_each_card_at_its_offset(cx: &mut TestAppContext) {
    for dialog in &DIALOGS {
        let handle = render_dialog(cx, dialog, size(px(1280.), px(800.)));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let viewport = window.viewport_size();
            let surface = window.find(dialog.surface).bounds();
            let card = window.find(dialog.card).bounds();
            assert_eq!(card.size.width, px(420.), "{}: {card:?}", dialog.card);
            assert_eq!(
                card.left(),
                surface.left(),
                "the wider layout adds no overlay around {}: {surface:?}",
                dialog.card
            );
            assert_eq!(
                card.top(),
                surface.top(),
                "the wider layout adds no overlay around {}: {surface:?}",
                dialog.card
            );
            assert!(card.right() <= viewport.width, "{}: {card:?}", dialog.card);
        })
        .unwrap();
    }
}
