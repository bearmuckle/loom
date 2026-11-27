use super::*;

impl LoomView {
    /// The label shown in the session model picker when `SelectState` has no
    /// catalog entry selected.
    ///
    /// If a model was used for the run (or an explicit model switch is queued
    /// while the catalog refreshes), the footer shows that model instead of
    /// claiming that no model is configured. Only a session with no run model
    /// and no configured catalog models shows "No model is configured".
    pub(crate) fn model_picker_placeholder(&self) -> String {
        let active_node = self
            .session_node_ids
            .get(&self.active_session.id)
            .map(String::as_str)
            .unwrap_or(&self.default_backend_node_id);
        let active_provider_names = self.node_model_provider_names.get(active_node);
        let presentation = self.session_run_presentation();
        let has_named_model = {
            let raw = self.model.as_str().trim();
            !raw.is_empty() && raw != "default" && raw != "deterministic/demo"
        };
        if let Some(run_model) = presentation.run_model.as_ref() {
            if has_named_model {
                return model_choice_label(&self.model, active_provider_names);
            }
            return model_choice_label(run_model, active_provider_names);
        }
        if !self.models.is_empty() {
            if has_named_model {
                return model_choice_label(&self.model, active_provider_names);
            }
            return "Select a model".to_owned();
        }
        if self.model_refreshes_in_flight.contains(active_node) && has_named_model {
            return model_choice_label(&self.model, active_provider_names);
        }
        "No model is configured".to_owned()
    }

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
            let placeholder = self.model_picker_placeholder();
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
                            .placeholder(placeholder)
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
                                                        view.open_settings_section(
                                                            SettingsSection::Workers,
                                                            cx,
                                                        );
                                                    }
                                                })),
                                        )
                                        .when(screen.reconnect, |row| {
                                            row.child(
                                                Button::new("workspace-status-settings")
                                                    .icon(Icon::new(IconName::Settings))
                                                    .accessibility_label("Open Settings")
                                                    .ghost()
                                                    .when(layout.phone, |button| {
                                                        button
                                                            .large()
                                                            .h(layout.control_size())
                                                            .w(layout.control_size())
                                                    })
                                                    .when(!layout.phone, |button| button.xsmall())
                                                    .on_click(cx.listener(|view, _, _, cx| {
                                                        view.open_settings_section(
                                                            SettingsSection::Workers,
                                                            cx,
                                                        );
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

#[cfg(not(target_family = "wasm"))]
impl LoomView {
    /// The first-run setup screen: install a worker on this machine, or reach a
    /// worker that already runs.
    pub(crate) fn render_worker_setup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        self.ensure_setup_inputs(window, cx);
        let layout = responsive_layout(window.bounds().size.width);
        let Some(setup) = self.setup.as_ref() else {
            return div().into_any();
        };
        let busy = setup.busy;
        let progress = setup.progress.clone();
        let error = setup.error.clone();
        let existing = setup.existing.clone();
        let url_input = setup
            .url_input
            .clone()
            .expect("setup URL input initialized before rendering");
        let token_input = setup
            .token_input
            .clone()
            .expect("setup token input initialized before rendering");
        let heading = if existing.is_some() {
            "Worker not reachable"
        } else {
            "Set up a worker"
        };
        let description = match &existing {
            Some(worker) => format!(
                "Loom could not reach the worker you set up at {}. It may still be starting, so \
                 retry first; reinstalling restarts the worker and stops its current work.",
                worker.url
            ),
            None => "Loom's client needs a worker to run sessions. Connect to a worker you \
                     already run, or install one on this machine."
                .to_owned(),
        };
        let connect_heading = if existing.is_some() {
            "Or connect to a different worker"
        } else {
            "Connect to a worker"
        };
        let connect_hint = "ws://host:port/ws with its access token, or ssh://user@host to \
                            install a worker over SSH.";
        let install_hint = "Downloads the latest loom-server release and runs it as a user \
                            service that also starts at boot.";
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
                            .id("worker-setup-card")
                            .test_support()
                            .role(Role::Region)
                            .aria_label(heading)
                            .w_full()
                            .max_w(px(560.))
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_6()
                            .rounded_lg()
                            .bg(rgb(0x171c25))
                            .border_1()
                            .border_color(rgb(0x293244))
                            .child(
                                div()
                                    .id("worker-setup-heading")
                                    .test_support()
                                    .role(Role::Heading)
                                    .aria_level(1)
                                    .aria_label(heading)
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(heading),
                            )
                            .child(
                                div()
                                    .id("worker-setup-description")
                                    .test_support()
                                    .role(Role::Label)
                                    .aria_label(description.clone())
                                    .text_sm()
                                    .text_color(rgb(0x8f98a6))
                                    .child(description),
                            )
                            .when_some(existing.clone(), |card, _| {
                                card.child(
                                    Button::new("worker-setup-retry")
                                        .label("Retry")
                                        .primary()
                                        .small()
                                        .disabled(busy)
                                        .when(layout.phone, |button| {
                                            button.h(layout.control_size())
                                        })
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.retry_initial_worker(cx)
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .id("worker-setup-connect-heading")
                                    .test_support()
                                    .role(Role::Heading)
                                    .aria_level(2)
                                    .aria_label(connect_heading)
                                    .text_sm()
                                    .child(connect_heading),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        KitInput::new(&url_input)
                                            .id("worker-setup-url")
                                            .aria_label("Worker URL")
                                            .small(),
                                    )
                                    .child(
                                        KitInput::new(&token_input)
                                            .id("worker-setup-token")
                                            .aria_label("Access token (optional)")
                                            .content_type(InputContentType::Password)
                                            .small(),
                                    )
                                    .child(
                                        div().flex().justify_end().child(
                                            Button::new("worker-setup-connect")
                                                .label("Connect")
                                                .when(existing.is_none(), Button::primary)
                                                .small()
                                                .disabled(busy)
                                                .when(layout.phone, |button| {
                                                    button.h(layout.control_size())
                                                })
                                                .on_click(cx.listener(|view, _, _, cx| {
                                                    view.connect_setup_worker(cx)
                                                })),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .id("worker-setup-connect-hint")
                                    .test_support()
                                    .role(Role::Label)
                                    .aria_label(connect_hint)
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child(connect_hint),
                            )
                            .when_some(progress, |card, progress| {
                                card.child(
                                    div()
                                        .id("worker-setup-progress")
                                        .test_support()
                                        .role(Role::Status)
                                        .aria_label(progress.clone())
                                        .text_xs()
                                        .text_color(rgb(0xfcd34d))
                                        .child(progress),
                                )
                            })
                            .when_some(error, |card, error| {
                                card.child(
                                    div()
                                        .id("worker-setup-error")
                                        .test_support()
                                        .role(Role::Alert)
                                        .aria_label(error.clone())
                                        .text_xs()
                                        .text_color(rgb(0xfca5a5))
                                        .child(error),
                                )
                            })
                            .child(div().h(px(1.)).w_full().bg(rgb(0x293244)))
                            .when(existing.is_none(), |card| {
                                card.child(
                                    div()
                                        .id("worker-setup-install-heading")
                                        .test_support()
                                        .role(Role::Heading)
                                        .aria_level(2)
                                        .aria_label("Or install on this machine")
                                        .text_sm()
                                        .child("Or install on this machine"),
                                )
                            })
                            .child(
                                div()
                                    .id("worker-setup-install-hint")
                                    .test_support()
                                    .role(Role::Label)
                                    .aria_label(install_hint)
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child(install_hint),
                            )
                            .child(
                                Button::new("worker-setup-install")
                                    .label(if existing.is_some() {
                                        "Reinstall or update (restarts the worker)"
                                    } else {
                                        "Install a worker on this machine"
                                    })
                                    .when(existing.is_some(), |button| button.ghost())
                                    .when(existing.is_none(), |button| button.outline())
                                    .small()
                                    .disabled(busy)
                                    .when(layout.phone, |button| button.h(layout.control_size()))
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.install_local_worker(cx)),
                                    ),
                            ),
                    ),
            )
            .into_any()
    }

    fn ensure_setup_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .setup
            .as_ref()
            .is_some_and(|setup| setup.url_input.is_none())
        {
            let auto_focus = self
                .setup
                .as_ref()
                .is_some_and(|setup| setup.existing.is_none());
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder("ws://127.0.0.1:8765/ws or ssh://user@host")
            });
            if auto_focus {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        if let Some(setup) = view.setup.as_mut() {
                            setup.error = None;
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => view.connect_setup_worker(cx),
                    _ => {}
                },
            ));
            if let Some(setup) = self.setup.as_mut() {
                setup.url_input = Some(input);
            }
        }
        if self
            .setup
            .as_ref()
            .is_some_and(|setup| setup.token_input.is_none())
        {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Access token (optional)")
                    .masked(true)
            });
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        if let Some(setup) = view.setup.as_mut() {
                            setup.error = None;
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => view.connect_setup_worker(cx),
                    _ => {}
                },
            ));
            if let Some(setup) = self.setup.as_mut() {
                setup.token_input = Some(input);
            }
        }
    }
}
