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
        let mut part_index = 0;
        while part_index < turn.parts.len() {
            match &turn.parts[part_index] {
                AssistantPart::Reasoning(text) => {
                    if !text.trim().is_empty() {
                        let key = tool_element_id(index, part_index);
                        let expanded = self.expanded_reasoning.contains(&key);
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
                                        parent_for_toggle
                                            .update(cx, |this, cx| this.toggle_reasoning(key, cx));
                                    })
                                    .child(
                                        div()
                                            .w_full()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .text_xs()
                                            .child(
                                                div()
                                                    .text_color(rgb(0x64748b))
                                                    .child(if expanded { "⌄" } else { "›" }),
                                            )
                                            .child(
                                                div().text_color(rgb(0x94a3b8)).child("Reasoning"),
                                            )
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
                            block = block.child(div().mt_1().child(render_timeline_text(
                                format!("transcript-reasoning-{index}-{part_index}"),
                                text.clone(),
                                0x94a3b8,
                            )));
                        }
                        body = body.child(block);
                    }
                    part_index += 1;
                }
                AssistantPart::Text(text) => {
                    if !text.trim().is_empty() {
                        let mut display = text.clone();
                        if turn.streaming && part_index + 1 == turn.parts.len() {
                            display.push('▍');
                        }
                        body = body.child(render_timeline_text(
                            format!("transcript-assistant-{index}-{part_index}"),
                            display,
                            0xf3f4f6,
                        ));
                    }
                    part_index += 1;
                }
                AssistantPart::Tool(part) => {
                    let mut end = part_index + 1;
                    while end < turn.parts.len() {
                        match &turn.parts[end] {
                            AssistantPart::Tool(next) if next.name == part.name => end += 1,
                            _ => break,
                        }
                    }
                    if end - part_index >= TOOL_GROUP_THRESHOLD {
                        body = body.child(self.render_tool_group(
                            &turn.parts[part_index..end],
                            index,
                            part_index,
                            parent,
                        ));
                    } else {
                        for offset in part_index..end {
                            if let Some(AssistantPart::Tool(part)) = turn.parts.get(offset) {
                                body =
                                    body.child(self.render_tool_part(part, index, offset, parent));
                            }
                        }
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

    /// One collapsed row for a run of same-kind tool calls, expandable to the
    /// individual blocks.
    pub(crate) fn render_tool_group(
        &self,
        parts: &[AssistantPart],
        index: usize,
        first_part_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let tools = parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Tool(tool) => Some(tool.as_ref()),
                _ => None,
            })
            .collect::<Vec<&ToolPart>>();
        let Some(first) = tools.first() else {
            return div().into_any();
        };
        let failed = tools
            .iter()
            .any(|tool| tool.status == ToolPartStatus::Failed);
        let running = tools.iter().any(|tool| {
            matches!(
                tool.status,
                ToolPartStatus::Running
                    | ToolPartStatus::AwaitingApproval
                    | ToolPartStatus::AwaitingInput
            )
        });
        let status_color = if failed {
            rgb(0xfca5a5)
        } else if running {
            rgb(0x93c5fd)
        } else {
            rgb(0x9ad7bd)
        };
        let status_label = if failed {
            "failed"
        } else if running {
            "running"
        } else {
            "done"
        };
        let key = tool_element_id(index, first_part_index);
        let expanded = running || failed || self.expanded_tool_groups.contains(&key);
        let total_ms = tools.iter().filter_map(|tool| tool.elapsed_ms).sum::<u64>();
        let duration = (total_ms > 0).then(|| format_duration(total_ms));
        let label = tool_group_label(&first.name, tools.len());
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
                    .accessibility_label(format!("{label}, {status_label}"))
                    .on_click(move |_, _, cx| {
                        parent_for_toggle.update(cx, |this, cx| this.toggle_tool_group(key, cx));
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
                            .child(div().text_color(rgb(0x64748b)).child(if expanded {
                                "⌄"
                            } else {
                                "›"
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(rgb(0xdbeafe))
                                    .child(label),
                            )
                            .child(div().text_color(status_color).child(status_label))
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
            ToolPartStatus::Completed => rgb(0x9ad7bd),
            ToolPartStatus::AwaitingApproval | ToolPartStatus::AwaitingInput => rgb(0xfef3c7),
            ToolPartStatus::Running => rgb(0x93c5fd),
            ToolPartStatus::Queued | ToolPartStatus::Cancelled => rgb(0x94a3b8),
        };
        let duration = part.elapsed_ms.map(format_duration);
        // Active, failed, and approval-gated work stays open; finished successes
        // collapse so the transcript stays scannable.
        let expanded = self.expanded_tools.contains(&part.id)
            || matches!(
                part.status,
                ToolPartStatus::Running
                    | ToolPartStatus::AwaitingApproval
                    | ToolPartStatus::AwaitingInput
                    | ToolPartStatus::Failed
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
                        parent_for_toggle.update(cx, |this, cx| this.toggle_tool(call_id, cx));
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
                            .child(div().text_color(rgb(0x64748b)).child(if expanded {
                                "⌄"
                            } else {
                                "›"
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_family(mono_font())
                                    .text_color(rgb(0xdbeafe))
                                    .child(part.title.clone()),
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
        if expanded {
            if let Some(detail) = &part.detail {
                block = block.child(
                    div()
                        .ml(px(20.))
                        .pt_1()
                        .font_family(mono_font())
                        .text_size(gpui_kit::rems(mono_size() / BASE_FONT_SIZE))
                        .text_color(rgb(0x94a3b8))
                        .child(detail.clone()),
                );
            }
            if part.output.is_some() {
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
                                            let output = part.output.clone().unwrap_or_default();
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
        if awaiting && !self.approval_request_in_flight {
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
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        match item {
            TimelineItem::User(text) => div()
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(3.)).h(px(13.)).rounded_full().bg(rgb(0x60a5fa)))
                        .child(div().text_xs().text_color(rgb(0xbfdbfe)).child("You")),
                )
                .child(div().mt_1().child(render_timeline_text(
                    format!("transcript-user-{index}"),
                    text.clone(),
                    0xf3f4f6,
                )))
                .into_any(),
            TimelineItem::Assistant(turn) => self.render_assistant_turn(turn, index, parent),
            TimelineItem::System(note) => {
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
                            .child("This operation can be retried."),
                    );
                }
                card.into_any()
            }
            TimelineItem::ProjectMessageContext(text) => div()
                .mx_3()
                .my_1()
                .px_3()
                .py_2()
                .border_l_2()
                .border_color(rgb(0x3b4555))
                .text_color(rgb(0xcbd5e1))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("Project agent context"),
                )
                .child(render_timeline_text(
                    format!("transcript-project-context-{index}"),
                    text.clone(),
                    0xcbd5e1,
                ))
                .into_any(),
            TimelineItem::ProjectMessage(message) => {
                let kind = match message.kind {
                    loom_core::AgentMessageKind::Progress => "Progress",
                    loom_core::AgentMessageKind::Result => "Result",
                    loom_core::AgentMessageKind::Question => "Question",
                    loom_core::AgentMessageKind::Blocker => "Blocker",
                    loom_core::AgentMessageKind::Direction => "Direction",
                    loom_core::AgentMessageKind::Answer => "Answer",
                };
                let participant = |session_id: AgentSessionId| {
                    self.sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| session.name.clone())
                        .unwrap_or_else(|| session_id.to_string())
                };
                let accent = if message.kind == loom_core::AgentMessageKind::Blocker {
                    rgb(0xfbbf24)
                } else if message.kind == loom_core::AgentMessageKind::Result {
                    rgb(0x86efac)
                } else {
                    rgb(0x93c5fd)
                };
                let mut card = div()
                    .mx_3()
                    .my_1()
                    .px_3()
                    .py_1()
                    .border_l_2()
                    .border_color(accent.opacity(0.45))
                    .text_color(rgb(0xb7c0d0))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(format!(
                        "{kind} · {} → {} · #{}",
                        participant(message.sender_session_id),
                        participant(message.target_session_id),
                        message.project_sequence
                    )));
                if !message.body.trim().is_empty() {
                    card = card.child(render_timeline_text(
                        format!("project-message-{}", message.message_id),
                        message.body.clone(),
                        0xb7c0d0,
                    ));
                }
                card.into_any()
            }
            TimelineItem::Plan {
                steps,
                completed,
                active,
            } => {
                let mut card = div()
                    .px_3()
                    .py_2()
                    .text_color(rgb(0xb7c0d0))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Plan"));
                for (index, step) in steps.iter().enumerate() {
                    let index = index as u32;
                    let marker = if completed.contains(&index) {
                        "✓"
                    } else if active == &Some(index) {
                        ">"
                    } else {
                        "○"
                    };
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(if completed.contains(&index) {
                                rgb(0x9ad7bd)
                            } else {
                                rgb(0xb7c0d0)
                            })
                            .child(format!("{marker} {}", step)),
                    );
                }
                card.into_any()
            }
        }
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

    /// The single line that carries run progress: spinner, state, elapsed, and
    /// the interrupt hint. Run state lives here instead of the session header.
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
            row = row.child(Spinner::new().small().color(color.into()));
        }
        if let Some(state) = self.run_state {
            let label = if state == AgentRunState::Paused && self.project_has_live_children() {
                "Waiting for sub-agents".to_owned()
            } else {
                run_state_label(Some(state)).to_owned()
            };
            row = row.child(div().text_color(run_state_color(state)).child(label));
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
