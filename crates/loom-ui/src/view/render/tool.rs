use super::*;

impl LoomView {
    pub(crate) fn render_assistant_turn(
        &self,
        turn: &AssistantTurn,
        index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let role = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(3.)).h(px(13.)).rounded_full().bg(rgb(0x9ad7bd)))
            .child(div().text_xs().text_color(rgb(0x9ad7bd)).child("Agent"));
        let mut body = div().px_3().py_2().flex().flex_col().gap_1().child(role);

        // Every reasoning part in the response is gathered into one disclosure
        // at the top of the agent entry, so a tool-using model that thinks
        // before each call still reads as a single turn. Low hides reasoning
        // entirely, like tool activity; High starts the disclosure expanded.
        let reasoning = turn
            .parts
            .iter()
            .enumerate()
            .filter_map(|(part_index, part)| match part {
                AssistantPart::Reasoning(text) if !text.trim().is_empty() => {
                    Some((part_index, text.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if self.output_detail.shows_activity() && self.show_reasoning && !reasoning.is_empty() {
            let first_part_index = reasoning[0].0;
            let key = tool_element_id(index, first_part_index);
            let combined = reasoning
                .iter()
                .map(|(_, text)| text.trim())
                .collect::<Vec<_>>()
                .join("\n\n");
            body = body.child(Self::render_reasoning(
                index,
                first_part_index,
                &combined,
                disclosure_expanded(
                    &self.expanded_reasoning,
                    &key,
                    self.output_detail.expands_activity(),
                ),
                parent,
            ));
        }

        let mut part_index = 0;
        while part_index < turn.parts.len() {
            match &turn.parts[part_index] {
                AssistantPart::Reasoning(_) => {
                    part_index += 1;
                }
                AssistantPart::Text(text) => {
                    if !text.trim().is_empty() {
                        let mut display = text.clone();
                        if turn.streaming && part_index + 1 == turn.parts.len() {
                            display.push('▮');
                        }
                        // The wrapper carries the part's id so a transcript test
                        // can tell which parts a detail level renders.
                        body = body.child(
                            div()
                                .id(("assistant-text", tool_element_id(index, part_index)))
                                .test_support()
                                .w_full()
                                .child(render_timeline_text(
                                    format!("transcript-assistant-{index}-{part_index}"),
                                    display,
                                    0xf3f4f6,
                                )),
                        );
                    }
                    part_index += 1;
                }
                AssistantPart::Tool(_) => {
                    // All tool calls in a response collapse behind one usage
                    // summary line; expanding it reveals the existing per-tool
                    // presentation. Reasoning between calls is gathered at the
                    // top of the entry, so it does not split the run.
                    let mut end = part_index + 1;
                    while end < turn.parts.len() {
                        match &turn.parts[end] {
                            AssistantPart::Tool(_) | AssistantPart::Reasoning(_) => end += 1,
                            _ => break,
                        }
                    }
                    let parts = &turn.parts[part_index..end];
                    // Low hides the agent's work, except for a run blocked on the
                    // user: its usage block holds the approval and input
                    // controls, so it stays reachable at every level.
                    if self.output_detail.shows_activity()
                        || tool_needs_attention(&run_tool_parts(parts))
                    {
                        body = body.child(self.render_tool_usage(parts, index, part_index, parent));
                    }
                    part_index = end;
                }
                AssistantPart::Evidence(links) => {
                    body = body.child(Self::render_evidence(links, index, part_index));
                    part_index += 1;
                }
            }
        }
        let last_is_text = matches!(
            turn.parts.last(),
            Some(AssistantPart::Text(text)) if !text.trim().is_empty()
        );
        if turn.streaming && !last_is_text {
            body = body.child(streaming_caret(index));
        }
        body.into_any()
    }

    /// The gathered reasoning disclosure for one agent entry. The caller
    /// concatenates every reasoning part; the disclosure is keyed by the first
    /// reasoning part so its expansion state survives re-renders.
    fn render_reasoning(
        index: usize,
        first_part_index: usize,
        text: &str,
        expanded: bool,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let key = tool_element_id(index, first_part_index);
        let parent_for_toggle = parent.clone();
        let mut block = div()
            .id(("reasoning", key))
            .test_support()
            .flex()
            .flex_col()
            .pl_2()
            .border_l_2()
            .border_color(rgb(0x3b4555))
            .child(
                Button::new(("reasoning-header", key))
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label("Reasoning")
                    .on_click(move |_, _, cx| {
                        parent_for_toggle.update(cx, |this, cx| {
                            this.set_reasoning_expanded(key, !expanded, cx)
                        });
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(disclosure_chevron(expanded, rgb(0x64748b)))
                            .child(div().text_color(rgb(0x94a3b8)).child("Reasoning"))
                            .when(!expanded, |element| {
                                element.child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_color(rgb(0x64748b))
                                        .child(reasoning_preview(text)),
                                )
                            }),
                    ),
            );
        if expanded {
            // The wrapper carries the body's id so a transcript test can tell
            // whether the disclosure was open without clicking.
            block = block.child(
                div()
                    .id(("reasoning-body", key))
                    .test_support()
                    .mt_1()
                    .child(render_timeline_text(
                        format!("transcript-reasoning-{index}-{first_part_index}"),
                        text.to_owned(),
                        0x94a3b8,
                    )),
            );
        }
        block.into_any()
    }

    /// One collapsed line for a run of tool calls, summarizing the tools by type
    /// and invocation count. Expanding it shows the existing per-tool
    /// presentation. A run that is blocked on an approval or an input decision
    /// opens automatically so the control stays reachable, and the High output
    /// detail level opens it because the level says so. Both are display
    /// defaults rather than a lock: a click records the user's own choice, which
    /// wins over them.
    pub(crate) fn render_tool_usage(
        &self,
        parts: &[AssistantPart],
        index: usize,
        first_part_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let tools = run_tool_parts(parts);
        if tools.is_empty() {
            return div().into_any();
        }
        let status = tool_group_status(&tools);
        let status_color = match status {
            ToolPartStatus::Failed => rgb(0xfca5a5),
            ToolPartStatus::Denied => rgb(0xfbbf24),
            ToolPartStatus::Running => rgb(0x93c5fd),
            ToolPartStatus::Queued | ToolPartStatus::Cancelled => rgb(0x94a3b8),
            ToolPartStatus::Completed
            | ToolPartStatus::AwaitingApproval
            | ToolPartStatus::AwaitingInput => rgb(0x9ad7bd),
        };
        let status_label = status.label();
        let key = tool_element_id(index, first_part_index);
        let expanded = disclosure_expanded(
            &self.expanded_tool_usage,
            &key,
            tool_needs_attention(&tools) || self.output_detail.expands_activity(),
        );
        let total_ms = tools.iter().filter_map(|tool| tool.elapsed_ms).sum::<u64>();
        let duration = (total_ms > 0).then(|| format_duration(total_ms));
        let summary = tool_usage_summary(&tools);
        let notes = tool_outcome_notes(&tools, status);
        let accessibility_notes = notes
            .iter()
            .map(|(_, note)| format!(", {note}"))
            .collect::<String>();
        let parent_for_toggle = parent.clone();
        let mut block = div()
            .id(("tool-usage", key))
            .test_support()
            .w_full()
            .pl_2()
            .border_l_2()
            .border_color(status_color.opacity(0.4))
            .flex()
            .flex_col()
            .child(
                Button::new(("tool-usage-header", key))
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label(format!(
                        "Tools used: {summary}, {status_label}{accessibility_notes}"
                    ))
                    .on_click(move |_, _, cx| {
                        parent_for_toggle.update(cx, |this, cx| {
                            this.set_tool_usage_expanded(key, !expanded, cx)
                        });
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(
                                Icon::new(AssetIconName::Wrench)
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(status_color),
                            )
                            .child(disclosure_chevron(expanded, rgb(0x64748b)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(rgb(0xdbeafe))
                                    .child(summary),
                            )
                            .child(div().text_color(status_color).child(status_label))
                            .children(notes.into_iter().map(outcome_note))
                            .when_some(duration, |element, duration| {
                                element.child(div().text_color(rgb(0x64748b)).child(duration))
                            }),
                    ),
            );
        if expanded {
            block = block.child(div().mt_1().flex().flex_col().child(self.render_tool_run(
                parts,
                index,
                first_part_index,
                parent,
            )));
        }
        block.into_any()
    }

    /// The per-tool presentation shown once the usage summary is expanded:
    /// same-kind runs of at least `TOOL_GROUP_THRESHOLD` collapse into one group
    /// row, everything else renders as an individual block, keeping the order in
    /// which the calls arrived.
    pub(crate) fn render_tool_run(
        &self,
        parts: &[AssistantPart],
        index: usize,
        first_part_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let mut run = div().flex().flex_col();
        let mut cursor = 0;
        while cursor < parts.len() {
            let Some(AssistantPart::Tool(part)) = parts.get(cursor) else {
                cursor += 1;
                continue;
            };
            let mut end = cursor + 1;
            while end < parts.len() {
                match &parts[end] {
                    AssistantPart::Tool(next) if next.name == part.name => end += 1,
                    _ => break,
                }
            }
            if end - cursor >= TOOL_GROUP_THRESHOLD {
                run = run.child(self.render_tool_group(
                    &parts[cursor..end],
                    index,
                    first_part_index + cursor,
                    parent,
                ));
            } else {
                for offset in cursor..end {
                    if let Some(AssistantPart::Tool(part)) = parts.get(offset) {
                        run = run.child(self.render_tool_part(
                            part,
                            index,
                            first_part_index + offset,
                            parent,
                        ));
                    }
                }
            }
            cursor = end;
        }
        run.into_any()
    }

    /// One collapsed row for a run of same-kind tool calls, expandable to the
    /// individual blocks.
    pub(crate) fn render_tool_group(
        &self,
        parts: &[AssistantPart],
        index: usize,
        first_part_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let tools = run_tool_parts(parts);
        let Some(first) = tools.first() else {
            return div().into_any();
        };
        let status = tool_group_status(&tools);
        let status_color = match status {
            ToolPartStatus::Failed => rgb(0xfca5a5),
            ToolPartStatus::Denied => rgb(0xfbbf24),
            ToolPartStatus::Running => rgb(0x93c5fd),
            ToolPartStatus::Queued | ToolPartStatus::Cancelled => rgb(0x94a3b8),
            ToolPartStatus::Completed
            | ToolPartStatus::AwaitingApproval
            | ToolPartStatus::AwaitingInput => rgb(0x9ad7bd),
        };
        let status_label = status.label();
        let key = tool_element_id(index, first_part_index);
        // A group starts collapsed like the summary above it; only a call that
        // needs the user opens it, so a running group does not flicker open and
        // shut as each call settles. The High output detail level also opens it
        // because the level says so. Both are display defaults rather than a
        // lock: a click records the user's own choice, which wins over them.
        let expanded = disclosure_expanded(
            &self.expanded_tool_groups,
            &key,
            tool_needs_attention(&tools) || self.output_detail.expands_activity(),
        );
        let total_ms = tools.iter().filter_map(|tool| tool.elapsed_ms).sum::<u64>();
        let duration = (total_ms > 0).then(|| format_duration(total_ms));
        let label = tool_group_label(&first.name, tools.len());
        let notes = tool_outcome_notes(&tools, status);
        let accessibility_notes = notes
            .iter()
            .map(|(_, note)| format!(", {note}"))
            .collect::<String>();
        let parent_for_toggle = parent.clone();
        let mut group = div()
            .id(("tool-group", key))
            .test_support()
            .w_full()
            .pl_2()
            .border_l_2()
            .border_color(status_color.opacity(0.4))
            .flex()
            .flex_col()
            .child(
                Button::new(("tool-group-header", key))
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label(format!("{label}, {status_label}{accessibility_notes}"))
                    .on_click(move |_, _, cx| {
                        parent_for_toggle.update(cx, |this, cx| {
                            this.set_tool_group_expanded(key, !expanded, cx)
                        });
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(
                                Icon::new(tool_icon(&first.name))
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(status_color),
                            )
                            .child(disclosure_chevron(expanded, rgb(0x64748b)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(rgb(0xdbeafe))
                                    .child(label),
                            )
                            .child(div().text_color(status_color).child(status_label))
                            .children(notes.into_iter().map(outcome_note))
                            .when_some(duration, |element, duration| {
                                element.child(div().text_color(rgb(0x64748b)).child(duration))
                            }),
                    ),
            );
        if expanded {
            for offset in first_part_index..(first_part_index + tools.len()) {
                if let Some(AssistantPart::Tool(part)) = parts.get(offset - first_part_index) {
                    group = group.child(self.render_tool_part(part, index, offset, parent));
                }
            }
        }
        group.into_any()
    }

    pub(crate) fn render_evidence(
        links: &[EvidenceText],
        index: usize,
        part_index: usize,
    ) -> gpui_kit::AnyElement {
        let mut list = div().flex().flex_col().gap_1().pt_1();
        for (link_index, link) in links.iter().enumerate() {
            let uri = link.uri.clone();
            let label = if link.label.trim().is_empty() {
                link.uri.clone()
            } else {
                link.label.clone()
            };
            list = list.child(
                Button::new((
                    "evidence",
                    ((index as u64) << 32) | ((part_index as u64) << 16) | link_index as u64,
                ))
                .ghost()
                .small()
                .accessibility_label(format!("Open evidence: {label}"))
                .on_click(move |_, _, _| {
                    let _ = open_external_url(&uri);
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .text_color(rgb(0x93c5fd))
                        .child(
                            Icon::new(AssetIconName::ExternalLink)
                                .size_3()
                                .text_color(rgb(0x93c5fd)),
                        )
                        .child(label),
                ),
            );
        }
        list.into_any()
    }

    pub(crate) fn render_tool_part(
        &self,
        part: &ToolPart,
        index: usize,
        part_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let status_color = match part.status {
            ToolPartStatus::Failed => rgb(0xfca5a5),
            ToolPartStatus::Denied => rgb(0xfbbf24),
            ToolPartStatus::Completed => rgb(0x9ad7bd),
            ToolPartStatus::AwaitingApproval | ToolPartStatus::AwaitingInput => rgb(0xfef3c7),
            ToolPartStatus::Running => rgb(0x93c5fd),
            ToolPartStatus::Queued | ToolPartStatus::Cancelled => rgb(0x94a3b8),
        };
        let duration = part.elapsed_ms.map(format_duration);
        // Active and approval-gated work starts open; finished successes collapse
        // so the transcript stays scannable. A failure is collapsed too, and a
        // block with nothing to reveal has no disclosure control at all. The
        // High output detail level opens every block because the level says so.
        // Every one of those is a display default rather than a lock: a click
        // records the user's own choice, which wins over them.
        let expandable = tool_has_body(part);
        let expanded = expandable
            && disclosure_expanded(
                &self.expanded_tools,
                &part.id,
                self.output_detail.expands_activity()
                    || matches!(
                        part.status,
                        ToolPartStatus::Running
                            | ToolPartStatus::AwaitingApproval
                            | ToolPartStatus::AwaitingInput
                    ),
            );
        let call_id = part.id;
        let parent_for_toggle = parent.clone();
        let awaiting = part.status == ToolPartStatus::AwaitingApproval
            && self
                .pending_approval
                .as_ref()
                .is_some_and(|call| call.id == part.id);
        let patch_summary = part.output.as_deref().and_then(syntax::patch_summary);
        let mut block = div()
            .id(("tool", tool_element_id(index, part_index)))
            .test_support()
            .w_full()
            .pl_2()
            .border_l_2()
            .border_color(status_color.opacity(0.4))
            .flex()
            .flex_col()
            .child(
                Button::new(("tool-header", tool_element_id(index, part_index)))
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label(format!("{}: {}", part.title, part.status.label()))
                    .on_click(move |_, _, cx| {
                        if expandable {
                            parent_for_toggle.update(cx, |this, cx| {
                                this.set_tool_expanded(call_id, !expanded, cx)
                            });
                        }
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(
                                Icon::new(tool_icon(&part.name))
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(status_color),
                            )
                            .child(
                                div()
                                    .w(px(12.))
                                    .flex_shrink_0()
                                    .text_color(rgb(0x64748b))
                                    .when(expandable, |element| {
                                        element.child(disclosure_chevron(expanded, rgb(0x64748b)))
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_family(mono_font())
                                    .text_color(rgb(0xdbeafe))
                                    .child(tool_display_title(part)),
                            )
                            .when_some(patch_summary, |element, summary| {
                                element.child(
                                    div()
                                        .font_family(mono_font())
                                        .text_color(rgb(0x64748b))
                                        .child(summary),
                                )
                            })
                            .child(div().text_color(status_color).child(part.status.label()))
                            .when_some(duration, |element, duration| {
                                element.child(div().text_color(rgb(0x64748b)).child(duration))
                            }),
                    ),
            );
        if let Some(reason) = tool_failure_reason(part) {
            // The reason is the point of the row, so it is always visible
            // instead of sitting behind the disclosure that holds the whole
            // output.
            block = block.child(
                div()
                    .id(("tool-failure-note", tool_element_id(index, part_index)))
                    .test_support()
                    .ml(px(20.))
                    .pt_1()
                    .text_xs()
                    .text_color(rgb(0xfca5a5))
                    .aria_label(reason.clone())
                    .child(reason),
            );
        }
        if let Some(note) = tool_policy_note(part) {
            // The decision is the point of the row, so the action kind and the
            // reason are always visible instead of sitting behind a disclosure.
            block = block.child(
                div()
                    .id(("tool-policy-note", tool_element_id(index, part_index)))
                    .test_support()
                    .ml(px(20.))
                    .pt_1()
                    .text_xs()
                    .text_color(rgb(0xfbbf24))
                    .aria_label(note.clone())
                    .child(note),
            );
        }
        if expanded {
            if let Some(detail) = tool_display_detail(part) {
                block = block.child(
                    div()
                        .ml(px(20.))
                        .pt_1()
                        .font_family(mono_font())
                        .text_size(gpui_kit::rems(mono_size() / BASE_FONT_SIZE))
                        .text_color(rgb(0x94a3b8))
                        .child(detail),
                );
            }
            if tool_shows_output(part) {
                let output = part.output.as_deref().unwrap_or_default();
                let output_id = tool_element_id(index, part_index);
                block = block.child(
                    div()
                        .ml(px(20.))
                        .mt_1()
                        .w_full()
                        .min_w(px(0.))
                        .border_l_1()
                        .border_color(rgb(0x30343f))
                        .pl_2()
                        .text_color(rgb(0x8f98a6))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .py_1()
                                .child(
                                    div()
                                        .font_family(mono_font())
                                        .text_xs()
                                        .text_color(rgb(0x64748b))
                                        .child(tool_output_language(part).label()),
                                )
                                .child(
                                    Button::new(("copy-tool-output", output_id))
                                        .icon(Icon::new(AssetIconName::Copy))
                                        .ghost()
                                        .xsmall()
                                        .accessibility_label("Copy tool output")
                                        .tooltip("Copy output")
                                        .on_click({
                                            let output = output.to_owned();
                                            move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    output.clone(),
                                                ));
                                            }
                                        }),
                                ),
                        )
                        .child(render_tool_output(("tool-output", output_id), part)),
                );
            }
        }
        if awaiting && !self.approval_request_in_flight && self.session_permissions().can_act {
            let parent_for_approve = parent.clone();
            let parent_for_reject = parent.clone();
            block = block.child(
                div()
                    .ml(px(20.))
                    .mt_1()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new(("approve-tool", tool_element_id(index, part_index)))
                            .label("Approve")
                            .success()
                            .small()
                            .on_click(move |_, _, cx| {
                                parent_for_approve
                                    .update(cx, |this, cx| this.approve_pending_action(cx));
                            }),
                    )
                    .child(
                        Button::new(("reject-tool", tool_element_id(index, part_index)))
                            .label("Reject")
                            .danger()
                            .small()
                            .on_click(move |_, _, cx| {
                                parent_for_reject
                                    .update(cx, |this, cx| this.reject_pending_action(cx));
                            }),
                    ),
            );
        } else if awaiting && self.approval_request_in_flight {
            block = block.child(
                div()
                    .ml(px(20.))
                    .mt_1()
                    .text_xs()
                    .text_color(rgb(0x94a3b8))
                    .child("Submitting approval..."),
            );
        }
        block.into_any()
    }

    pub(crate) fn render_timeline_item(
        &self,
        item: &TimelineItem,
        index: usize,
        layout: ResponsiveLayout,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        match item {
            // The user's message sits on the trailing edge in a tinted bubble so
            // the back-and-forth with the agent reads at a glance. The role row
            // mirrors the agent's leading accent bar and label.
            TimelineItem::User(text) => {
                // Block Markdown (a numbered list, fenced code, a table) sizes
                // itself to its minimum width, so a content-sized bubble
                // collapses to a one-character column. Give those bubbles a
                // definite width and keep short messages content-sized.
                let mut bubble = Bubble::new()
                    .alignment(MessageAlignment::End)
                    .with_variant(BubbleVariant::Secondary);
                if has_block_markdown(text) {
                    bubble = bubble.w_full().content(BubbleContent::new().w_full());
                }
                // In a shared session the sender is named with their role, so
                // a direction reads "Name · Role -> Agent" rather than "You".
                let speaker = match self.collaboration.author_of(text) {
                    Some(author @ loom_core::Author::Participant { .. }) => {
                        format!("{} -> Agent", crate::collaboration::author_label(author))
                    }
                    _ => "You".to_owned(),
                };
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_col()
                    .child(
                        bubble
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .gap_2()
                                    .child(div().text_xs().text_color(rgb(0xbfdbfe)).child(speaker))
                                    .child(
                                        div().w(px(3.)).h(px(13.)).rounded_full().bg(rgb(0x60a5fa)),
                                    ),
                            )
                            .child(div().mt_1().child(render_timeline_text(
                                format!("transcript-user-{index}"),
                                text.clone(),
                                0xf3f4f6,
                            ))),
                    )
                    .into_any()
            }
            TimelineItem::Assistant(turn) => self.render_assistant_turn(turn, index, parent),
            TimelineItem::Collaboration(entry) => render_collaboration_entry(entry, index),
            TimelineItem::System(note) => {
                // A clarification request is a question for the user to answer,
                // not a status line, so it gets its own component instead of the
                // generic note card.
                if note.tone == SystemTone::Input {
                    return Self::render_agent_question(note, layout, parent);
                }
                let (surface, accent) = match note.tone {
                    SystemTone::Error => (rgb(ERROR_CARD_SURFACE), rgb(ERROR_CARD_ACCENT)),
                    SystemTone::Input => (rgb(0x241f3b), rgb(0xc4b5fd)),
                    SystemTone::Neutral => (rgb(0x191c22), rgb(0x64748b)),
                };
                let text_color = match note.tone {
                    SystemTone::Error => ERROR_CARD_FOREGROUND,
                    SystemTone::Input => 0xe9d5ff,
                    SystemTone::Neutral => 0x94a3b8,
                };
                let mut card = div()
                    .id(("system-note", index))
                    .test_support()
                    .px_3()
                    .py_2()
                    .rounded_sm()
                    .bg(surface)
                    .text_color(rgb(text_color));
                if let Some(heading) = &note.heading {
                    card = card.child(div().text_xs().text_color(accent).child(heading.clone()));
                }
                card = card.child(render_timeline_text(
                    format!("timeline-system-{index}"),
                    note.text.clone(),
                    text_color,
                ));
                if note.retryable {
                    card = card.child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(accent)
                            // The note has no control to offer, so it names the
                            // one that does instead of promising a retry the
                            // user cannot start from here.
                            .child("Retry by sending another message."),
                    );
                }
                card.into_any()
            }
        }
    }

    /// The agent's clarification request, rendered as its own component instead
    /// of the generic note card.
    ///
    /// The question is the point of the entry, so it is never behind a
    /// disclosure: the whole text wraps to the available width and stays
    /// readable at phone sizes. The answer action is the next step and hands the
    /// user to the composer, where `pending_input` expects the reply. The
    /// lifecycle heading is replaced by the question framing, which says the
    /// same thing to the reader.
    fn render_agent_question(
        note: &SystemNote,
        layout: ResponsiveLayout,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let parent_for_answer = parent.clone();
        let mut card = div()
            .id("agent-question")
            .test_support()
            .w_full()
            .min_w(px(0.))
            .px_3()
            .py_2()
            .rounded_sm()
            .bg(rgb(0x241f3b))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .id("agent-question-heading")
                    .test_support()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .child(div().w(px(3.)).h(px(13.)).rounded_full().bg(rgb(0xc4b5fd)))
                    .child(div().text_color(rgb(0xc4b5fd)).child("Agent asks")),
            )
            .child(
                // One wrapping text element, so the question reads in full
                // instead of being clipped at a fixed width.
                div()
                    .id("agent-question-text")
                    .test_support()
                    .w_full()
                    .min_w(px(0.))
                    .aria_label(note.text.clone())
                    .text_sm()
                    .text_color(rgb(0xe9d5ff))
                    .child(note.text.clone()),
            );
        card = card.child(
            Button::new("agent-question-answer")
                .label("Answer")
                .small()
                .accessibility_label("Answer the agent question")
                .when(layout.phone, |button| {
                    button.h(layout.control_size()).w_full()
                })
                .on_click(move |_, window, cx| {
                    parent_for_answer.update(cx, |view, cx| view.focus_composer(window, cx));
                }),
        );
        card.into_any()
    }

    pub(crate) fn timeline_entity(&mut self, cx: &mut Context<Self>) -> Entity<TimelineView> {
        if let Some(timeline_view) = &self.timeline_view {
            return timeline_view.clone();
        }

        let parent = cx.entity();
        let timeline_view = cx.new(|cx| {
            let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
            TimelineView::new(parent, scroller)
        });
        self.timeline_view = Some(timeline_view.clone());
        timeline_view
    }

    /// The single line that carries run progress: the Loom mark, state,
    /// elapsed, and the interrupt hint. Run state lives here instead of the
    /// session header.
    pub(crate) fn render_run_status(&self) -> gpui_kit::Div {
        let running = self.run_is_active();
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .mb_2()
            .min_h(px(16.))
            .text_xs();
        if running || self.sending_message || self.pending_input.is_some() {
            let color = self
                .run_state
                .map(run_state_color)
                .unwrap_or_else(|| rgb(0x93c5fd));
            row = row.child(loom_mark(color));
        }
        if let Some(state) = self.run_state {
            let label = if self.project_has_live_children() && !self.run_is_active() {
                "Waiting for sub-agents".to_owned()
            } else {
                run_state_label(Some(state)).to_owned()
            };
            let color = run_state_color(state);
            row = row.child(div().text_color(color).child(label));
            // A failed run names the reason it failed next to the state label,
            // so the status row describes the failure instead of only labeling
            // it. The reason is presentation state from the active run's
            // snapshot; no other state adds one.
            if state == AgentRunState::Failed
                && let Some(reason) = run_failure_reason(
                    self.active_run
                        .as_ref()
                        .and_then(|run| run.summary.as_deref()),
                )
            {
                row = row.child(
                    div()
                        .id("run-failure-reason")
                        .test_support()
                        .text_color(color)
                        .child(reason),
                );
            }
        } else if self.sending_message {
            row = row.child(div().text_color(rgb(0x93c5fd)).child("Sending"));
        } else if self.pending_input.is_some() {
            row = row.child(
                div()
                    .text_color(rgb(0xfbbf24))
                    .child("Waiting for your answer"),
            );
        } else {
            row = row.child(div().text_color(rgb(0x64748b)).child("Ready"));
        }
        if let Some(elapsed) = self.run_elapsed_label() {
            row = row.child(
                div()
                    .font_family(mono_font())
                    .text_color(rgb(0x64748b))
                    .child(elapsed),
            );
        }
        row = row.child(div().flex_1());
        if running {
            row = row.child(div().text_color(rgb(0x64748b)).child("esc to interrupt"));
        }
        row
    }

    pub(crate) fn run_elapsed_label(&self) -> Option<String> {
        let run = self.active_run.as_ref()?;
        let end = run.completed_at.unwrap_or_else(Timestamp::now);
        let milliseconds = end
            .as_unix_millis()
            .saturating_sub(run.started_at.as_unix_millis());
        Some(format_duration(milliseconds))
    }
}

/// The colour of a collapsed run's outcome note: a policy denial is amber like
/// the denied row, and an operational failure is the failure red.
fn outcome_note_color(condition: ToolPartStatus) -> Rgba {
    match condition {
        ToolPartStatus::Denied => rgb(0xfbbf24),
        _ => rgb(0xfca5a5),
    }
}

/// One note appended to a collapsed run's aggregate status.
fn outcome_note((condition, note): (ToolPartStatus, String)) -> gpui_kit::AnyElement {
    div()
        .text_color(outcome_note_color(condition))
        .child(note)
        .into_any()
}

/// An attributed history entry that is not a message to the agent.
///
/// Peer chat is a leading-edge, outlined bubble with a "Peer chat · not sent
/// to the agent" label, so it never reads as an agent turn (no accent bar) or
/// as the trailing-edge direction bubble. Run control and access changes are a
/// compact attributed line.
fn render_collaboration_entry(
    entry: &loom_core::CollaborationEntry,
    index: usize,
) -> gpui_kit::AnyElement {
    let heading = crate::collaboration::entry_heading(entry);
    let text = crate::collaboration::entry_text(entry);
    if matches!(
        entry.kind,
        loom_core::CollaborationEntryKind::PeerMessage { .. }
    ) {
        return div()
            .id(("peer-chat", index))
            .test_support()
            .w_full()
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .child(
                Bubble::new()
                    .alignment(MessageAlignment::Start)
                    .with_variant(BubbleVariant::Outline)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_xs().text_color(rgb(0x86efac)).child(heading))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child("Peer chat · not sent to the agent"),
                            ),
                    )
                    .child(div().mt_1().child(render_timeline_text(
                        format!("transcript-peer-{index}"),
                        text,
                        0xd1fae5,
                    ))),
            )
            .into_any();
    }
    div()
        .id(("collaboration-entry", index))
        .test_support()
        .px_3()
        .py_1()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .child(div().text_color(rgb(0x93c5fd)).child(heading))
        .child(div().text_color(rgb(0x94a3b8)).child(text))
        .into_any()
}
