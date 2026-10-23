use super::*;

impl LoomView {
    pub(crate) fn render_model_picker(
        &self,
        phone: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut picker = div().flex().items_center().min_w(px(0.));
        if phone {
            // Let the model picker take the space left by the other composer
            // actions so the full model name stays readable on a phone, and keep
            // a finger-sized tap target.
            picker = picker.flex_1().h(px(44.));
        }
        if let Some(state) = &self.model_select {
            picker = picker.child(
                div()
                    .flex()
                    .items_center()
                    .min_w(px(0.))
                    .when(phone, |element| element.flex_1())
                    .rounded_md()
                    .hover(|style| style.bg(rgb(0x293244)))
                    // Opening the picker is the on-demand trigger for provider
                    // model discovery; the refresh itself is throttled and runs
                    // off the UI thread.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _, _, cx| {
                            view.refresh_models_on_demand(cx);
                        }),
                    )
                    .child(
                        Select::new(state)
                            .id("session-model-select")
                            .max_w(if phone { px(180.) } else { px(220.) })
                            .when(phone, |select| select.w_full())
                            .menu_width(px(320.))
                            .small()
                            .appearance(false)
                            .accessibility_label("Model for this session")
                            .placeholder("No model is configured")
                            .search_placeholder("Search models"),
                    ),
            );
        }
        picker
    }

    /// The screen the browser client shows while it has no workspace to show:
    /// one centred card, either the startup progress while it connects and
    /// opens its landing project, or the reconnect state once it has no worker
    /// to reach.
    ///
    /// The card carries the whole screen. Painting a sidebar or an empty pane
    /// around it would make the client look half loaded, so the surface behind
    /// it stays empty until the workspace is ready to be revealed in one paint.
    #[cfg(any(target_family = "wasm", test))]
    pub(crate) fn render_startup_screen(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let layout = responsive_layout(window.bounds().size.width);
        #[cfg(target_family = "wasm")]
        let transport_connected = self.connected;
        #[cfg(not(target_family = "wasm"))]
        let transport_connected = self.is_connected();
        let screen = if self.browser_starting {
            startup_screen(transport_connected)
        } else {
            disconnected_screen(self.browser_connection_lost.as_deref())
        }
        .with_progress(self.reconnect_progress());
        // The spinner reports work in flight: a bootstrap, or an automatic
        // reconnect whose schedule still has attempts left. A spent schedule
        // stopped on its own, so it offers the retry control instead.
        let busy = self.browser_starting
            || self
                .reconnecting
                .as_ref()
                .is_some_and(|state| !state.schedule().exhausted());
        let retry_label = self
            .reconnecting
            .as_ref()
            .map(|state| state.retry_label())
            .unwrap_or("Reconnect");
        // A bootstrap in flight is progress, not a failure: it offers neither a
        // settings prompt nor a reconnect, which would only look like something
        // to fix while the client is already working on it.
        let offer_actions = !self.browser_starting;
        let details = screen.details.clone();
        div()
            .size_full()
            .relative()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_4()
                    .child(
                        div()
                            .id("workspace-status-card")
                            .test_support()
                            .w_full()
                            .max_w(px(420.))
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_3()
                            .p_6()
                            .rounded_lg()
                            .bg(rgb(0x171c25))
                            .border_1()
                            .border_color(rgb(0x293244))
                            .when(busy, |card| {
                                card.child(
                                    div()
                                        .id("workspace-status-spinner")
                                        .test_support()
                                        .child(Spinner::new().large()),
                                )
                            })
                            .child(
                                div()
                                    .id("workspace-status-heading")
                                    .test_support()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_center()
                                    .child(screen.heading),
                            )
                            .child(
                                div()
                                    .id("workspace-status-detail")
                                    .test_support()
                                    .text_sm()
                                    .text_color(rgb(0x8f98a6))
                                    .text_center()
                                    .child(screen.detail.clone()),
                            )
                            // One progress line, so the attempt count never repeats the
                            // copy above it.
                            .when_some(screen.progress.clone(), |card, progress| {
                                card.child(
                                    div()
                                        .id("workspace-status-progress")
                                        .test_support()
                                        .text_xs()
                                        .text_color(rgb(0x64748b))
                                        .text_center()
                                        .child(progress),
                                )
                            })
                            // The raw close reason is a transport string, so it stays
                            // behind this disclosure instead of in the copy.
                            .when_some(details, |card, raw| {
                                card.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            Button::new("workspace-status-details")
                                                .label(if self.status_details_open {
                                                    "Hide details"
                                                } else {
                                                    "Details"
                                                })
                                                .ghost()
                                                .xsmall()
                                                .on_click(cx.listener(|view, _, _, cx| {
                                                    view.toggle_status_details(cx);
                                                })),
                                        )
                                        .when(self.status_details_open, |column| {
                                            column.child(
                                                div()
                                                    .id("workspace-status-raw-details")
                                                    .test_support()
                                                    .text_xs()
                                                    .text_color(rgb(0x64748b))
                                                    .text_center()
                                                    .child(raw),
                                            )
                                        }),
                                )
                            })
                            .when(offer_actions, |card| {
                                card.child(
                                    div()
                                        .mt_1()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            Button::new("workspace-status-primary")
                                                .label(if screen.reconnect {
                                                    retry_label
                                                } else {
                                                    "Open Settings"
                                                })
                                                .small()
                                                .when(layout.phone, |button| {
                                                    button.h(layout.control_size())
                                                })
                                                .on_click(cx.listener(|view, _, _, cx| {
                                                    if view.browser_connection_lost.is_some() {
                                                        view.retry_worker_connection(cx);
                                                    } else {
                                                        view.open_settings_from_menu(cx);
                                                    }
                                                })),
                                        )
                                        .when(screen.reconnect, |row| {
                                            row.child(
                                                Button::new("workspace-status-settings")
                                                    .icon(Icon::new(IconName::Settings))
                                                    .ghost()
                                                    .when(layout.phone, |button| {
                                                        button
                                                            .large()
                                                            .h(layout.control_size())
                                                            .w(layout.control_size())
                                                    })
                                                    .when(!layout.phone, |button| button.xsmall())
                                                    .on_click(cx.listener(|view, _, _, cx| {
                                                        view.open_settings_from_menu(cx);
                                                    })),
                                            )
                                        }),
                                )
                            }),
                    ),
            )
            .when(
                startup_screen_shows_settings(self.settings_open(), self.browser_starting),
                |element| {
                    let section = self.settings_section().unwrap_or(SettingsSection::Agents);
                    element.child(self.render_settings_dialog(section, layout, cx))
                },
            )
            .into_any()
    }
}
