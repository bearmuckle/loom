use super::*;

impl LoomView {
    pub(crate) fn render_review(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let layout = responsive_layout(window.bounds().size.width);
        let title = if self.project_child_review.is_some() {
            "Project child review"
        } else {
            "Changes"
        };
        let mut body = div()
            .when(!layout.phone, |element| {
                element
                    .w(px(220.))
                    .h_full()
                    .border_r_1()
                    .border_color(rgb(0x30343f))
            })
            .when(layout.phone, |element| {
                element
                    .h(px(170.))
                    .w_full()
                    .border_b_1()
                    .border_color(rgb(0x30343f))
            })
            .id("changes-sidebar-scroll")
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_2();
        match self.review.panel {
            ReviewPanel::Changes => {
                if self.project_child_review.is_none() && self.session_repositories.len() > 1 {
                    body = body.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x93c5fd))
                            .child("REPOSITORIES"),
                    );
                    for (index, repository) in self.session_repositories.iter().enumerate() {
                        let selected = self.selected_repository_id == Some(repository.id);
                        let repository_id = repository.id;
                        let name = repository
                            .source
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .filter(|name| !name.is_empty())
                            .unwrap_or("Repository");
                        body = body.child(
                            div()
                                .id(("review-repository", index))
                                .p_1()
                                .cursor_pointer()
                                .when(selected, |element| element.bg(rgb(0x293244)))
                                .text_sm()
                                .child(name.to_owned())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_session_repository(repository_id, cx);
                                })),
                        );
                    }
                }
                if let Some(status) = &self.review.vcs {
                    body = body.child(div().mt_2().text_xs().text_color(rgb(0x93c5fd)).child(
                        if self.project_child_review.is_some() {
                            "CHILD WORKTREE CHANGES"
                        } else {
                            "REPOSITORY CHANGES"
                        },
                    ));
                    for (index, file) in status.files.iter().enumerate() {
                        let path = file.path.clone();
                        let project_child_review = self.project_child_review.is_some();
                        let staged = matches!(
                            file.worktree,
                            GitFileStatusKind::Unknown | GitFileStatusKind::Ignored
                        );
                        let selected = self.review.selected_path.as_deref() == Some(&file.path)
                            && self.review.selected_staged == staged;
                        let (additions, deletions) = if staged {
                            (file.index_additions, file.index_deletions)
                        } else {
                            (file.worktree_additions, file.worktree_deletions)
                        };
                        body = body.child(
                            div()
                                .id(("git-file", index))
                                .p_1()
                                .when(selected, |element| element.bg(rgb(0x293244)))
                                .text_sm()
                                .text_color(rgb(0xfef3c7))
                                .cursor_pointer()
                                .child(format!(
                                    "{:?}  {}  +{} −{}",
                                    if staged { file.index } else { file.worktree },
                                    file.path,
                                    additions,
                                    deletions
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !project_child_review {
                                        this.open_review_diff(path.clone(), staged, cx);
                                    }
                                })),
                        );
                        if !staged && file.index != GitFileStatusKind::Unknown {
                            let path = file.path.clone();
                            let project_child_review = self.project_child_review.is_some();
                            body = body.child(
                                div()
                                    .id(("git-staged-file", index))
                                    .p_1()
                                    .pl_3()
                                    .text_xs()
                                    .text_color(rgb(0x93c5fd))
                                    .cursor_pointer()
                                    .child(format!(
                                        "Staged  +{} −{}",
                                        file.index_additions, file.index_deletions
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if !project_child_review {
                                            this.open_review_diff(path.clone(), true, cx);
                                        }
                                    })),
                            );
                        }
                    }
                }
                let workspace_changes = if self.project_child_review.is_some() {
                    Vec::new()
                } else {
                    self.review
                        .changes
                        .iter()
                        .enumerate()
                        .filter(|(_, change)| {
                            self.review.repositories_loaded
                                && !belongs_to_repository(&change.path, &self.session_repositories)
                        })
                        .collect::<Vec<_>>()
                };
                if !workspace_changes.is_empty() {
                    body = body.child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(0x93c5fd))
                            .child("OTHER WORKSPACE FILES"),
                    );
                }
                for (index, change) in workspace_changes {
                    let path = change.path.clone();
                    body = body.child(
                        div()
                            .id(("review-file", index))
                            .text_sm()
                            .text_color(change_color(change.kind))
                            .cursor_pointer()
                            .child(format!(
                                "{}  {}",
                                change_kind_label(change.kind),
                                change.path
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_review_file(path.clone(), cx);
                                cx.notify();
                            })),
                    );
                }
                if self.project_child_review.is_none() && !self.review.repositories_loaded {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("Loading repositories…"),
                    );
                } else if self.review.changes.is_empty()
                    && self
                        .review
                        .vcs
                        .as_ref()
                        .is_none_or(|status| status.files.is_empty())
                {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No changed files"),
                    );
                }
            }
        }
        let parent = cx.entity();
        let diff_list = list(self.review.list_state.clone(), move |index, _window, cx| {
            let view = parent.read(cx);
            view.render_review_row(index).into_any()
        })
        .size_full();
        let mut detail = div().flex_1().min_w(px(0.)).flex().flex_col();
        if let Some(path) = &self.review.selected_path {
            detail = detail.child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child(path.clone()))
                            .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                                if self.project_child_review.is_some() {
                                    "Project child checkout · read only"
                                } else if self.review.selected_file.is_some() {
                                    "Current file · no repository diff"
                                } else if self.review.selected_staged {
                                    "Staged changes · read only"
                                } else {
                                    "Working changes · read only"
                                },
                            )),
                    )
                    .when(!self.review.hunk_rows.is_empty(), |header| {
                        header.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Button::new("previous-review-hunk")
                                        .label("Previous hunk")
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.jump_review_hunk(false, cx)
                                        })),
                                )
                                .child(
                                    Button::new("next-review-hunk")
                                        .label("Next hunk")
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.jump_review_hunk(true, cx)
                                        })),
                                ),
                        )
                    }),
            );
        }
        if self.review.loading_diff {
            detail = detail.child(div().p_3().text_sm().child("Loading diff…"));
        } else if let Some(error) = &self.review.diff_error {
            detail = detail.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        } else if let Some(diff) = &self.review.selected_diff {
            if diff.binary {
                detail = detail.child(
                    div()
                        .p_3()
                        .text_sm()
                        .child("Binary file: no text diff is available."),
                );
            } else if self.review.rows.is_empty() {
                detail = detail.child(div().p_3().text_sm().child(if diff.truncated {
                    "The first changed line exceeds the review size limit."
                } else {
                    "No line changes in this version of the file."
                }));
            } else {
                detail = detail.child(diff_list);
            }
            if diff.truncated {
                detail =
                    detail.child(
                        div().p_2().text_xs().text_color(rgb(0xfef3c7)).child(
                            "Diff exceeds the review size limit; showing the beginning only.",
                        ),
                    );
            }
        } else if let Some(file) = &self.review.selected_file {
            detail = detail.child(
                div()
                    .flex_1()
                    .id("review-file-scroll")
                    .overflow_y_scroll()
                    .p_3()
                    .child(SelectableText::new(
                        "review-file-content",
                        file.content.clone(),
                    )),
            );
        } else {
            detail = detail.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("Select a changed file to review its diff."),
            );
        }
        let content = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .when(layout.phone, |element| element.flex_col())
            .child(body)
            .child(detail);
        div()
            .when(layout.phone, |element| {
                element.size_full().absolute().top(px(0.)).left(px(0.))
            })
            .when(!layout.phone, |element| element.size_full())
            .flex()
            .flex_col()
            .bg(rgb(0x17191f))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(0x293244))
                    .child(div().text_xs().text_color(rgb(0x93c5fd)).child(title))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(!layout.phone, |element| {
                                element.child(
                                    div().text_xs().text_color(rgb(0x8f98a6)).child(
                                        self.review
                                            .vcs
                                            .as_ref()
                                            .map(|status| {
                                                format!(
                                                    "{}  {}",
                                                    status.branch.as_deref().unwrap_or("detached"),
                                                    if status.clean { "clean" } else { "modified" }
                                                )
                                            })
                                            .unwrap_or_else(|| "VCS unavailable".to_owned()),
                                    ),
                                )
                            })
                            .when(!layout.phone, |element| {
                                element.child(
                                    Button::new("close-review")
                                        .icon(Icon::new(IconName::FileText))
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Show changes")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.review.panel = ReviewPanel::Changes;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new("toggle-review-sidebar-close")
                                    .label("Close")
                                    .ghost()
                                    .small()
                                    .tooltip("Close review panel")
                                    .on_click(cx.listener(Self::close_review)),
                            ),
                    ),
            )
            .child(content)
    }

    pub(crate) fn render_review_row(&self, index: usize) -> gpui_kit::Div {
        let Some(row) = self.review.rows.get(index) else {
            return div().w_full().min_h(px(22.));
        };
        match row {
            ReviewRow::Hunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
            } => div()
                .w_full()
                .px_2()
                .py_1()
                .bg(rgb(0x293244))
                .font_family(mono_font())
                .text_xs()
                .text_color(rgb(0x93c5fd))
                .child(format!(
                    "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@"
                )),
            ReviewRow::Line(line) => {
                let (marker, background, foreground) = match line.kind {
                    GitDiffLineKind::Added => ("+", 0x24543d, 0xbbf7d0),
                    GitDiffLineKind::Removed => ("−", 0x542936, 0xfecaca),
                    GitDiffLineKind::Context => (" ", 0x17191f, 0xcbd5e1),
                };
                div()
                    .w_full()
                    .min_h(px(22.))
                    .flex()
                    .items_start()
                    .bg(rgb(background))
                    .font_family(mono_font())
                    .text_xs()
                    .text_color(rgb(foreground))
                    .child(
                        div()
                            .w(px(40.))
                            .flex_shrink_0()
                            .text_color(rgb(0x8f98a6))
                            .child(line.old_line.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .w(px(40.))
                            .flex_shrink_0()
                            .text_color(rgb(0x8f98a6))
                            .child(line.new_line.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(div().w(px(18.)).flex_shrink_0().child(marker))
                    .child(div().flex_1().min_w(px(0.)).child(SelectableText::new(
                        ("review-line", index),
                        line.content.clone(),
                    )))
            }
        }
    }
}
