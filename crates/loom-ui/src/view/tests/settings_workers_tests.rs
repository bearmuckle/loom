//! Accessibility, scrolling, and form-behavior tests for Settings (#450) and
//! adding remote workers in first-run setup and Settings -> Workers (#455).

use super::*;
use gpui_kit::test::{TestQueryExt, TestWindowExt};
use gpui_kit::{Role, ScrollDelta, TestAppContext, point, px, size};

fn sample_worker(id: u64, name: &str, url: &str) -> WorkerNodeEntry {
    WorkerNodeEntry {
        id,
        status: WorkerNodeStatus {
            node_id: format!("node-{id}"),
            name: name.to_owned(),
            online: true,
            capabilities: CapabilitySet::default(),
            resources: WorkerNodeResources {
                cpu_count: 4,
                cpu_usage_percent: Some(12),
                memory_usage_percent: Some(34),
                memory_total_bytes: Some(16 << 30),
                memory_available_bytes: Some(10 << 30),
                disk_total_bytes: Some(256 << 30),
                disk_available_bytes: Some(120 << 30),
            },
        },
        is_local: id == 0,
        remote_startup: false,
        url: (id != 0).then(|| url.to_owned()),
        connection: None,
        connection_state: WorkerConnectionState::Connected,
        connection_detail: None,
        severe_load_streak: 0,
    }
}

#[gpui_kit::test]
fn settings_overlay_exposes_dialog_semantics_and_scrolls_to_clipped_rows_at_1280x800(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);

        // Dialog root, heading, close button, and section navigation all appear
        // with proper AccessKit roles and labels.
        let dialog = window.find("settings-dialog");
        assert_eq!(dialog.role(), Some(Role::Dialog));
        assert_eq!(dialog.label(), Some("Settings"));

        let title = window
            .within("settings-dialog")
            .find("settings-dialog-title");
        assert_eq!(title.role(), Some(Role::Heading));
        assert_eq!(title.label(), Some("Settings"));

        let close = window.within("settings-dialog").find("close-settings");
        assert_eq!(close.role(), Some(Role::Button));
        assert_eq!(close.label(), Some("Close settings"));

        let nav = window.within("settings-dialog").find("settings-nav");
        assert_eq!(nav.role(), Some(Role::TabList));
        let tabs = window.within("settings-dialog").find_all_by_role(Role::Tab);
        assert_eq!(tabs.len(), SETTINGS_SECTIONS.len());
        assert_eq!(tabs[0].label(), Some("Agents"));

        // At 1280x800 the Agents pane overflows vertically; the scroll handle
        // reports scrollable overflow and scrolling reaches the bottom row.
        let view = window.root::<LoomView>().unwrap().unwrap();
        let max_offset = view.read(cx).settings_scroll_handle.max_offset().y;
        assert!(
            max_offset > px(0.),
            "Settings -> Agents should overflow and be scrollable at 1280x800"
        );
        let content_bounds = window
            .within("settings-dialog")
            .find("settings-content")
            .bounds();
        window.scroll(
            "settings-content",
            ScrollDelta::Pixels(point(px(0.), -(max_offset + px(40.)))),
            cx,
        );
        let bottom_choice = window
            .within("settings-dialog")
            .find(("output-detail", 0usize));
        assert!(bottom_choice.visible());
        assert!(
            bottom_choice.bounds().bottom() <= content_bounds.bottom(),
            "scrolling Settings must bring the bottom row fully into view: {:?} vs {:?}",
            bottom_choice.bounds(),
            content_bounds
        );

        // Switching sections resets the scroll offset to the top.
        window
            .within("settings-dialog")
            .click(("settings-section", 2usize), cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).settings_scroll_handle.offset().y,
            px(0.),
            "switching Settings sections must reset the scroll offset"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn settings_workers_uses_separate_masked_token_and_exposes_inline_validation_errors(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.show_surface(CoveringSurface::Settings(SettingsSection::Workers));
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);

        // Separate URL and masked token fields, both named in the a11y tree.
        let url_field = window.within("settings-dialog").find_by_label("Worker URL");
        assert!(url_field.visible());
        let token_field = window
            .within("settings-dialog")
            .find_by_label("Access token (optional)");
        assert!(token_field.visible());
        assert_eq!(token_field.role(), Some(Role::PasswordInput));

        let view = window.root::<LoomView>().unwrap().unwrap();
        let url_state = view
            .read(cx)
            .node_input_state
            .clone()
            .expect("URL input initialized");
        let token_state = view
            .read(cx)
            .node_token_input_state
            .clone()
            .expect("token input initialized");
        assert!(
            token_state.read(cx).presentation().is_masked(),
            "Settings -> Workers token field must mask input"
        );

        // Empty Connect shows an inline error exposed as Role::Alert.
        window
            .within("settings-dialog")
            .click("connect-worker-node", cx);
        window.render_frame(cx);
        let error = window
            .within("settings-dialog")
            .find("worker-connect-error");
        assert!(error.visible());
        assert_eq!(error.role(), Some(Role::Alert));
        assert!(
            error
                .label()
                .is_some_and(|label| label.contains("Enter the worker URL"))
        );

        // Credentials embedded in URL are rejected inline without adding a row.
        url_state.update(cx, |state, cx| {
            state.set_value("ws://user:dummy@127.0.0.1:9999/ws", window, cx);
        });
        window
            .within("settings-dialog")
            .click("connect-worker-node", cx);
        window.render_frame(cx);
        assert!(view.read(cx).worker_nodes.is_empty());
        let error = window
            .within("settings-dialog")
            .find("worker-connect-error");
        assert_eq!(error.role(), Some(Role::Alert));
        assert!(
            error
                .label()
                .is_some_and(|label| label.contains("Do not include credentials"))
        );

        // Invalid non-URL text is also rejected inline as Role::Alert.
        url_state.update(cx, |state, cx| {
            state.set_value("not-a-url", window, cx);
        });
        window
            .within("settings-dialog")
            .click("connect-worker-node", cx);
        window.render_frame(cx);
        assert!(view.read(cx).worker_nodes.is_empty());
        let error = window
            .within("settings-dialog")
            .find("worker-connect-error");
        assert_eq!(error.role(), Some(Role::Alert));
        assert!(
            error
                .label()
                .is_some_and(|label| label.contains("Invalid worker URL"))
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn failed_worker_connect_stays_on_screen_with_remove_control_at_1280x800(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        // Seed enough existing workers to overflow the 1280x800 Settings body.
        for id in 0..7u64 {
            view.worker_nodes.push(sample_worker(
                id,
                &format!("worker-{id}"),
                &format!("ws://127.0.0.1:{}/ws", 8765 + id),
            ));
        }
        view.next_worker_node_id = 7;
        view.show_surface(CoveringSurface::Settings(SettingsSection::Workers));
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let view = window.root::<LoomView>().unwrap().unwrap();
        let url_state = view
            .read(cx)
            .node_input_state
            .clone()
            .expect("URL input initialized");
        let token_state = view
            .read(cx)
            .node_token_input_state
            .clone()
            .expect("token input initialized");
        url_state.update(cx, |state, cx| {
            state.set_value("ws://127.0.0.1:9999/ws", window, cx);
        });
        token_state.update(cx, |state, cx| {
            state.set_value("dummy-token-not-real", window, cx);
        });
        view.update(cx, |view, cx| {
            let id = view
                .begin_worker_node_connection("ws://127.0.0.1:9999/ws")
                .expect("new worker row created");
            let error = LoomError::new(ErrorCode::Internal, "connection refused", true);
            view.fail_worker_node_connection(
                id,
                "ws://127.0.0.1:9999/ws",
                WorkerConnectionStage::Transport,
                &error,
                Some("dummy-token-not-real"),
                false,
            );
            view.settings_scroll_handle.scroll_to_bottom();
            cx.notify();
        });
        window.render_frame(cx);
        window.render_frame(cx);

        let content_bounds = window
            .within("settings-dialog")
            .find("settings-content")
            .bounds();
        let row = window.within("settings-dialog").find("worker-node-row-7");
        let detail = window
            .within("settings-dialog")
            .find("worker-node-detail-7");
        let remove = window
            .within("settings-dialog")
            .find("remove-worker-node-7");

        assert!(row.visible());
        assert!(detail.visible());
        assert!(remove.visible());
        assert_eq!(detail.role(), Some(Role::Alert));
        assert!(
            detail
                .label()
                .is_some_and(|label| label.contains("refused the connection"))
        );
        assert!(
            row.bounds().top() >= content_bounds.top()
                && row.bounds().bottom() <= content_bounds.bottom(),
            "the failed worker row must stay fully inside the 1280x800 Settings viewport: {:?} vs {:?}",
            row.bounds(),
            content_bounds
        );
        assert!(
            remove.bounds().bottom() <= content_bounds.bottom(),
            "the failed row's Remove control must stay reachable at 1280x800: {:?} vs {:?}",
            remove.bounds(),
            content_bounds
        );

        // Removing the failed row works directly without resizing the window.
        window
            .within("settings-dialog")
            .click("remove-worker-node-7", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx)
                .worker_nodes
                .iter()
                .all(|node| node.id != 7)
        );
    })
    .unwrap();
}

#[cfg(not(target_family = "wasm"))]
struct FirstRunSetupHost {
    view: Entity<LoomView>,
}

#[cfg(not(target_family = "wasm"))]
impl Render for FirstRunSetupHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.view
            .update(cx, |view, cx| view.render_worker_setup(window, cx))
    }
}

#[cfg(not(target_family = "wasm"))]
#[gpui_kit::test]
fn first_run_setup_prioritizes_connect_and_exposes_copy_and_errors_to_accessibility(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let mut created = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let options = UiOptions {
            project: None,
            task: "setup".to_owned(),
            model: ModelId::new("deterministic/demo"),
            remote: None,
            token: None,
            reset_state: false,
            server_binary: None,
            ca: None,
            allow_insecure_remote: false,
        };
        let view = cx.new(|cx| LoomView::new_setup(&options, None, None, cx.focus_handle()));
        created = Some(view.clone());
        FirstRunSetupHost { view }
    });
    let view = created.expect("setup view created");

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);

        // Heading, description, and fields are exposed in the a11y tree.
        let card = window.find("worker-setup-card");
        assert_eq!(card.role(), Some(Role::Region));
        assert_eq!(window.find("worker-setup-heading").role(), Some(Role::Heading));
        assert_eq!(
            window.find("worker-setup-description").role(),
            Some(Role::Label)
        );
        let url_field = window.find_by_label("Worker URL");
        let token_field = window.find_by_label("Access token (optional)");
        assert!(url_field.visible());
        assert_eq!(token_field.role(), Some(Role::PasswordInput));

        // Connect comes above the secondary local-service install control.
        let connect_bounds = window.find("worker-setup-connect").bounds();
        let install_bounds = window.find("worker-setup-install").bounds();
        assert!(
            connect_bounds.bottom() < install_bounds.top(),
            "Connect must appear above the secondary Install action: connect={connect_bounds:?}, install={install_bounds:?}"
        );

        // Empty Connect exposes the validation error as Role::Alert.
        window.click("worker-setup-connect", cx);
        window.render_frame(cx);
        let error = window.find("worker-setup-error");
        assert!(error.visible());
        assert_eq!(error.role(), Some(Role::Alert));
        assert!(
            error
                .label()
                .is_some_and(|label| label.contains("Enter the worker URL"))
        );

        // Credentials in URL are rejected and exposed as Role::Alert.
        let url_input = view
            .read(cx)
            .setup
            .as_ref()
            .and_then(|setup| setup.url_input.clone())
            .expect("setup URL input");
        url_input.update(cx, |state, cx| {
            state.set_value("ws://user:dummy@127.0.0.1:9999/ws", window, cx);
        });
        window.click("worker-setup-connect", cx);
        window.render_frame(cx);
        let error = window.find("worker-setup-error");
        assert_eq!(error.role(), Some(Role::Alert));
        assert!(
            error
                .label()
                .is_some_and(|label| label.contains("Do not include credentials"))
        );

        // Invalid non-URL text is rejected and exposed as Role::Alert.
        url_input.update(cx, |state, cx| {
            state.set_value("not-a-url", window, cx);
        });
        window.click("worker-setup-connect", cx);
        window.render_frame(cx);
        let error = window.find("worker-setup-error");
        assert_eq!(error.role(), Some(Role::Alert));
        assert!(
            error
                .label()
                .is_some_and(|label| label.contains("Invalid worker URL"))
        );
    })
    .unwrap();
}

struct DisconnectedStartupHost {
    view: Entity<LoomView>,
}

impl Render for DisconnectedStartupHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.view
            .update(cx, |view, cx| view.render_startup_screen(window, cx))
    }
}

#[gpui_kit::test]
fn disconnected_startup_open_settings_opens_workers_section(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let mut created = None;
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
        created = Some(view.clone());
        DisconnectedStartupHost { view }
    });
    let view = created.expect("view created");

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("workspace-status-primary", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).settings_section(),
            Some(SettingsSection::Workers)
        );
        assert!(
            window
                .within("settings-dialog")
                .find("settings-content")
                .visible()
        );
    })
    .unwrap();
}
