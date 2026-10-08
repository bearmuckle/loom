use super::*;

impl LoomView {
    pub(crate) fn render_inspector_changes(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let layout = responsive_layout(window.bounds().size.width);
        let mut body = div()
            .when(!layout.phone, |element| {
                element
                    .w(px(240.))
                    .h_full()
                    .border_r_1()
                    .border_color(rgb(0x30343f))
            })
            .when(layout.phone, |element| {
                element
                    .h(px(190.))
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

        let filter = self
            .review_filter_input
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_lowercase())
            .unwrap_or_default();
        let mut visible_repository_files = 0usize;
        if let Some(input) = self.review_filter_input.as_ref() {
            body = body.child(
                div()
                    .w_full()
                    .pb_1()
                    .child(KitInput::new(input).id("review-file-filter").small()),
            );
        }

        if self.project_child_review.is_none() && self.session_repositories.len() > 1 {
            body = body.child(section_heading("REPOSITORIES"));
            for (index, repository) in self.session_repositories.iter().enumerate() {
                let selected = self.selected_repository_id == Some(repository.id);
                let repository_id = repository.id;
                let name = repository_display_name(&repository.source);
                body = body.child(
                    div()
                        .id(("review-repository", index))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(selected, |element| element.bg(rgb(0x293244)))
                        .hover(|style| style.bg(rgb(0x20242c)))
                        .text_sm()
                        .text_color(if selected {
                            rgb(0xe5e7eb)
                        } else {
                            rgb(0xb7c0d0)
                        })
                        .child(name)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_session_repository(repository_id, cx);
                        })),
                );
            }
        }

        if let Some(status) = &self.review.vcs {
            let matching = status
                .files
                .iter()
                .filter(|file| filter.is_empty() || file.path.to_lowercase().contains(&filter))
                .collect::<Vec<_>>();
            visible_repository_files = matching.len();
            let (files, additions, deletions) = git_status_totals(status);
            body = body.child(
                div()
                    .mt_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(section_heading(if self.project_child_review.is_some() {
                        "CHILD WORKTREE CHANGES"
                    } else {
                        "REPOSITORY CHANGES"
                    }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_family(mono_font())
                            .text_xs()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div().text_color(rgb(0x8f98a6)).child(format!("{files}")),
                                    )
                                    .child(
                                        div()
                                            .text_color(rgb(0x86efac))
                                            .child(format!("+{additions}")),
                                    )
                                    .child(
                                        div()
                                            .text_color(rgb(0xfca5a5))
                                            .child(format!("-{deletions}")),
                                    ),
                            ),
                    ),
            );
            for (index, file) in matching.iter().enumerate() {
                let path = file.path.clone();
                let project_child_review = self.project_child_review.is_some();
                let staged = matches!(
                    file.worktree,
                    GitFileStatusKind::Unknown | GitFileStatusKind::Ignored
                );
                let selected = self.review.selected_path.as_deref() == Some(&file.path)
                    && self.review.selected_staged == staged;
                let kind = if staged { file.index } else { file.worktree };
                let (additions, deletions) = if staged {
                    (file.index_additions, file.index_deletions)
                } else {
                    (file.worktree_additions, file.worktree_deletions)
                };
                body = body.child(
                    div()
                        .id(("git-file", index))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .when(selected, |element| element.bg(rgb(0x293244)))
                        .hover(|style| style.bg(rgb(0x20242c)))
                        .child(
                            div()
                                .w(px(12.))
                                .flex_shrink_0()
                                .font_family(mono_font())
                                .text_sm()
                                .text_color(git_status_color(kind))
                                .child(git_status_glyph(kind)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_sm()
                                .text_color(rgb(0xe5e7eb))
                                .child(file.path.clone()),
                        )
                        .when(file.conflicted, |element| {
                            element.child(
                                div()
                                    .flex_shrink_0()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(rgb(0x542936))
                                    .text_xs()
                                    .text_color(rgb(0xfca5a5))
                                    .child("conflict"),
                            )
                        })
                        .when(additions > 0 || deletions > 0, |element| {
                            element.child(counts(additions, deletions))
                        })
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
                            .ml_5()
                            .mr_1()
                            .px_1()
                            .py_1()
                            .rounded_sm()
                            .flex()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x20242c)))
                            .child(div().text_xs().text_color(rgb(0x93c5fd)).child("staged"))
                            .child(div().flex_1())
                            .child(counts(file.index_additions, file.index_deletions))
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
                        && (filter.is_empty() || change.path.to_lowercase().contains(&filter))
                })
                .collect::<Vec<_>>()
        };
        let visible_workspace_files = workspace_changes.len();
        if !workspace_changes.is_empty() {
            body = body.child(div().mt_2().child(section_heading("OTHER WORKSPACE FILES")));
        }
        for (index, change) in workspace_changes {
            let path = change.path.clone();
            body = body.child(
                div()
                    .id(("review-file", index))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0x20242c)))
                    .child(
                        div()
                            .w(px(44.))
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(change_color(change.kind))
                            .child(change_kind_label(change.kind)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_sm()
                            .text_color(rgb(0xe5e7eb))
                            .child(change.path.clone()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_review_file(path.clone(), cx);
                        cx.notify();
                    })),
            );
        }

        if self.project_child_review.is_none() && !self.review.repositories_loaded {
            body = body.child(empty_note("Loading repositories…"));
        } else if self.review.changes.is_empty()
            && self
                .review
                .vcs
                .as_ref()
                .is_none_or(|status| status.files.is_empty())
        {
            body = body.child(empty_note("No changed files"));
        } else if visible_repository_files == 0 && visible_workspace_files == 0 {
            body = body.child(empty_note("No files match the filter"));
        }

        let parent_for_rows = cx.entity();
        let diff_list = list(self.review.list_state.clone(), move |index, _window, cx| {
            let view = parent_for_rows.read(cx);
            let is_hunk = matches!(view.review.rows.get(index), Some(ReviewRow::Hunk { .. }));
            let row = view.render_review_row(index);
            if is_hunk {
                let parent = parent_for_rows.clone();
                row.id(("review-hunk-toggle", index))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        parent.update(cx, |this, cx| this.toggle_review_hunk(index, cx));
                    })
                    .into_any()
            } else {
                row.into_any()
            }
        })
        .size_full();
        let mut detail = div().flex_1().min_w(px(0.)).flex().flex_col();
        if let Some(path) = &self.review.selected_path {
            detail = detail.child(self.render_changes_detail_header(path, cx));
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
                detail = detail.child(empty_note("Binary file: no text diff is available."));
            } else if self.review.rows.is_empty() {
                detail = detail.child(empty_note(if diff.truncated {
                    "The first changed line exceeds the review size limit."
                } else {
                    "No line changes in this version of the file."
                }));
            } else if self.review.wrap_lines {
                detail = detail.child(diff_list);
            } else {
                detail = detail.child(
                    div()
                        .id("review-diff-hscroll")
                        .flex_1()
                        .min_h(px(0.))
                        .w_full()
                        .overflow_x_scroll()
                        .child(div().min_w(px(1.)).child(diff_list)),
                );
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
                    .min_h(px(0.))
                    .id("review-file-scroll")
                    .overflow_y_scroll()
                    .p_3()
                    .child(file_content_view(
                        "review-file-content",
                        file,
                        self.review.wrap_lines,
                    )),
            );
        } else {
            detail = detail.child(empty_note("Select a changed file to review its diff."));
        }

        div()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .flex()
            .when(layout.phone, |element| element.flex_col())
            .child(body)
            .child(detail)
    }

    fn render_changes_detail_header(&self, path: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let subtitle = if self.project_child_review.is_some() {
            "Project child checkout · read only"
        } else if self.review.selected_file.is_some() {
            "Current file · no repository diff"
        } else if self.review.selected_staged {
            "Staged changes · read only"
        } else {
            "Working changes · read only"
        };
        let path_owned = path.to_owned();
        let patch_text = review_patch_text(self.review.selected_diff.as_ref());
        div()
            .w_full()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(0x30343f))
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .text_color(rgb(0xe5e7eb))
                            .child(path.to_owned()),
                    )
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(subtitle)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(!self.review.hunk_rows.is_empty(), |header| {
                        header
                            .child(
                                Button::new("previous-review-hunk")
                                    .icon(Icon::new(AssetIconName::ArrowUp))
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Previous hunk")
                                    .accessibility_label("Previous hunk")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.jump_review_hunk(false, cx)
                                    })),
                            )
                            .child(
                                Button::new("next-review-hunk")
                                    .icon(Icon::new(AssetIconName::ArrowUp))
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Next hunk")
                                    .accessibility_label("Next hunk")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.jump_review_hunk(true, cx)
                                    })),
                            )
                    })
                    .child(
                        Button::new("toggle-review-wrap")
                            .label(if self.review.wrap_lines {
                                "No wrap"
                            } else {
                                "Wrap"
                            })
                            .ghost()
                            .xsmall()
                            .tooltip("Toggle line wrapping")
                            .accessibility_label("Toggle line wrapping")
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_review_wrap(cx))),
                    )
                    .when(!patch_text.is_empty(), |header| {
                        header.child(
                            Button::new("copy-review-patch")
                                .icon(Icon::new(AssetIconName::Copy))
                                .ghost()
                                .xsmall()
                                .tooltip("Copy diff patch")
                                .accessibility_label("Copy diff patch")
                                .on_click({
                                    let patch_text = patch_text.clone();
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            patch_text.clone(),
                                        ));
                                    }
                                }),
                        )
                    })
                    .child(
                        Button::new("copy-review-path")
                            .label("Path")
                            .ghost()
                            .xsmall()
                            .tooltip("Copy file path")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    path_owned.clone(),
                                ));
                            }),
                    ),
            )
    }

    pub(crate) fn render_review_row(&self, index: usize) -> gpui_kit::Div {
        let Some(row) = self.review.rows.get(index) else {
            return div().w_full().min_h(px(22.));
        };
        let wrap = self.review.wrap_lines;
        match row {
            ReviewRow::Hunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
            } => {
                let (added, removed) = self.hunk_change_summary(index);
                let mut header = format!("@@ -{old_start},{old_lines} +{new_start},{new_lines} @@");
                if added > 0 || removed > 0 {
                    header.push_str(&format!("  +{added} -{removed}"));
                }
                let collapsed = self
                    .review
                    .hunk_rows
                    .iter()
                    .position(|candidate| *candidate == index)
                    .is_some_and(|hunk| self.review.collapsed_hunks.contains(&hunk));
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .bg(rgb(0x293244))
                    .font_family(mono_font())
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child(format!("{}  {header}", if collapsed { "▸" } else { "▾" }))
            }
            ReviewRow::Line(line) => {
                let (marker, background, foreground) = match line.kind {
                    GitDiffLineKind::Added => ("+", 0x24543d, 0xbbf7d0),
                    GitDiffLineKind::Removed => ("-", 0x542936, 0xfecaca),
                    GitDiffLineKind::Context => (" ", 0x17191f, 0xcbd5e1),
                };
                let language = self.diff_language();
                let spans = syntax::highlight(&line.content, language);
                let mut highlights = syntax::line_highlights(&spans, 0..line.content.len());
                if let Some((start, end)) = self.line_emphasis(index) {
                    let emphasis = match line.kind {
                        GitDiffLineKind::Added => rgb(0x86efac).alpha(0.28),
                        GitDiffLineKind::Removed => rgb(0xfca5a5).alpha(0.32),
                        GitDiffLineKind::Context => rgb(0x93c5fd).alpha(0.25),
                    };
                    highlights.push((
                        start..end,
                        HighlightStyle {
                            background_color: Some(emphasis.into()),
                            ..Default::default()
                        },
                    ));
                }
                div()
                    .w_full()
                    .min_h(px(22.))
                    .flex()
                    .items_start()
                    .bg(rgb(background))
                    .font_family(mono_font())
                    .text_xs()
                    .text_color(rgb(foreground))
                    .when(wrap, |element| element.whitespace_normal())
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
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            StyledText::new(line.content.clone()).with_highlights(highlights),
                        ),
                    )
            }
        }
    }

    /// The language used to highlight the selected diff, derived from its path.
    fn diff_language(&self) -> Language {
        self.review
            .selected_path
            .as_deref()
            .map(Language::from_path)
            .unwrap_or(Language::Text)
    }

    /// The added/removed line counts for the hunk whose header is at `index`.
    fn hunk_change_summary(&self, index: usize) -> (usize, usize) {
        let mut added = 0;
        let mut removed = 0;
        for row in self.review.rows.iter().skip(index + 1) {
            match row {
                ReviewRow::Hunk { .. } => break,
                ReviewRow::Line(line) => match line.kind {
                    GitDiffLineKind::Added => added += 1,
                    GitDiffLineKind::Removed => removed += 1,
                    GitDiffLineKind::Context => {}
                },
            }
        }
        (added, removed)
    }

    /// The differing byte range for a removed/added line paired with its
    /// neighbour, used for intra-line emphasis.
    fn line_emphasis(&self, index: usize) -> Option<(usize, usize)> {
        let current = match self.review.rows.get(index)? {
            ReviewRow::Line(line) => line,
            ReviewRow::Hunk { .. } => return None,
        };
        match current.kind {
            GitDiffLineKind::Removed => {
                let next = match self.review.rows.get(index + 1)? {
                    ReviewRow::Line(line) => line,
                    ReviewRow::Hunk { .. } => return None,
                };
                (next.kind == GitDiffLineKind::Added)
                    .then(|| emphasis_range(&current.content, &next.content))
                    .flatten()
            }
            GitDiffLineKind::Added => {
                if index == 0 {
                    return None;
                }
                let previous = match self.review.rows.get(index - 1)? {
                    ReviewRow::Line(line) => line,
                    ReviewRow::Hunk { .. } => return None,
                };
                (previous.kind == GitDiffLineKind::Removed)
                    .then(|| emphasis_range(&current.content, &previous.content))
                    .flatten()
            }
            GitDiffLineKind::Context => None,
        }
    }
}

/// Reconstructs a unified patch from the typed hunks, including hunk headers.
fn review_patch_text(diff: Option<&loom_protocol::GitDiff>) -> String {
    let Some(diff) = diff else {
        return String::new();
    };
    let mut patch = String::new();
    for hunk in &diff.hunks {
        patch.push_str(&format!(
            "@@ -{},{ } +{},{ } @@\n",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        ));
        for line in &hunk.lines {
            patch.push(match line.kind {
                GitDiffLineKind::Added => '+',
                GitDiffLineKind::Removed => '-',
                GitDiffLineKind::Context => ' ',
            });
            patch.push_str(&line.content);
            patch.push('\n');
        }
    }
    patch
}

/// The byte range in `line` that differs from `other`, or `None` when they are
/// identical. Shared prefixes and suffixes are left unemphasised.
fn emphasis_range(line: &str, other: &str) -> Option<(usize, usize)> {
    if line == other {
        return None;
    }
    let prefix = common_prefix(line, other);
    let line_tail = &line[prefix..];
    let other_tail = &other[prefix..];
    let suffix = common_suffix(line_tail, other_tail);
    let end = line.len() - suffix;
    (prefix < end).then_some((prefix, end))
}

fn common_prefix(left: &str, right: &str) -> usize {
    let mut length = 0;
    for (a, b) in left.chars().zip(right.chars()) {
        if a != b {
            break;
        }
        length += a.len_utf8();
    }
    length
}

fn common_suffix(left: &str, right: &str) -> usize {
    let mut length = 0;
    for (a, b) in left.chars().rev().zip(right.chars().rev()) {
        if a != b {
            break;
        }
        length += a.len_utf8();
    }
    length
}

fn counts(additions: u32, deletions: u32) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .font_family(mono_font())
        .text_xs()
        .when(additions > 0, |element| {
            element.child(
                div()
                    .text_color(rgb(0x86efac))
                    .child(format!("+{additions}")),
            )
        })
        .when(deletions > 0, |element| {
            element.child(
                div()
                    .text_color(rgb(0xfca5a5))
                    .child(format!("-{deletions}")),
            )
        })
}

fn repository_display_name(source: &str) -> String {
    source
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("Repository")
        .to_owned()
}

fn git_status_totals(status: &GitRepositoryStatus) -> (usize, u64, u64) {
    let mut additions = 0;
    let mut deletions = 0;
    for file in &status.files {
        let staged = matches!(
            file.worktree,
            GitFileStatusKind::Unknown | GitFileStatusKind::Ignored
        );
        let (file_additions, file_deletions) = if staged {
            (file.index_additions, file.index_deletions)
        } else {
            (file.worktree_additions, file.worktree_deletions)
        };
        additions += u64::from(file_additions);
        deletions += u64::from(file_deletions);
    }
    (status.files.len(), additions, deletions)
}

/// Renders a read-only file with line numbers when the lines are short enough,
/// otherwise falls back to plain wrapping text.
pub(crate) fn file_content_view(
    id: impl Into<gpui_kit::ElementId>,
    file: &loom_protocol::SessionFilesystemFile,
    wrap: bool,
) -> gpui_kit::AnyElement {
    let language = Language::from_path(&file.path);
    if wrap {
        render_code_block_wrapped(id, &file.content, language)
    } else {
        render_code_block(id, &file.content, language, true)
    }
}

fn render_code_block_wrapped(
    id: impl Into<gpui_kit::ElementId>,
    code: &str,
    language: Language,
) -> gpui_kit::AnyElement {
    let spans = syntax::highlight(code, language);
    let mut lines = code.split('\n').collect::<Vec<_>>();
    if lines.len() > 1 && lines.last() == Some(&"") {
        lines.pop();
    }
    let mut column = div()
        .id(id)
        .w_full()
        .flex()
        .flex_col()
        .font_family(mono_font())
        .text_size(gpui_kit::rems(mono_size() / BASE_FONT_SIZE))
        .whitespace_normal();
    let mut offset = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let line_start = offset;
        let line_end = line_start + line.len();
        let highlights = syntax::line_highlights(&spans, line_start..line_end);
        column = column.child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .w_full()
                .child(
                    div()
                        .w(px(34.))
                        .flex_shrink_0()
                        .pr_2()
                        .text_right()
                        .text_color(rgb(0x64748b))
                        .child(format!("{}", index + 1)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(StyledText::new(line.to_string()).with_highlights(highlights)),
                ),
        );
        offset = line_end + 1;
    }
    column.into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_protocol::{GitDiff, GitDiffHunk, GitDiffLine, GitDiffLineKind};

    #[test]
    fn emphasis_range_isolates_the_changed_span() {
        assert_eq!(emphasis_range("let x = 1;", "let x = 1;"), None);
        assert_eq!(emphasis_range("let x = 1;", "let x = 2;"), Some((8, 9)));
        // A shorter line has nothing beyond the shared prefix to emphasise.
        assert_eq!(emphasis_range("abc", "abc def"), None);
        assert_eq!(emphasis_range("abc def", "abc"), Some((3, 7)));
        // Multi-byte characters must stay on character boundaries.
        let (start, end) = emphasis_range("aαb", "aβb").unwrap();
        assert_eq!(&"aαb"[start..end], "α");
    }

    #[test]
    fn common_affixes_are_measured_in_bytes() {
        assert_eq!(
            common_prefix("prefix-left", "prefix-right"),
            "prefix-".len()
        );
        assert_eq!(common_suffix("abα", "cdα"), "α".len());
        assert_eq!(common_prefix("", "abc"), 0);
        assert_eq!(common_suffix("abc", "abc"), 3);
    }

    #[test]
    fn patch_text_reconstructs_hunk_headers_and_markers() {
        let diff = GitDiff {
            path: Some("src/lib.rs".to_owned()),
            staged: false,
            patch: String::new(),
            binary: false,
            truncated: false,
            hunks: vec![GitDiffHunk {
                old_start: 1,
                old_lines: 2,
                new_start: 1,
                new_lines: 2,
                lines: vec![
                    GitDiffLine {
                        kind: GitDiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                        content: "old".to_owned(),
                    },
                    GitDiffLine {
                        kind: GitDiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        content: "new".to_owned(),
                    },
                    GitDiffLine {
                        kind: GitDiffLineKind::Context,
                        old_line: Some(2),
                        new_line: Some(2),
                        content: "same".to_owned(),
                    },
                ],
            }],
        };
        assert_eq!(
            review_patch_text(Some(&diff)),
            "@@ -1,2 +1,2 @@\n-old\n+new\n same\n"
        );
        assert!(review_patch_text(None).is_empty());
    }
}
