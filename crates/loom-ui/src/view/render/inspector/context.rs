use super::*;

impl LoomView {
    pub(crate) fn render_inspector_context(
        &self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut content = inspector_scroll();

        match self.context_inspection.as_ref() {
            Some(inspection) => {
                let budget_text = inspection
                    .budget
                    .effective_input_tokens
                    .map(format_tokens)
                    .unwrap_or_else(|| "unknown budget".to_owned());
                let over_budget = inspection
                    .budget
                    .effective_input_tokens
                    .is_some_and(|budget| inspection.included_tokens > budget);
                let fraction = inspection
                    .budget
                    .effective_input_tokens
                    .filter(|budget| *budget > 0)
                    .map(|budget| {
                        (inspection.included_tokens as f32 / budget as f32).clamp(0.0, 1.0)
                    })
                    .unwrap_or(0.0);
                let mut budget = div().flex().flex_col().gap_2().child(info_row(
                    "Context",
                    format!(
                        "≈ {} / {}",
                        format_tokens(inspection.included_tokens),
                        budget_text
                    ),
                ));
                budget = budget.child(
                    div()
                        .w_full()
                        .h(px(6.))
                        .rounded_full()
                        .bg(rgb(0x293244))
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .w(gpui_kit::relative(fraction))
                                .bg(if over_budget {
                                    rgb(0xfca5a5)
                                } else {
                                    rgb(0x60a5fa)
                                }),
                        ),
                );
                budget = budget
                    .child(info_row(
                        "Reserved output",
                        format_tokens(inspection.budget.reserved_output_tokens),
                    ))
                    .child(info_row(
                        "Omitted",
                        format_tokens(inspection.omitted_tokens),
                    ))
                    .child(info_row("Total", format_tokens(inspection.total_tokens)));
                content = content.child(card("Budget", budget));

                let items = if inspection.items.is_empty() {
                    empty_note("No context items reported.").into_any_element()
                } else {
                    let mut list = div().flex().flex_col().gap_1();
                    for item in &inspection.items {
                        let mut row = div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .py_1()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(rgb(0x293244))
                                    .text_xs()
                                    .text_color(if item.included {
                                        rgb(0x86efac)
                                    } else {
                                        rgb(0x8f98a6)
                                    })
                                    .child(context_item_label(item.kind)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_sm()
                                    .text_color(rgb(0xe5e7eb))
                                    .child(item.label.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .font_family(mono_font())
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child(format_tokens(item.estimated_tokens)),
                            );
                        if let Some(reason) = &item.omission_reason {
                            row = row.child(
                                div()
                                    .flex_shrink_0()
                                    .max_w(px(160.))
                                    .truncate()
                                    .text_xs()
                                    .text_color(rgb(0xfcd34d))
                                    .child(reason.clone()),
                            );
                        }
                        list = list.child(row);
                    }
                    list.into_any_element()
                };
                content = content.child(card("Items", items));

                if inspection.compacted || inspection.summary.is_some() {
                    let mut details = div().flex().flex_col().gap_2().child(info_row(
                        "Compacted",
                        if inspection.compacted { "yes" } else { "no" },
                    ));
                    if let Some(summary) = &inspection.summary {
                        details = details
                            .child(info_row(
                                "Source messages",
                                summary.source_message_count.to_string(),
                            ))
                            .child(info_row(
                                "Projection",
                                format!("v{}", summary.projection_version),
                            ))
                            .child(info_row(
                                "Created",
                                format!("{}", summary.created_at.as_unix_millis()),
                            ))
                            .child(
                                div()
                                    .w_full()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child(summary.text.clone()),
                            );
                    }
                    content = content.child(card("Compaction", details));
                }
            }
            None => {
                content = content.child(card(
                    "Context",
                    empty_note("No context inspection is available for this run yet."),
                ));
            }
        }

        let refresh = div().w_full().flex().justify_start().child(
            Button::new("refresh-context")
                .label("Refresh context")
                .small()
                .disabled(self.active_run_id.is_none())
                .on_click(cx.listener(|this, _, _, cx| this.refresh_context(cx))),
        );
        content = content.child(refresh);
        content = content.child(self.render_usage_card());

        content
    }
}

fn context_item_label(kind: loom_protocol::ContextItemKind) -> &'static str {
    match kind {
        loom_protocol::ContextItemKind::SystemInstructions => "system",
        loom_protocol::ContextItemKind::RepositoryInstructions => "repo",
        loom_protocol::ContextItemKind::Task => "task",
        loom_protocol::ContextItemKind::Summary => "summary",
        loom_protocol::ContextItemKind::Conversation => "conversation",
    }
}
