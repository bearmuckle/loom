use super::*;

impl LoomView {
    pub(crate) fn render_rename_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dialog) = &self.rename_dialog else {
            return div().into_any();
        };
        div()
            .id("rename-dialog")
            .occlude()
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

    pub(crate) fn render_source_dialog(
        &self,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(dialog) = &self.source_dialog else {
            return div().into_any();
        };
        let phone = layout.phone;
        let is_start = dialog.purpose == SessionSourceDialogPurpose::StartSession;
        let selected_is_cached = dialog.choice == SessionSourceChoice::GitHub
            && dialog.selected_repository.as_deref().is_some_and(|name| {
                self.cloned_repositories
                    .iter()
                    .any(|repository| repository.full_name == name)
            });
        let selected_repo = dialog.selected_repository.as_deref();
        let repository_query = self
            .repository_filter_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let repository_query = repository_query.trim().to_owned();
        let query_ready = repository_query.chars().count() >= GITHUB_REPOSITORY_QUERY_MIN_CHARS;
        let cloned_names = self
            .cloned_repositories
            .iter()
            .map(|repository| repository.full_name.as_str())
            .collect::<Vec<_>>();
        // The "Run on" picker lists connected workers with the home worker
        // first. Selecting one scopes repository discovery and creation.
        let mut worker_choices = self
            .worker_nodes
            .iter()
            .map(|node| (node.status.node_id.clone(), worker_node_display_name(node)))
            .collect::<Vec<_>>();
        worker_choices = order_session_nodes(worker_choices, &self.default_backend_node_id);
        let worker_online = self
            .worker_nodes
            .iter()
            .map(|node| {
                (
                    node.status.node_id.clone(),
                    matches!(node.connection_state, WorkerConnectionState::Connected)
                        && node.status.online,
                )
            })
            .collect::<BTreeMap<_, _>>();

        let mut cloned_rows = div().flex().flex_col().gap_1();
        for repository in &self.cloned_repositories {
            let name = repository.full_name.clone();
            let selected = selected_repo == Some(name.as_str());
            let subtitle = cloned_repository_subtitle(repository);
            cloned_rows = cloned_rows.child(
                div()
                    .id(format!("cloned-repository-{name}"))
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
                        rgb(0x1f6b4d)
                    })
                    .cursor_pointer()
                    .child(div().text_sm().child(name.clone()))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(subtitle))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(dialog) = &mut this.source_dialog {
                            dialog.selected_repository = Some(name.clone());
                        }
                        cx.notify();
                    })),
            );
        }

        let mut repository_rows = div().flex().flex_col().gap_1();
        for repository in &dialog.repositories {
            let name = repository.full_name.clone();
            let selected = selected_repo == Some(name.as_str());
            let already_cloned = cloned_names.contains(&name.as_str());
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
                    } else if already_cloned {
                        rgb(0x1f6b4d)
                    } else {
                        rgb(0x293244)
                    })
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_sm().child(name.clone()))
                            .when(already_cloned, |row| {
                                row.child(
                                    div()
                                        .rounded_sm()
                                        .px_2()
                                        .bg(rgb(0x10291f))
                                        .border_1()
                                        .border_color(rgb(0x1f6b4d))
                                        .text_xs()
                                        .text_color(rgb(0x34d399))
                                        .child("cloned here"),
                                )
                            }),
                    )
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

        let mut dialog_body = div();
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
                )
                .when_some(self.local_current_directory.as_ref(), |body, directory| {
                    body.child(
                        div()
                            .mt_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child(format!("Current folder: {}", directory.display())),
                            )
                            .child(
                                Button::new("use-current-session-directory")
                                    .label("Use current folder")
                                    .small()
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.use_current_source_directory(cx);
                                    })),
                            ),
                    )
                });
        } else if dialog.choice == SessionSourceChoice::GitHub {
            // A column: the heading row, cloned repositories, search field and
            // results stack vertically instead of sharing one row.
            let mut github_body = div().w_full().flex().flex_col().child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x7f8b9c))
                            .child("ON THIS NODE"),
                    )
                    .when(!self.cloned_repositories.is_empty(), |row| {
                        row.child(
                            div()
                                .rounded_sm()
                                .px_2()
                                .bg(rgb(0x10291f))
                                .border_1()
                                .border_color(rgb(0x1f6b4d))
                                .text_xs()
                                .text_color(rgb(0x34d399))
                                .child(format!("{} clones", self.cloned_repositories.len())),
                        )
                    }),
            );
            github_body = if self.cloned_repositories_loading && self.cloned_repositories.is_empty()
            {
                github_body.child(
                    div()
                        .mt_2()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child("Loading cached repositories…"),
                )
            } else if self.cloned_repositories.is_empty() {
                github_body.child(div().mt_2().text_xs().text_color(rgb(0x8f98a6)).child(
                    "No repositories are cloned on this worker yet. Search below to add one.",
                ))
            } else {
                github_body.child(div().mt_2().child(cloned_rows))
            };
            github_body = github_body.child(
                div()
                    .mt_3()
                    .mb_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(rgb(0x5c6572))
                    .child(div().flex_1().h(px(1.)).bg(rgb(0x232c39)))
                    .child("Find another repository")
                    .child(div().flex_1().h(px(1.)).bg(rgb(0x232c39))),
            );
            github_body = github_body.child(
                KitInput::new(
                    self.repository_filter_input
                        .as_ref()
                        .expect("repository filter initialized before rendering"),
                )
                .id("github-repository-filter")
                .small()
                .into_any_element(),
            );
            github_body = if !query_ready {
                github_body.child(
                    div()
                        .mt_1()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(format!(
                            "Type at least {GITHUB_REPOSITORY_QUERY_MIN_CHARS} characters to search GitHub."
                        )),
                )
            } else if dialog.repositories_loading {
                github_body.child(
                    div()
                        .mt_2()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child("Searching GitHub…"),
                )
            } else if let Some(error) = &dialog.error {
                github_body
                    .child(
                        div()
                            .mt_2()
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
                github_body.child(
                    div()
                        .mt_2()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child(format!("No repositories match \"{repository_query}\".")),
                )
            } else {
                github_body.child(
                    div()
                        .mt_2()
                        .max_h(px(240.))
                        .overflow_y_scrollbar()
                        .child(repository_rows),
                )
            };
            dialog_body = github_body;
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

        let mut source_choices = ButtonGroup::new("session-source-choices")
            .outline()
            .small()
            .when(phone, |group| {
                group.w_full().layout(gpui_kit::Axis::Vertical)
            });
        if is_start {
            source_choices = source_choices.child(
                Button::new("source-empty")
                    .label("Empty project")
                    .small()
                    .when(phone, |button| button.w_full())
                    .selected(dialog.choice == SessionSourceChoice::Empty)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choose_source(SessionSourceChoice::Empty, cx)
                    })),
            );
        }
        if dialog.local_directory_available {
            source_choices = source_choices.child(
                Button::new("source-local-directory")
                    .label("Local folder")
                    .small()
                    .when(phone, |button| button.w_full())
                    .selected(dialog.choice == SessionSourceChoice::LocalDirectory)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choose_source(SessionSourceChoice::LocalDirectory, cx)
                    })),
            );
        }
        source_choices = source_choices.child(
            Button::new("source-github")
                .label("GitHub repository")
                .small()
                .when(phone, |button| button.w_full())
                .selected(dialog.choice == SessionSourceChoice::GitHub)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.choose_source(SessionSourceChoice::GitHub, cx)
                })),
        );

        let title = if is_start {
            "New project"
        } else {
            "Add to this session"
        };
        let confirm_label = if is_start {
            if selected_is_cached {
                "Start from existing clone"
            } else {
                "Create project"
            }
        } else if selected_is_cached {
            "Add existing clone"
        } else {
            "Add to session"
        };
        let mut run_on = div().w_full().flex().flex_col().gap_1();
        if is_start && worker_choices.len() > 1 {
            run_on = run_on.child(div().text_xs().text_color(rgb(0x8f98a6)).child("Run on"));
            let mut row = div().flex().flex_wrap().items_center().gap_1();
            for (node_id, label) in &worker_choices {
                let selected = *node_id == dialog.target_node_id;
                let online = worker_online.get(node_id).copied().unwrap_or(false);
                let click_node = node_id.clone();
                row = row.child(
                    Button::new(format!("source-worker-node-{node_id}"))
                        .label(label.clone())
                        .small()
                        .selected(selected)
                        .disabled(!online)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.choose_source_node(click_node.clone(), cx);
                        })),
                );
            }
            run_on = run_on.child(row);
        }

        let hint = div()
            .w_full()
            .text_xs()
            .text_color(rgb(0x8f98a6))
            .child(if is_start {
                "Choose what the new project starts with."
            } else {
                "Choose a repository or folder to add to the active session."
            });
        let mut form = div().w_full().flex().flex_col().gap_3();
        if is_start && worker_choices.len() > 1 {
            form = form.child(run_on);
        }
        let form = form.child(hint).child(source_choices).child(dialog_body);

        // The fixed-width modal clipped its contents on phones and wasted space
        // on desktop. Use a full-height pane in the central area (like
        // Settings) at every width, with the actions pinned to the bottom.
        div()
            .id("source-dialog-pane")
            .test_support()
            .occlude()
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
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(0x242833))
                    .child(div().text_sm().text_color(rgb(0xf3f4f6)).child(title))
                    .child(
                        div()
                            .id("close-source-dialog")
                            .test_support()
                            .w(px(if phone { 44. } else { 30. }))
                            .h(px(if phone { 44. } else { 30. }))
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
                                    text: "Close new project".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.source_dialog = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("source-dialog-content")
                    .test_support()
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .when(phone, |element| element.p_4())
                    .when(!phone, |element| element.px_6().py_5())
                    .overflow_y_scroll()
                    // Keep the form readable when the pane is much wider than
                    // a phone instead of stretching controls edge to edge.
                    .child(
                        div()
                            .id("source-dialog-form")
                            .test_support()
                            .w_full()
                            .max_w(px(640.))
                            .child(form),
                    ),
            )
            .child(
                // Keep the actions under the form instead of pinning them to
                // the window's lower-right corner: the footer shares the same
                // centered max-width container as the body on desktop.
                div()
                    .flex()
                    .justify_center()
                    .border_t_1()
                    .border_color(rgb(0x242833))
                    .when(phone, |element| element.p_3())
                    .when(!phone, |element| element.px_6().py_3())
                    .child(
                        div()
                            .id("source-dialog-actions")
                            .test_support()
                            .flex()
                            .w_full()
                            .max_w(px(640.))
                            .gap_2()
                            .when(!phone, |element| element.justify_end())
                            .child(
                                div().when(phone, |element| element.flex_1()).child(
                                    Button::new("cancel-session-source")
                                        .label("Cancel")
                                        .when(phone, |button| button.w_full().h(px(44.)))
                                        .when(!phone, |button| button.small())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.source_dialog = None;
                                            cx.notify();
                                        })),
                                ),
                            )
                            .child(
                                div().when(phone, |element| element.flex_1()).child(
                                    Button::new("confirm-session-source")
                                        .label(confirm_label)
                                        .primary()
                                        .when(phone, |button| button.w_full().h(px(44.)))
                                        .when(!phone, |button| button.small())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_source_dialog(cx);
                                            cx.notify();
                                        })),
                                ),
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
            .occlude()
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

    pub(crate) fn render_settings_dialog(
        &self,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let phone = layout.phone;
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
                            phone,
                        ))
                        .child(settings_row(
                            "Default model for new sessions",
                            "Used when a session has no model of its own",
                            if phone {
                                div().w_full()
                            } else {
                                div().w(px(320.))
                            }
                            .child(default_model_body),
                            false,
                            phone,
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
                            phone,
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
                            phone,
                        )),
                )
                .into_any_element(),
            SettingsSection::Providers => {
                let node_name = self
                    .providers_node_id
                    .as_ref()
                    .and_then(|node_id| self.node_names.get(node_id))
                    .map_or("this worker", String::as_str);
                div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("PROVIDERS"))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(format!("Configured on {node_name}")),
                )
                .child(
                    settings_card().child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
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
                .child(
                    settings_card().child(settings_row(
                        "GitHub repository access",
                        "Authorize clone, push, and pull requests through the GitHub CLI app on the worker",
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if self.github_repository_connected {
                                        rgb(0x9ad7bd)
                                    } else {
                                        rgb(0xfef3c7)
                                    })
                                    .child(if self.github_repository_connected {
                                        "Authorized"
                                    } else {
                                        "Not authorized"
                                    }),
                            )
                            .when(
                                !self.github_repository_connected && self.login_enabled,
                                |element| {
                                    element.child(
                                        Button::new("connect-github-repository")
                                            .label("Authorize")
                                            .small()
                                            .on_click(
                                                cx.listener(Self::authorize_github_repository),
                                            ),
                                    )
                                },
                            ),
                        true,
                        phone,
                    )),
                )
                .child(
                    settings_card().child(settings_row(
                        "Allow GitHub writes and pull requests",
                        "Let the agent push branches and open pull requests with the connected account",
                        Switch::new("github-write-access-toggle")
                            .checked(self.github_write_access)
                            .disabled(!self.github_repository_connected)
                            .accessibility_label("Allow GitHub writes and pull requests")
                            .on_change({
                                let view = cx.entity();
                                move |checked, _window, cx| {
                                    let enabled = *checked;
                                    view.update(cx, |view, cx| {
                                        view.toggle_github_write_access(enabled, cx);
                                    });
                                }
                            }),
                        true,
                        phone,
                    )),
                )
                .child(self.render_api_key_providers(cx))
                .into_any_element()
            }
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
                        .flex_wrap()
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
                                .flex_wrap()
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
                            phone,
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
                                        .label("-")
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
                            phone,
                        ))
                        .child(settings_row(
                            "Show model reasoning",
                            "Reveal the model's thinking above each answer",
                            Switch::new("show-reasoning-toggle")
                                .checked(self.show_reasoning)
                                .accessibility_label("Show model reasoning")
                                .on_change({
                                    let view = cx.entity();
                                    move |_checked, _window, cx| {
                                        view.update(cx, |view, cx| {
                                            view.toggle_show_reasoning(cx);
                                        });
                                    }
                                }),
                            false,
                            phone,
                        )),
                )
                .into_any_element(),
            SettingsSection::About => {
                let subtitle = about_backend_label(
                    self.demo_workspace,
                    self.browser_client,
                    self.backend_endpoint.as_deref(),
                );
                let version = about_version_label(about_build_version(), about_git_revision());
                let platform = about_platform_label(
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    self.browser_client,
                );
                let protocol =
                    about_protocol(CURRENT_PROTOCOL_VERSION, self.server_protocol_version);
                let protocol_color = if protocol.mismatch {
                    rgb(0xfcd34d)
                } else {
                    rgb(0xb7c0d0)
                };

                let mut links_card = settings_card();
                for (index, link) in about_links().into_iter().enumerate() {
                    let url = link.url();
                    let open_url = url.clone();
                    let label = link.label;
                    links_card = links_card.child(settings_row(
                        label,
                        url.as_str(),
                        Button::new(link.id)
                            .label("Open")
                            .ghost()
                            .small()
                            .accessibility_label(format!("Open {label}"))
                            .tooltip(format!("Open {label}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Err(error) = open_external_url(&open_url) {
                                    this.record_status(format!(
                                        "Could not open {label} ({open_url}): {error}. The URL is shown above and can be copied."
                                    ));
                                }
                                cx.notify();
                            })),
                        index == 0,
                        phone,
                    ));
                }

                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_section_heading("ABOUT"))
                    .child(
                        settings_card().child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_1()
                                .px_4()
                                .py_6()
                                .child(div().text_lg().text_color(rgb(0xf3f4f6)).child("Loom"))
                                .child(
                                    div()
                                        .id("about-subtitle")
                                        .test_support()
                                        .text_sm()
                                        .text_color(rgb(0x8f98a6))
                                        .child(subtitle),
                                ),
                        ),
                    )
                    .child(settings_section_heading("THIS BUILD"))
                    .child(
                        settings_card()
                            .child(settings_row(
                                "Version",
                                "",
                                about_value("about-version", version, rgb(0xb7c0d0)),
                                true,
                                phone,
                            ))
                            .child(settings_row(
                                "Platform",
                                "",
                                about_value("about-platform", platform, rgb(0xb7c0d0)),
                                false,
                                phone,
                            ))
                            .child(settings_row(
                                "Protocol",
                                "",
                                about_value("about-protocol", protocol.text, protocol_color),
                                false,
                                phone,
                            )),
                    )
                    .child(settings_section_heading("LINKS"))
                    .child(links_card)
                    .into_any_element()
            }
        };

        div()
            .id("settings-dialog")
            .occlude()
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
                    .px_4()
                    .when(!phone, |element| element.px_6().py_4())
                    .when(phone, |element| element.py_3())
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
                    .when(phone, |element| element.flex_col())
                    .child(settings_nav(section, phone, cx))
                    .child(
                        div()
                            .id("settings-content")
                            .test_support()
                            .flex_1()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .w_full()
                            .h_full()
                            .p_4()
                            .when(!phone, |element| element.p_6())
                            .overflow_y_scroll()
                            .child(content),
                    ),
            )
            .into_any()
    }

    fn render_api_key_providers(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let local_providers = self
            .providers
            .iter()
            .filter(|provider| provider.kind != ProviderKind::GitHubCopilot)
            .collect::<Vec<_>>();
        let mut body = div().w_full().flex().flex_col().gap_2();
        if local_providers.is_empty() {
            body = body.child(
                settings_card().child(
                    div()
                        .px_4()
                        .py_3()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("No other providers are configured."),
                ),
            );
        } else {
            for provider in local_providers {
                let api_key_configurable = provider.api_key_configurable;
                let configured = provider.credential_id.is_some();
                let provider_id = provider.id.clone();
                let mut card = settings_card().child(
                    div()
                        .px_4()
                        .py_3()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(0xf3f4f6))
                                .child(provider.display_name.clone()),
                        )
                        // Keyless API providers advertise a seeded default model,
                        // so only claim models once the provider is usable.
                        .when(configured || !api_key_configurable, |element| {
                            element.child(div().text_xs().text_color(rgb(0x8f98a6)).child(format!(
                                "{} model{} available",
                                provider.models.len(),
                                if provider.models.len() == 1 { "" } else { "s" }
                            )))
                        })
                        .when(api_key_configurable, |element| {
                            element.child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                                if configured {
                                    "API key configured".to_owned()
                                } else {
                                    "API key not configured".to_owned()
                                },
                            ))
                        }),
                );
                if api_key_configurable {
                    if let Some(input) = self.provider_api_key_inputs.get(&provider.id) {
                        card = card.child(
                            div().px_4().pb_3().child(
                                KitInput::new(input)
                                    .id(format!("provider-api-key-input-{}", provider.id.as_str()))
                                    .small(),
                            ),
                        );
                    }
                    if let Some(status) = self.provider_setup_status.get(&provider.id) {
                        card = card.child(
                            div()
                                .id(format!("provider-setup-status-{}", provider.id.as_str()))
                                .px_4()
                                .pb_2()
                                .text_xs()
                                .text_color(rgb(0x8f98a6))
                                .child(status.clone()),
                        );
                    }
                    card = card.child(
                        div().px_4().pb_3().child(
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
                        card = card.child(
                            div()
                                .id(format!("refresh-provider-models-{}", provider.id.as_str()))
                                .mx_4()
                                .mb_3()
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
                body = body.child(card);
            }
        }
        body.into_any_element()
    }
}

/// Subtitle for a repository already cloned on the worker node, e.g.
/// `main · cloned 3m ago`.
fn cloned_repository_subtitle(repository: &ClonedRepository) -> String {
    let mut parts = Vec::new();
    if let Some(branch) = &repository.branch {
        parts.push(branch.clone());
    }
    let now = Timestamp::now().as_unix_millis();
    parts.push(format!(
        "cloned {}",
        relative_time(repository.last_used_at.as_unix_millis(), now)
    ));
    parts.join(" · ")
}
