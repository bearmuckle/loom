//! The new-project pane: source choice control, heading, and action placement.

use super::loom_view_render_tests::render_scenario_at;
use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

/// Renders the new-project pane in the shared scenario harness with every
/// source choice on offer, and returns the window it was rendered into.
fn render_new_project_pane(
    cx: &mut TestAppContext,
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
    choice: SessionSourceChoice,
) -> gpui_kit::AnyWindowHandle {
    cx.update(gpui_kit::init);
    render_scenario_at(cx, window_size, move |view| {
        view.show_surface(CoveringSurface::NewProject);
        view.source_dialog = Some(SessionSourceDialog {
            purpose: SessionSourceDialogPurpose::StartSession,
            target_node_id: String::new(),
            choice,
            local_directory_available: true,
            filter_subscription: None,
            repositories: Vec::new(),
            selected_repository: None,
            repositories_loading: false,
            error: None,
        });
    });
    let windows = cx.windows();
    assert_eq!(windows.len(), 1, "the scenario renders in a single window");
    windows[0]
}

const SOURCE_CHOICES: [&str; 3] = ["source-empty", "source-local-directory", "source-github"];

#[gpui_kit::test]
fn source_choices_render_as_distinct_rows_with_a_selected_state(cx: &mut TestAppContext) {
    let handle = render_new_project_pane(cx, size(px(1280.), px(800.)), SessionSourceChoice::Empty);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let empty = window.find("source-empty");
        let local = window.find("source-local-directory");
        let github = window.find("source-github");
        assert!(empty.visible() && local.visible() && github.visible());

        // Each choice is its own control, marked with its own selected state
        // instead of the three reading as one joined group.
        assert_eq!(empty.selected(), Some(true));
        assert_eq!(local.selected(), Some(false));
        assert_eq!(github.selected(), Some(false));
        assert!(empty.bounds().bottom() <= local.bounds().top());
        assert!(local.bounds().bottom() <= github.bounds().top());
        assert!(empty.bounds().size.height >= px(44.));
        assert_eq!(
            SOURCE_CHOICES
                .into_iter()
                .filter(|id| window.find(*id).selected() == Some(true))
                .count(),
            1,
            "exactly the current choice is marked"
        );

        // The header is a real heading with a description under it.
        let title = window.find("source-dialog-title").bounds();
        let subtitle = window.find("source-dialog-subtitle").bounds();
        assert!(
            title.size.height > subtitle.size.height,
            "title {title:?} is not larger than subtitle {subtitle:?}"
        );
        assert!(title.bottom() <= subtitle.top());
        assert!(window.find("close-source-dialog").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn phone_source_choices_stack_as_full_width_touch_targets(cx: &mut TestAppContext) {
    let handle = render_new_project_pane(cx, size(px(390.), px(520.)), SessionSourceChoice::Empty);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        let empty = window.find("source-empty");
        let local = window.find("source-local-directory");
        let github = window.find("source-github");
        assert!(empty.visible() && local.visible() && github.visible());

        // The choices stack vertically instead of running off the pane.
        assert!(empty.bounds().bottom() <= local.bounds().top());
        assert!(local.bounds().bottom() <= github.bounds().top());
        for (id, selected) in [
            ("source-empty", true),
            ("source-local-directory", false),
            ("source-github", false),
        ] {
            let row = window.find(id);
            assert_eq!(row.selected(), Some(selected), "{id} selection");
            assert!(
                row.bounds().size.height >= px(44.),
                "{id} is not a comfortable touch target: {:?}",
                row.bounds()
            );
            assert!(
                row.bounds().right() <= viewport.width,
                "{id} overflows the pane: {:?}",
                row.bounds()
            );
        }

        let title = window.find("source-dialog-title").bounds();
        let subtitle = window.find("source-dialog-subtitle").bounds();
        assert!(
            title.size.height > subtitle.size.height,
            "title {title:?} is not larger than subtitle {subtitle:?}"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn desktop_actions_sit_adjacent_to_the_form(cx: &mut TestAppContext) {
    let handle = render_new_project_pane(cx, size(px(1280.), px(800.)), SessionSourceChoice::Empty);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let pane = window.find("source-dialog-pane").bounds();
        let form = window.find("source-dialog-form").bounds();
        let actions = window.find("source-dialog-actions").bounds();
        let confirm = window.find("confirm-session-source").bounds();

        // The actions share the form's centered container...
        assert_eq!(
            actions.left(),
            form.left(),
            "actions {actions:?} vs form {form:?}"
        );
        assert_eq!(
            actions.right(),
            form.right(),
            "actions {actions:?} vs form {form:?}"
        );
        assert!(confirm.right() <= form.right() + px(1.));
        assert!(confirm.right() >= form.right() - px(1.));

        // ...and end with the form content instead of being pinned to the pane
        // bottom, far away from what they act on.
        assert!(actions.top() >= form.top());
        assert!(
            form.bottom() - actions.bottom() <= px(1.),
            "actions {actions:?} vs form {form:?}"
        );
        assert!(
            pane.bottom() - actions.bottom() > px(150.),
            "actions pinned to the pane bottom: {actions:?} pane {pane:?}"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn choosing_a_source_switches_the_visible_body(cx: &mut TestAppContext) {
    let handle = render_new_project_pane(cx, size(px(1280.), px(800.)), SessionSourceChoice::Empty);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("source-empty").selected(), Some(true));
        assert!(window.try_find("github-repository-filter").is_none());
        assert!(window.try_find("local-session-directory-path").is_none());

        window.click("source-github", cx);
        window.render_frame(cx);
        assert!(window.find("github-repository-filter").visible());
        assert!(window.try_find("local-session-directory-path").is_none());
        assert_eq!(window.find("source-github").selected(), Some(true));
        assert_eq!(window.find("source-empty").selected(), Some(false));

        window.click("source-local-directory", cx);
        window.render_frame(cx);
        assert!(window.find("local-session-directory-path").visible());
        assert!(window.try_find("github-repository-filter").is_none());
        assert_eq!(window.find("source-local-directory").selected(), Some(true));
        assert_eq!(window.find("source-github").selected(), Some(false));
    })
    .unwrap();
}
