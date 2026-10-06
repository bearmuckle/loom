use super::*;

impl Render for LoomView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(px(
            BASE_FONT_SIZE * self.font_scale_percent as f32 / DEFAULT_FONT_SCALE_PERCENT as f32
        ));
        window.set_window_title(&format!("Loom - {}", self.active_session.name));
        #[cfg(target_family = "wasm")]
        if !self.browser_window_initialized {
            self.browser_window_initialized = true;
            self.observe_system_appearance(window, cx);
            self.composer_focus_handle.focus(window, cx);
            self.select_theme(ThemeChoice::System, window, cx);
        }
        if self.shortcut_interceptor.is_none() {
            // Keystrokes are dispatched along the focus path, so a view-level
            // handler stops working whenever focus is lost (for example after
            // the command palette input is torn down). Intercept them globally
            // so these shortcuts always fire.
            let view = cx.entity().downgrade();
            self.shortcut_interceptor = Some(cx.intercept_keystrokes(move |event, _window, cx| {
                let modifiers = event.keystroke.modifiers;
                if (modifiers.platform || modifiers.control)
                    && modifiers.shift
                    && event.keystroke.key.eq_ignore_ascii_case("p")
                {
                    let _ = view.update(cx, |view, cx| view.toggle_command_palette(cx));
                    cx.stop_propagation();
                } else if event.keystroke.key == "escape" {
                    let _ = view.update(cx, |view, cx| {
                        if view.command_palette_open {
                            view.close_command_palette(cx);
                            cx.stop_propagation();
                        }
                    });
                }
            }));
        }
        if self.composer_input.is_none() {
            let input = cx.new(|cx| TextareaState::new(window, cx));
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => view.refresh_composer_completion(cx),
                    InputEvent::PressEnter { shift: false, .. } => {
                        if view.composer_completion.is_some() {
                            view.pending_completion_accept = true;
                            cx.notify();
                        } else {
                            view.submit_composer(cx);
                        }
                    }
                    _ => {}
                },
            ));
            self.composer_input = Some(input);
        }
        if self.clear_composer_on_render {
            if let Some(input) = self.composer_input.as_ref() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.clear_composer_on_render = false;
        }
        if self.pending_completion_accept {
            self.pending_completion_accept = false;
            self.accept_composer_completion(window, cx);
        }
        if self.command_palette_open && self.command_palette_input.is_none() {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Type a command or search…"));
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter { .. } => view.confirm_command_palette(cx),
                    _ => {}
                },
            ));
            self.command_palette_input = Some(input);
        } else if !self.command_palette_open && self.command_palette_input.is_some() {
            if let Some(input) = self.command_palette_input.as_ref() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.command_palette_input = None;
        }
        if self.session_filter_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter projects…"));
            self.input_subscriptions
                .push(cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }));
            self.session_filter_input = Some(input);
        }
        if self.review_filter_input.is_none() {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Filter changed files…"));
            self.input_subscriptions
                .push(cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }));
            self.review_filter_input = Some(input);
        }
        if self.node_input_state.is_none() {
            let initial_value = self.node_input_initial.clone();
            let input = cx.new(|cx| InputState::new(window, cx).default_value(initial_value));
            if self.settings_open {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.connect_worker_node(cx);
                    }
                },
            ));
            self.node_input_state = Some(input);
        }
        if self.clear_node_on_render {
            if let Some(input) = self.node_input_state.as_ref() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.clear_node_on_render = false;
        }
        #[cfg(target_family = "wasm")]
        if !self.connected {
            return self.render_disconnected(window, cx);
        }
        if self.rename_dialog.is_some() && self.rename_input_state.is_none() {
            let initial_value = self
                .rename_dialog
                .as_ref()
                .map(|dialog| dialog.input.clone())
                .unwrap_or_default();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(initial_value)
                    .placeholder("Session name")
            });
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.confirm_rename(cx);
                    }
                },
            ));
            self.rename_input_state = Some(input);
        }
        if self.settings_open && self.settings_section == SettingsSection::Providers {
            for provider in self
                .providers
                .iter()
                .filter(|provider| provider.api_key_configurable)
            {
                if !self.provider_api_key_inputs.contains_key(&provider.id) {
                    let placeholder = format!("{} API key", provider.display_name);
                    self.provider_api_key_inputs.insert(
                        provider.id.clone(),
                        cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(placeholder)
                                .masked(true)
                        }),
                    );
                }
            }
        } else {
            for input in self.provider_api_key_inputs.values() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.provider_api_key_inputs.clear();
            self.provider_setup_status.clear();
        }
        if let Some(path) = self.pending_source_path.take() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(path)
                    .placeholder("Absolute folder path")
            });
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.confirm_source_dialog(cx);
                    }
                },
            ));
            self.source_path_input = Some(input);
        }
        if self
            .source_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.choice == SessionSourceChoice::LocalDirectory)
            && self.source_path_input.is_none()
            && self.pending_source_path.is_none()
        {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Absolute folder path"));
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.confirm_source_dialog(cx);
                    }
                },
            ));
            self.source_path_input = Some(input);
        }
        if self.source_dialog.as_ref().is_some_and(|dialog| {
            dialog.choice == SessionSourceChoice::GitHub && dialog.filter_subscription.is_none()
        }) {
            let filter = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Search GitHub repositories (2+ characters)")
            });
            filter.update(cx, |state, cx| state.focus(window, cx));
            let subscription = cx.subscribe(&filter, |view, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    view.on_repository_search_changed(cx);
                }
            });
            self.repository_filter_input = Some(filter);
            if let Some(dialog) = &mut self.source_dialog {
                dialog.filter_subscription = Some(subscription);
            }
        }
        self.schedule_worker_node_poll(cx);
        self.sync_model_select_states(window, cx);
        self.sync_agent_mode_select_state(window, cx);
        self.schedule_run_poll(cx);
        self.schedule_project_poll(cx);
        let view = cx.entity();
        let layout = responsive_layout(window.bounds().size.width);
        let review_panel_visible = review_panel_is_visible(
            layout,
            self.review.open,
            self.sessions.len(),
            self.settings_open,
            self.github_login.is_some(),
        );
        let panel_layout = h_resizable("loom-workspace-panels")
            .with_handle_appearance(Rc::new(|handle, _, _| {
                let active = handle.is_active();
                let line = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(2.))
                    .w(px(1.))
                    .bg(rgb(0x30343f));
                let grip = div()
                    .w(px(5.))
                    .h(px(28.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(0x60a5fa))
                    .opacity(0.)
                    .group_hover("handle", |element| element.opacity(1.))
                    .when(active, |element| element.opacity(1.).h(px(40.)));
                Some(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .relative()
                                .w(px(5.))
                                .h_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(line)
                                .child(grip),
                        )
                        .into_any(),
                )
            }))
            .children([
                resizable_panel()
                    .size(layout.sidebar_width)
                    .size_range(px(150.)..px(420.))
                    .flex_none()
                    .visible(!layout.phone)
                    .child(div().size_full().when(!layout.phone, |element| {
                        element.child(self.render_session_sidebar(&view, layout, cx))
                    })),
                resizable_panel().min_w(px(0.)).child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .relative()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(rgb(0x14161a))
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .items_center()
                            .gap_2()
                            .when(layout.phone, |element| {
                                element.child(
                                    Button::new("open-session-drawer")
                                        .label("Projects")
                                        .small()
                                        .h(layout.control_size())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.session_drawer_open = true;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                session_header_title()
                                    .child(
                                        div()
                                            .truncate()
                                            .child(self.active_session.name.clone()),
                                    )
                                    .when(!layout.phone, |element| {
                                        element.child(
                                            div()
                                                .truncate()
                                                .text_xs()
                                                .text_color(rgb(0x8f98a6))
                                                .child(
                                                    self.review
                                                        .vcs
                                                        .as_ref()
                                                        .map(|status| {
                                                            format!(
                                                                "{}  ·  {}",
                                                                status
                                                                    .branch
                                                                    .as_deref()
                                                                    .unwrap_or("detached"),
                                                                if status.clean {
                                                                    "clean"
                                                                } else {
                                                                    "modified"
                                                                }
                                                            )
                                                        })
                                                        .unwrap_or_else(|| {
                                                            let sources = self
                                                                .session_directories
                                                                .len()
                                                                + self.session_repositories.len();
                                                            match sources {
                                                                0 => "No source attached".to_owned(),
                                                                1 => "1 source".to_owned(),
                                                                count => format!("{count} sources"),
                                                            }
                                                        }),
                                                ),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        session_header_actions()
                            .child(header_tooltip("session-sources-tooltip", "Session sources",
                        Button::new("session-sources")
                            .icon(Icon::new(AssetIconName::ListTree))
                            .ghost()
                            .when(layout.phone, |button| {
                                button
                                    .large()
                                    .h(layout.control_size())
                                    .w(layout.control_size())
                            })
                            .when(!layout.phone, |button| button.small())
                            .accessibility_label("Session sources")
                            .dropdown_menu({
                                let view = view.clone();
                                let repositories = self.session_repositories.clone();
                                let directories = self.session_directories.clone();
                                let selected_repository_id = self.selected_repository_id;
                                move |mut menu, window, cx| {
                                    menu = menu.label("Session sources");
                                    let add_view = view.clone();
                                    menu = menu.item(PopupMenuItem::new("Add source…").on_click(
                                        move |_, _, cx| {
                                            add_view.update(cx, |this, cx| {
                                                this.begin_source_dialog(
                                                    SessionSourceDialogPurpose::AddToSession,
                                                    cx,
                                                );
                                            });
                                        },
                                    ));
                                    if directories.is_empty() && repositories.is_empty() {
                                        menu = menu.separator().label("No sources attached");
                                    } else {
                                        menu = menu.separator();
                                    }
                                    for directory in &directories {
                                        let detach_view = view.clone();
                                        let path = directory.path.clone();
                                        let root_repository = repositories.iter().find(|repository| repository.path == directory.path);
                                        let name = Path::new(&directory.source)
                                            .file_name()
                                            .map(|name| name.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| directory.source.clone());
                                        let label = if root_repository.is_some() {
                                            format!("Git repository: {name}")
                                        } else {
                                            format!("Folder: {name}")
                                        };
                                        let source_menu = PopupMenu::build(window, cx, |mut menu, _, _| {
                                            menu = menu.label(directory.source.clone());
                                            if let Some(repository) = root_repository {
                                                let repository_id = repository.id;
                                                let select_view = view.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new("Select for review")
                                                        .checked(selected_repository_id == Some(repository_id))
                                                        .on_click(move |_, _, cx| {
                                                            select_view.update(cx, |this, cx| {
                                                                this.select_session_repository(repository_id, cx);
                                                            });
                                                        }),
                                                );
                                            }
                                            menu.item(PopupMenuItem::new("Detach source")
                                                .on_click(move |_, _, cx| {
                                                    detach_view.update(cx, |this, cx| {
                                                        this.detach_session_directory(path.clone(), cx);
                                                    });
                                                }))
                                        });
                                        menu = menu.item(PopupMenuItem::submenu(
                                            label, source_menu,
                                        ));
                                    }
                                    let other_repositories = repositories.iter().filter(|repository| {
                                        !directories.iter().any(|directory| directory.path == repository.path)
                                    }).collect::<Vec<_>>();
                                    if !other_repositories.is_empty() {
                                        menu = menu.separator().label("Git repositories");
                                        for repository in other_repositories {
                                            let repository_id = repository.id;
                                            let select_view = view.clone();
                                            let detach_view = view.clone();
                                            let source_menu = PopupMenu::build(window, cx, |menu, _, _| {
                                                menu.label(repository.source.clone()).item(
                                                    PopupMenuItem::new("Select for review")
                                                        .checked(selected_repository_id == Some(repository_id))
                                                        .on_click(move |_, _, cx| {
                                                            select_view.update(cx, |this, cx| {
                                                                this.select_session_repository(repository_id, cx);
                                                            });
                                                        }),
                                                )
                                                .item(
                                                    PopupMenuItem::new("Detach source")
                                                        .on_click(move |_, _, cx| {
                                                            detach_view.update(cx, |this, cx| {
                                                                this.detach_session_repository(repository_id, cx);
                                                            });
                                                        }),
                                                )
                                            });
                                            let name = Path::new(&repository.source)
                                                .file_name()
                                                .map(|name| name.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| repository.source.clone());
                                            menu = menu.item(PopupMenuItem::submenu(name, source_menu));
                                        }
                                    }
                                    menu
                                }
                            }),
                            ))
                            .child({
                                let toggle = Button::new("toggle-review-sidebar")
                                    .icon(Icon::new(if self.review.open {
                                        IconName::PanelRightClose
                                    } else {
                                        IconName::PanelRightOpen
                                    }))
                                    .ghost()
                                    .when(layout.phone, |button| {
                                        button
                                            .large()
                                            .h(layout.control_size())
                                            .w(layout.control_size())
                                    })
                                    .when(!layout.phone, |button| button.small())
                                    .accessibility_label("Toggle side panel")
                                    .when(self.review.open, |button| button.secondary())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_review_pane(cx);
                                    }));
                                div()
                                    .relative()
                                    .when(self.review.has_unread(), |element| {
                                        element.child(
                                            div()
                                                .id("review-unread-dot")
                                                .test_support()
                                                .absolute()
                                                .top(px(1.))
                                                .right(px(1.))
                                                .w(px(6.))
                                                .h(px(6.))
                                                .child(Badge::new().dot()),
                                        )
                                    })
                                    .child(toggle)
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .id("timeline-scroll")
                    .overflow_hidden()
                    .child(self.timeline_entity(cx)),
            )
            .child(self.render_composer(layout, window, cx))
            .when(self.sessions.is_empty(), |element| {
                element.child(
                    div()
                        .absolute()
                        .top(px(0.))
                        .right(px(0.))
                        .bottom(px(0.))
                        .left(px(0.))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_4()
                        .bg(rgb(0x111318))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(AssetIconName::Sparkles)
                                        .size_8()
                                        .text_color(rgb(0x93c5fd)),
                                )
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(1.25))
                                        .text_color(rgb(0xf3f4f6))
                                        .child("Work with agents, keep the trail"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(0x8f98a6))
                                        .child("Start a session over a repository or folder."),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .text_xs()
                                .text_color(rgb(0x64748b))
                                .child("/ commands")
                                .child("@ files")
                                .child(format!(
                                    "{} command palette",
                                    command_palette_shortcut_label()
                                )),
                        )
                        .child(
                            Button::new("start-first-session")
                                .label("Create a project")
                                .primary()
                                .on_click(cx.listener(Self::new_session)),
                        ),
                )
            })
            .when(self.rename_dialog.is_some(), |element| {
                element.child(self.render_rename_dialog(cx))
            })
            .when(self.settings_open, |element| {
                element.child(self.render_settings_dialog(layout, cx))
            })
            .when(self.github_login.is_some(), |element| {
                element.child(self.render_github_login_dialog(cx))
            })
            .when(self.source_dialog.is_some(), |element| {
                element.child(self.render_source_dialog(layout, cx))
            }),
            ),
                resizable_panel()
                    .size(layout.review_width)
                    .size_range(px(340.)..px(900.))
                    .flex_none()
                    .visible(review_panel_visible)
                    .child(div().size_full().when(review_panel_visible, |element| {
                        element.child(self.render_inspector(window, cx))
                    })),
            ]);
        let content = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .text_size(gpui_kit::rems(0.8125))
            // Initializes the per-frame selection registry before selectable
            // text participants prepaint and register themselves.
            .child(TextSelectionLayer)
            .when(!cfg!(target_family = "wasm"), |element| {
                element.child(
                    div()
                        .h(px(30.))
                        .w_full()
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .bg(rgb(0x1b1d24))
                        .border_b_1()
                        .border_color(rgb(0x30343f))
                        .when(cfg!(target_os = "macos"), |element| element.pl(px(72.)))
                        .child(
                            div()
                                .id("window-titlebar-drag")
                                .flex()
                                .flex_1()
                                .items_center()
                                .gap_2()
                                .cursor_pointer()
                                .window_control_area(WindowControlArea::Drag)
                                .on_mouse_down(MouseButton::Left, |event, window, _| {
                                    if event.click_count == 2 {
                                        window.zoom_window();
                                    } else {
                                        window.start_window_move();
                                    }
                                })
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div().text_xs().text_color(rgb(0x8f98a6)).child("Loom"),
                                        )
                                        .child(div().text_xs().text_color(rgb(0x64748b)).child(
                                            if self.demo_workspace { "Demo" } else { "Local" },
                                        )),
                                ),
                        )
                        .child(
                            div().flex().items_center().gap_1().ml_2().child(
                                div()
                                    .id("window-close")
                                    .w(px(22.))
                                    .h(px(22.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_sm()
                                    .text_sm()
                                    .text_color(rgb(0xb7c0d0))
                                    .hover(|style| {
                                        style.bg(rgb(0x7f1d1d)).text_color(rgb(0xffffff))
                                    })
                                    .cursor_pointer()
                                    .window_control_area(WindowControlArea::Close)
                                    .child(Icon::new(IconName::Close).size_4())
                                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    })
                                    .on_click(|_, window, cx| {
                                        cx.stop_propagation();
                                        window.remove_window();
                                    }),
                            ),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .flex()
                    .relative()
                    .overflow_hidden()
                    .child(panel_layout)
                    .when(layout.phone && self.session_drawer_open, |row| {
                        row.child(
                            div()
                                .id("mobile-session-backdrop")
                                .size_full()
                                .absolute()
                                .top(px(0.))
                                .left(px(0.))
                                .bg(gpui_kit::hsla(0., 0., 0., 0.55))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.session_drawer_open = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("mobile-session-drawer")
                                .test_support()
                                .absolute()
                                .top(px(0.))
                                .bottom(px(0.))
                                .left(px(0.))
                                .w(layout.sidebar_width)
                                // The full-width drawer covers the panes, so it
                                // must capture taps instead of letting them
                                // reach the header and timeline behind it.
                                .occlude()
                                .shadow_lg()
                                .child(self.render_session_sidebar(&view, layout, cx)),
                        )
                    })
                    .when(
                        layout.phone
                            && self.review.open
                            && !self.sessions.is_empty()
                            && !self.settings_open
                            && self.github_login.is_none(),
                        |element| element.child(self.render_inspector(window, cx)),
                    ),
            )
            .when(self.command_palette_open, |element| {
                element.child(self.render_command_palette(cx))
            });
        content.into_any()
    }
}
