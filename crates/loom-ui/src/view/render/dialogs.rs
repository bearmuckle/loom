use super::*;

impl LoomView {
    pub(crate) fn render_rename_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dialog) = &self.rename_dialog else {
            return div().into_any();
        };
        div()
            .id("rename-dialog")
            .absolute()
            .top(px(120.))
            .left(px(280.))
            .w(px(420.))
            .p_3()
            .rounded_lg()
            .bg(rgb(0x1b1d24))
            .border_1()
            .border_color(rgb(0x3b4555))
            .shadow_lg()
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf3f4f6))
                    .child(if dialog.is_project {
                        "Rename project"
                    } else {
                        "Rename session"
                    }),
            )
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child(format!("Current name: {}", dialog.session.name)),
            )
            .child(
                div().mt_3().child(
                    KitInput::new(
                        self.rename_input_state
                            .as_ref()
                            .expect("rename input initialized before rendering"),
                    )
                    .id("rename-session-input")
                    .small(),
                ),
            )
            .child(
                div()
                    .mt_3()
                    .flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        div()
                            .id("cancel-rename")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x242833))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_sm()
                            .cursor_pointer()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename_dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("confirm-rename")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x2563eb))
                            .text_sm()
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .child("Rename")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm_rename(cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any()
    }

    pub(crate) fn render_source_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dialog) = &self.source_dialog else {
            return div().into_any();
        };
        let is_start = dialog.purpose == SessionSourceDialogPurpose::StartSession;
        let selected_repo = dialog.selected_repository.as_deref();
        let repository_query = self
            .repository_filter_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let mut repository_rows = div().mt_2().flex().flex_col().gap_1();
        let mut filtered_repository_count = 0;
        for repository in &dialog.repositories {
            let searchable = format!(
                "{} {}",
                repository.full_name,
                repository.description.as_deref().unwrap_or_default()
            )
            .to_lowercase();
            if !searchable.contains(&repository_query) {
                continue;
            }
            filtered_repository_count += 1;
            let name = repository.full_name.clone();
            let selected = selected_repo == Some(name.as_str());
            repository_rows = repository_rows.child(
                div()
                    .id(format!("github-repository-{name}"))
                    .p_2()
                    .rounded_sm()
                    .bg(if selected {
                        rgb(0x263b58)
                    } else {
                        rgb(0x171c25)
                    })
                    .border_1()
                    .border_color(if selected {
                        rgb(0x2563eb)
                    } else {
                        rgb(0x293244)
                    })
                    .cursor_pointer()
                    .child(div().text_sm().child(name.clone()))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                        repository.description.clone().unwrap_or_else(|| {
                            if repository.private {
                                "Private repository"
                            } else {
                                "Public repository"
                            }
                            .to_owned()
                        }),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(dialog) = &mut this.source_dialog {
                            dialog.selected_repository = Some(name.clone());
                        }
                        cx.notify();
                    })),
            );
        }

        let mut dialog_body = div().mt_3();
        if dialog.choice == SessionSourceChoice::LocalDirectory {
            dialog_body = dialog_body
                .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                    "Attach a local folder in place. Git repositories in that folder are available for review.",
                ))
                .child(
                    div()
                        .mt_2()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(34.))
                                .px_2()
                                .rounded_sm()
                                .bg(rgb(0x0f1115))
                                .border_1()
                                .border_color(rgb(0x3b4555))
                                .child(
                                    KitInput::new(
                                        self.source_path_input
                                            .as_ref()
                                            .expect("source input initialized before rendering"),
                                    )
                                    .id("local-session-directory-path")
                                    .appearance(false)
                                    .bordered(false),
                                ),
                        )
                        .when(cfg!(not(target_family = "wasm")), |row| {
                            row.child(
                                Button::new("browse-local-session-directory")
                                    .label("Browse…")
                                    .small()
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.browse_local_directory(cx);
                                    })),
                            )
                        }),
                );
        } else if dialog.choice == SessionSourceChoice::GitHub {
            dialog_body = if dialog.repositories_loading {
                dialog_body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child("Loading repositories…"),
                )
            } else if let Some(error) = &dialog.error {
                dialog_body
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xfca5a5))
                            .child(error.clone()),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child("Connect GitHub in Settings to browse repositories."),
                    )
                    .child(
                        Button::new("connect-github-from-repository-picker")
                            .label("Open GitHub settings")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.source_dialog = None;
                                this.open_settings_from_menu(cx);
                            })),
                    )
            } else if dialog.repositories.is_empty() {
                dialog_body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child("No repositories found."),
                )
            } else {
                dialog_body
                    .child(
                        KitInput::new(
                            self.repository_filter_input
                                .as_ref()
                                .expect("repository filter initialized before rendering"),
                        )
                        .id("github-repository-filter")
                        .small()
                        .into_any_element(),
                    )
                    .child(if filtered_repository_count == 0 {
                        div()
                            .mt_2()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No repositories match this filter.")
                            .into_any_element()
                    } else {
                        div()
                            .max_h(px(280.))
                            .overflow_y_scrollbar()
                            .child(repository_rows)
                            .into_any_element()
                    })
            };
        } else {
            dialog_body = dialog_body
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Start a new project with no files or repositories.");
        }
        if let Some(error) = &dialog.error
            && dialog.choice != SessionSourceChoice::GitHub
        {
            dialog_body = dialog_body.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        }

        Dialog::new(cx)
            .title(if is_start {
                "New project"
            } else {
                "Add to this session"
            })
            .on_close(cx.listener(|this, _, _, cx| {
                this.source_dialog = None;
                cx.notify();
            }))
            .keyboard(false)
            .overlay_closable(false)
            .w(px(560.))
            .max_h(px(600.))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child(if is_start {
                                "Choose what the new project starts with."
                            } else {
                                "Choose a repository or folder to add to the active session."
                            }),
                    )
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .gap_1()
                            .when(is_start, |row| {
                                row.child(
                                    Button::new("source-empty")
                                        .label("Empty project")
                                        .small()
                                        .when(
                                            dialog.choice == SessionSourceChoice::Empty,
                                            |button| button.primary(),
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.choose_source(SessionSourceChoice::Empty, cx)
                                        })),
                                )
                            })
                            .when(dialog.local_directory_available, |row| {
                                row.child(
                                    Button::new("source-local-directory")
                                        .label("Local folder")
                                        .small()
                                        .when(
                                            dialog.choice == SessionSourceChoice::LocalDirectory,
                                            |button| button.primary(),
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.choose_source(
                                                SessionSourceChoice::LocalDirectory,
                                                cx,
                                            )
                                        })),
                                )
                            })
                            .child(
                                Button::new("source-github")
                                    .label("GitHub repository")
                                    .small()
                                    .when(dialog.choice == SessionSourceChoice::GitHub, |button| {
                                        button.primary()
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_source(SessionSourceChoice::GitHub, cx)
                                    })),
                            ),
                    )
                    .child(dialog_body)
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .justify_end()
                            .gap_1()
                            .child(
                                Button::new("cancel-session-source")
                                    .label("Cancel")
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.source_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-session-source")
                                    .label(if is_start {
                                        "Create project"
                                    } else {
                                        "Add to session"
                                    })
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_source_dialog(cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_github_login_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(state) = &self.github_login else {
            return div().into_any();
        };
        let body = match state {
            GitHubLoginState::Starting => div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Requesting a GitHub device code..."),
            GitHubLoginState::Awaiting {
                verification_uri,
                user_code,
                expires_in,
            } => div()
                .text_sm()
                .text_color(rgb(0xe5e7eb))
                .child("Open this URL in a browser:")
                .child(
                    div()
                        .mt_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_color(rgb(0x93c5fd))
                                .child(verification_uri.clone()),
                        )
                        .child(
                            div()
                                .id("open-github-verification-url")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x2563eb))
                                .hover(|style| style.bg(rgb(0x1d4ed8)))
                                .text_xs()
                                .text_color(rgb(0xffffff))
                                .cursor_pointer()
                                .child("Open")
                                .on_click({
                                    let verification_uri = verification_uri.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        if let Err(error) = open_external_url(&verification_uri) {
                                            this.record_status(format!(
                                                "Could not open GitHub URL: {error}"
                                            ));
                                        }
                                        cx.notify();
                                    })
                                }),
                        )
                        .child(
                            div()
                                .id("copy-github-verification-url")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .cursor_pointer()
                                .child("Copy")
                                .on_click({
                                    let verification_uri = verification_uri.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.copy_github_login_value(
                                            verification_uri.clone(),
                                            "verification URL",
                                            cx,
                                        );
                                    })
                                }),
                        ),
                )
                .child(
                    div()
                        .mt_3()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_color(rgb(0xfef3c7))
                                .child(format!("Enter code: {user_code}")),
                        )
                        .child(
                            div()
                                .id("copy-github-user-code")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .cursor_pointer()
                                .child("Copy code")
                                .on_click({
                                    let user_code = user_code.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.copy_github_login_value(
                                            user_code.clone(),
                                            "device code",
                                            cx,
                                        );
                                    })
                                }),
                        ),
                )
                .child(
                    div()
                        .mt_1()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(format!(
                            "Waiting for authorization (expires in {expires_in}s)"
                        )),
                ),
            #[cfg(not(target_family = "wasm"))]
            GitHubLoginState::Completing => div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Authorization received. Saving credentials..."),
            GitHubLoginState::Success => div()
                .text_sm()
                .text_color(rgb(0x9ad7bd))
                .child("GitHub is connected. Repository browsing and the GitHub Copilot provider are available."),
            GitHubLoginState::Error(error) => div()
                .text_sm()
                .text_color(rgb(0xfca5a5))
                .child(error.clone()),
        };
        div()
            .id("github-login-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child("Connect GitHub account"),
                    )
                    .child(
                        div()
                            .id("close-github-login")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close GitHub connection".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.github_login = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(div().mt_4().child(body))
            .into_any()
    }

    pub(crate) fn render_settings_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let default_model_body = self.default_model_select.as_ref().map_or_else(
            || div().into_any_element(),
            |state| {
                Select::new(state)
                    .id("default-model-select")
                    .w_full()
                    .small()
                    .accessibility_label("Default model for new sessions")
                    .placeholder("No configured models are available")
                    .search_placeholder("Search models")
                    .into_any_element()
            },
        );

        let section = self.settings_section;
        let content: gpui_kit::AnyElement = match section {
            SettingsSection::Agents => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("AGENTS"))
                .child(
                    settings_card()
                        .child(settings_row(
                            "Auto-approve non-destructive actions",
                            "In Agent and Edit, writes, commands, and network won't prompt",
                            Switch::new("session-auto-approve-toggle")
                                .checked(self.auto_approve_actions)
                                .disabled(
                                    !self.is_connected()
                                        || self.approval_settings_request_in_flight,
                                )
                                .accessibility_label("Auto-approve non-destructive actions")
                                .on_change({
                                    let view = cx.entity();
                                    move |_checked, _window, cx| {
                                        view.update(cx, |view, cx| {
                                            view.toggle_auto_approve_actions(cx);
                                        });
                                    }
                                }),
                            true,
                        ))
                        .child(settings_row(
                            "Default model for new sessions",
                            "Used when a session has no model of its own",
                            div().w(px(320.)).child(default_model_body),
                            false,
                        ))
                        .child(settings_row(
                            "Session indicator pulse threshold",
                            "Pulse when CPU usage is above this value",
                            settings_stepper(
                                Button::new("cpu-pulse-threshold-decrease")
                                    .label("-")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.cpu_pulse_threshold_percent
                                                == 0,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_cpu_pulse_threshold(-1, cx);
                                    })),
                                format!(
                                    "{}%",
                                    self.workspace_config.cpu_pulse_threshold_percent.min(100)
                                ),
                                Button::new("cpu-pulse-threshold-increase")
                                    .label("+")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.cpu_pulse_threshold_percent
                                                >= 100,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_cpu_pulse_threshold(1, cx);
                                    })),
                            ),
                            false,
                        ))
                        .child(settings_row(
                            "Parallel project agents",
                            "Maximum delegated agents running at once",
                            settings_stepper(
                                Button::new("project-agent-concurrency-decrease")
                                    .label("-")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.project_agent_concurrency
                                                <= loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_project_agent_concurrency(-1, cx);
                                    })),
                                self.workspace_config.project_agent_concurrency.to_string(),
                                Button::new("project-agent-concurrency-increase")
                                    .label("+")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.project_agent_concurrency
                                                >= loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_project_agent_concurrency(1, cx);
                                    })),
                            ),
                            false,
                        )),
                )
                .into_any_element(),
            SettingsSection::Providers => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("PROVIDERS"))
                .child(
                    settings_card().child(
                        div()
                            .w_full()
                            .flex()
                            .items_start()
                            .gap_3()
                            .px_4()
                            .py_3()
                            .child(
                                div()
                                    .w(px(30.))
                                    .h(px(30.))
                                    .flex_shrink_0()
                                    .rounded_md()
                                    .bg(rgb(0x20242c))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(AssetIconName::Globe)
                                            .size_4()
                                            .text_color(rgb(0x93c5fd)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(div().text_sm().child("GitHub"))
                                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                                        "Browse and clone repositories, and add GitHub \
                                                 Copilot as a model provider.",
                                    )),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(if self.github_connected {
                                                rgb(0x9ad7bd)
                                            } else {
                                                rgb(0xfef3c7)
                                            })
                                            .child(if self.github_connected {
                                                "Connected"
                                            } else {
                                                "Not connected"
                                            }),
                                    )
                                    .when(
                                        !self.github_connected && self.login_enabled,
                                        |element| {
                                            element.child(
                                                Button::new("connect-github-account")
                                                    .label("Connect GitHub")
                                                    .small()
                                                    .on_click(
                                                        cx.listener(Self::toggle_github_login),
                                                    ),
                                            )
                                        },
                                    ),
                            ),
                    ),
                )
                .into_any_element(),
            SettingsSection::Workers => {
                let mut card = settings_card();
                if self.worker_nodes.is_empty() {
                    card = card.child(
                        div()
                            .px_4()
                            .py_3()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child("No worker connected. Add one below to load your sessions."),
                    );
                }
                card = card.children(self.worker_nodes.iter().enumerate().map(|(index, node)| {
                    let id = node.id;
                    let status = &node.status;
                    let node_id = status.node_id.clone();
                    let resources = &status.resources;
                    let connection_label = match node.connection_state {
                        WorkerConnectionState::Disconnected => "not connected",
                        WorkerConnectionState::Connecting => "connecting",
                        WorkerConnectionState::Connected if status.online => "connected · online",
                        WorkerConnectionState::Connected => "connected · offline",
                        WorkerConnectionState::Failed => "connection failed",
                    };
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .py_3()
                        .when(index > 0, |element| {
                            element.border_t_1().border_color(rgb(0x242833))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .child(div().text_sm().text_color(rgb(0xe5e7eb)).child(format!(
                                    "{} · {}",
                                    worker_node_display_name(node),
                                    connection_label,
                                )))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x8f98a6))
                                        .child(format_worker_node_resources(resources)),
                                )
                                .when_some(node.connection_detail.as_deref(), |element, detail| {
                                    element.child(
                                        div()
                                            .mt_1()
                                            .text_xs()
                                            .text_color(
                                                if node.connection_state
                                                    == WorkerConnectionState::Failed
                                                {
                                                    rgb(0xfca5a5)
                                                } else {
                                                    rgb(0xfcd34d)
                                                },
                                            )
                                            .child(detail.to_owned()),
                                    )
                                }),
                        )
                        .when(!node.is_local, |element| {
                            element.child(
                                Button::new(format!("remove-worker-node-{id}"))
                                    .label("Remove")
                                    .small()
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.remove_worker_node(id, cx)
                                    })),
                            )
                        })
                        .when(
                            !node.is_local && self.node_backends.contains_key(&node_id),
                            |element| {
                                element.child(
                                    Button::new(format!("worker-node-providers-{id}"))
                                        .label("Providers")
                                        .small()
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.open_providers_for_node(node_id.clone(), cx)
                                        })),
                                )
                            },
                        )
                }));

                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_section_heading("WORKERS"))
                    .when_some(self.browser_startup_error.as_deref(), |element, error| {
                        element.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xfca5a5))
                                .child(error.to_owned()),
                        )
                    })
                    .child(card)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div().flex_1().child(
                                    KitInput::new(self.node_input_state.as_ref().expect(
                                        "worker connection input initialized before rendering",
                                    ))
                                    .id("worker-node-connection-input")
                                    .small(),
                                ),
                            )
                            .child(
                                Button::new("connect-worker-node")
                                    .label("Connect")
                                    .small()
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.connect_worker_node(cx)),
                                    ),
                            ),
                    )
                    .child(div().text_xs().text_color(rgb(0x64748b)).child(
                        "Use: ws://host:port/ws token · URLs are shared; access tokens are not",
                    ))
                    .into_any_element()
            }
            SettingsSection::Appearance => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("APPEARANCE"))
                .child(
                    settings_card()
                        .child(settings_row(
                            "Theme",
                            "Follow the system or choose a palette",
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .p_1()
                                .rounded_md()
                                .bg(rgb(0x20242c))
                                .children(ThemeChoice::ALL.into_iter().enumerate().map(
                                    |(index, choice)| {
                                        let selected = choice == self.theme_choice;
                                        div()
                                            .id(("theme-choice", index))
                                            .px_3()
                                            .py_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .bg(if selected {
                                                rgb(0x263b58)
                                            } else {
                                                rgb(0x20242c)
                                            })
                                            .text_xs()
                                            .text_color(if selected {
                                                rgb(0xe5e7eb)
                                            } else {
                                                rgb(0xb7c0d0)
                                            })
                                            .child(choice.label())
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.select_theme(choice, window, cx);
                                            }))
                                    },
                                )),
                            true,
                        ))
                        .child(settings_row(
                            "Font size",
                            "Relative to the system display scale",
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Button::new("font-scale-decrease")
                                        .label("−")
                                        .small()
                                        .disabled(self.font_scale_percent <= MIN_FONT_SCALE_PERCENT)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.adjust_font_scale(
                                                -FONT_SCALE_STEP_PERCENT,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    div()
                                        .w(px(52.))
                                        .text_center()
                                        .text_sm()
                                        .child(format!("{}%", self.font_scale_percent)),
                                )
                                .child(
                                    Button::new("font-scale-increase")
                                        .label("+")
                                        .small()
                                        .disabled(self.font_scale_percent >= MAX_FONT_SCALE_PERCENT)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.adjust_font_scale(
                                                FONT_SCALE_STEP_PERCENT,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    Button::new("font-scale-reset")
                                        .label("Reset")
                                        .ghost()
                                        .small()
                                        .disabled(
                                            self.font_scale_percent == DEFAULT_FONT_SCALE_PERCENT,
                                        )
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.set_font_scale_percent(
                                                DEFAULT_FONT_SCALE_PERCENT,
                                                window,
                                                cx,
                                            );
                                        })),
                                ),
                            false,
                        )),
                )
                .into_any_element(),
        };

        div()
            .id("settings-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(rgb(0x242833))
                    .child(div().text_sm().text_color(rgb(0xf3f4f6)).child("Settings"))
                    .child(
                        div()
                            .id("close-settings")
                            .test_support()
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close settings".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_settings)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(settings_nav(section, cx))
                    .child(
                        div()
                            .id("settings-content")
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .p_6()
                            .overflow_y_scroll()
                            .child(content),
                    ),
            )
            .into_any()
    }

    pub(crate) fn render_about_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("about-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child("About Loom"),
                    )
                    .child(
                        div()
                            .id("close-about")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close about".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_about)),
                    ),
            )
            .child(
                div()
                    .mt_8()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(div().text_lg().text_color(rgb(0xf3f4f6)).child("Loom"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("Local agent"),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(0x64748b))
                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    ),
            )
            .into_any()
    }

    pub(crate) fn render_providers_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let node_name = self
            .providers_node_id
            .as_ref()
            .and_then(|node_id| self.node_names.get(node_id))
            .map_or("worker", String::as_str);
        let github_provider = self
            .providers
            .iter()
            .find(|provider| provider.kind == ProviderKind::GitHubCopilot);
        let local_providers = self
            .providers
            .iter()
            .filter(|provider| provider.kind != ProviderKind::GitHubCopilot)
            .collect::<Vec<_>>();
        let github_status = if self.github_connected {
            "Connected"
        } else {
            "Not connected"
        };
        let github_models = github_provider.map_or(0, |provider| provider.models.len());

        let mut local_body = div().flex().flex_col().gap_1();
        if local_providers.is_empty() {
            local_body = local_body.child(
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x171c25))
                    .border_1()
                    .border_color(rgb(0x293244))
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("No other providers are configured."),
            );
        } else {
            for provider in local_providers {
                let api_key_configurable = provider.api_key_configurable;
                let provider_id = provider.id.clone();
                let mut provider_card = div()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x171c25))
                    .border_1()
                    .border_color(rgb(0x293244))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child(provider.display_name.clone()),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child(format!(
                                "{} model{} available",
                                provider.models.len(),
                                if provider.models.len() == 1 { "" } else { "s" }
                            )),
                    )
                    .child(div().mt_1().text_xs().text_color(rgb(0x8f98a6)).child(
                        if provider.credential_id.is_some() {
                            "API key configured".to_owned()
                        } else {
                            "API key not configured".to_owned()
                        },
                    ));
                if api_key_configurable {
                    if let Some(input) = self.provider_api_key_inputs.get(&provider.id) {
                        provider_card = provider_card.child(
                            div().mt_2().child(
                                KitInput::new(input)
                                    .id(format!("provider-api-key-input-{}", provider.id.as_str()))
                                    .small(),
                            ),
                        );
                    }
                    if let Some(status) = self.provider_setup_status.get(&provider.id) {
                        provider_card = provider_card.child(
                            div()
                                .id(format!("provider-setup-status-{}", provider.id.as_str()))
                                .mt_2()
                                .text_xs()
                                .text_color(rgb(0x8f98a6))
                                .child(status.clone()),
                        );
                    }
                    provider_card = provider_card.child(
                        div().mt_2().child(
                            Button::new(format!("configure-api-key-{}", provider.id.as_str()))
                                .label("Save API key")
                                .small()
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.configure_api_key_provider(
                                        provider_id.clone(),
                                        window,
                                        cx,
                                    );
                                })),
                        ),
                    );
                    if provider.credential_id.is_some() {
                        let node_id = self
                            .providers_node_id
                            .clone()
                            .unwrap_or_else(|| self.default_backend_node_id.clone());
                        provider_card = provider_card.child(
                            div()
                                .id(format!("refresh-provider-models-{}", provider.id.as_str()))
                                .mt_1()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x242833))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_sm()
                                .cursor_pointer()
                                .child("Refresh models")
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.refresh_models_for_node_async(node_id.clone(), cx);
                                })),
                        );
                    }
                }
                local_body = local_body.child(provider_card);
            }
        }

        let github_card =
            div()
                .p_3()
                .rounded_lg()
                .bg(rgb(0x171c25))
                .border_1()
                .border_color(rgb(0x293244))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(0xf3f4f6))
                                .child("GitHub Copilot"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(if self.github_connected {
                                    rgb(0x9ad7bd)
                                } else {
                                    rgb(0xfef3c7)
                                })
                                .child(github_status),
                        ),
                )
                .child(div().mt_1().text_xs().text_color(rgb(0x8f98a6)).child(
                    if github_models == 0 {
                        "Connect your GitHub account to use Copilot models.".to_owned()
                    } else {
                        format!(
                            "{} model{} available",
                            github_models,
                            if github_models == 1 { "" } else { "s" }
                        )
                    },
                ))
                .child(
                    div()
                        .mt_2()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("Manage GitHub authentication in Settings. Connecting GitHub also adds this model provider."),
                );

        div()
            .id("providers-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .child(div().text_sm().text_color(rgb(0xf3f4f6)).child("Providers"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child(format!("Configured on {node_name}")),
                            ),
                    )
                    .child(
                        div()
                            .id("close-providers")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close providers".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_providers)),
                    ),
            )
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("GITHUB COPILOT"),
            )
            .child(div().mt_2().child(github_card))
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("LOCAL PROVIDER"),
            )
            .child(div().mt_2().child(local_body))
            .into_any()
    }
}
