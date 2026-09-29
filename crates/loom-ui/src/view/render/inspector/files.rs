use super::changes::file_content_view;
use super::*;

/// A soft cap so a very large workspace snapshot cannot stall the first paint.
const MAX_FILE_LIST_ENTRIES: usize = 1000;

impl LoomView {
    pub(crate) fn render_inspector_files(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let layout = responsive_layout(window.bounds().size.width);
        let mut list_body = div()
            .when(!layout.phone, |element| {
                element
                    .w(px(230.))
                    .h_full()
                    .border_r_1()
                    .border_color(rgb(0x30343f))
            })
            .when(layout.phone, |element| {
                element
                    .h(px(180.))
                    .w_full()
                    .border_b_1()
                    .border_color(rgb(0x30343f))
            })
            .id("files-sidebar-scroll")
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_2();

        if self.review.files.loading {
            list_body = list_body.child(empty_note("Loading files…"));
        } else if let Some(error) = &self.review.files.error {
            list_body = list_body.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        } else if !self.review.files.loaded {
            list_body = list_body.child(empty_note("No filesystem snapshot loaded."));
        } else {
            let mut entries = self
                .review
                .files
                .entries
                .iter()
                .filter(|entry| entry.kind == WorkspaceEntryKind::File)
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.path.cmp(&right.path));
            if entries.is_empty() {
                list_body = list_body.child(empty_note("No files in this workspace."));
            }
            for (index, entry) in entries.iter().take(MAX_FILE_LIST_ENTRIES).enumerate() {
                let selected = self.review.files.selected_path.as_deref() == Some(&entry.path);
                let path = entry.path.clone();
                list_body = list_body.child(
                    div()
                        .id(("inspector-file", index))
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
                                .w(px(16.))
                                .flex_shrink_0()
                                .text_color(rgb(0x8f98a6))
                                .child(Icon::new(AssetIconName::FileText).size_4()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_sm()
                                .text_color(rgb(0xe5e7eb))
                                .child(entry.path.clone()),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_inspector_file(path.clone(), cx);
                        })),
                );
            }
            if entries.len() > MAX_FILE_LIST_ENTRIES {
                list_body = list_body.child(empty_note(&format!(
                    "Showing the first {MAX_FILE_LIST_ENTRIES} of {} files.",
                    entries.len()
                )));
            }
        }

        let detail = self.render_files_detail(cx);
        div()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .flex()
            .when(layout.phone, |element| element.flex_col())
            .child(list_body)
            .child(detail)
    }

    fn render_files_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut detail = div().flex_1().min_w(px(0.)).flex().flex_col();
        let Some(path) = self.review.files.selected_path.clone() else {
            detail = detail.child(empty_note("Select a file to view it read-only."));
            if !self.review.files.loaded && !self.review.files.loading {
                detail = detail.child(
                    div().px_3().child(
                        Button::new("load-inspector-files")
                            .label("Load files")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.load_inspector_files(cx))),
                    ),
                );
            }
            return detail;
        };
        let content_copy = self
            .review
            .files
            .selected_file
            .as_ref()
            .map(|file| file.content.clone())
            .unwrap_or_default();
        detail = detail.child(
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
                                .child(path),
                        )
                        .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Read only")),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            Button::new("toggle-files-wrap")
                                .label(if self.review.wrap_lines {
                                    "No wrap"
                                } else {
                                    "Wrap"
                                })
                                .ghost()
                                .xsmall()
                                .tooltip("Toggle line wrapping")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.toggle_review_wrap(cx)),
                                ),
                        )
                        .child(
                            Button::new("copy-files-content")
                                .icon(Icon::new(AssetIconName::Copy))
                                .ghost()
                                .xsmall()
                                .tooltip("Copy file contents")
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        content_copy.clone(),
                                    ));
                                }),
                        ),
                ),
        );
        if self.review.files.file_loading {
            detail = detail.child(empty_note("Loading file…"));
        } else if let Some(error) = &self.review.files.file_error {
            detail = detail.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        } else if let Some(file) = &self.review.files.selected_file {
            detail = detail.child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .id("inspector-file-scroll")
                    .overflow_y_scroll()
                    .p_3()
                    .child(file_content_view(
                        "inspector-file-content",
                        file,
                        self.review.wrap_lines,
                    )),
            );
        }
        detail
    }
}
