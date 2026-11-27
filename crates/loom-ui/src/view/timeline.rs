use super::*;

impl TimelineView {
    pub(crate) fn new(parent: Entity<LoomView>, scroller: Entity<TimelineScrollerState>) -> Self {
        Self {
            parent,
            scroller,
            parent_subscription: None,
            scroller_subscription: None,
            session_id: None,
            timeline_revision: TimelineRevision::default(),
        }
    }

    fn sync_list(
        &mut self,
        item_count: usize,
        timeline_revision: TimelineRevision,
        session_changed: bool,
        prepend_count: usize,
        cx: &mut Context<Self>,
    ) {
        let content_changed = self.timeline_revision != timeline_revision;
        self.timeline_revision = timeline_revision;
        let current = self.scroller.read(cx).item_count();
        if session_changed || item_count < current {
            self.scroller
                .update(cx, |state, cx| state.reset(item_count, cx));
        } else if item_count > current {
            let added = item_count - current;
            self.scroller.update(cx, |state, cx| {
                if prepend_count > 0 {
                    state.prepend(added, cx);
                } else {
                    state.append(added, cx);
                }
            });
        } else if content_changed {
            self.scroller.update(cx, |state, cx| state.remeasure(cx));
        }
    }
}

impl Render for TimelineView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.parent_subscription.is_none() {
            let parent = self.parent.clone();
            self.parent_subscription = Some(cx.observe(&parent, |_, _, cx| cx.notify()));
        }
        if self.scroller_subscription.is_none() {
            let scroller = self.scroller.clone();
            self.scroller_subscription = Some(cx.observe(&scroller, |_, _, cx| cx.notify()));
        }

        let (item_count, session_changed, timeline_revision, prepend_count, conversation_opening) = {
            let parent_state = self.parent.read(cx);
            let item_count = parent_state.timeline.len();
            let session_id = parent_state.active_session.id;
            let session_changed = self.session_id != Some(session_id);
            if session_changed {
                self.session_id = Some(session_id);
            }
            let timeline_revision = TimelineRevision {
                item_count,
                last_item_len: parent_state
                    .timeline
                    .last()
                    .map(|item| format!("{item:?}").len())
                    .unwrap_or_default(),
                // The detail level and the reasoning switch change how tall a
                // row renders without changing its content, so they belong in
                // the revision: a change re-measures the rows instead of
                // leaving the virtual list on the old heights.
                output_detail: parent_state.output_detail,
                show_reasoning: parent_state.show_reasoning,
            };
            (
                item_count,
                session_changed,
                timeline_revision,
                parent_state.transcript_prepend_count,
                parent_state.conversation_opening,
            )
        };
        self.sync_list(
            item_count,
            timeline_revision,
            session_changed,
            prepend_count,
            cx,
        );
        if prepend_count > 0 {
            self.parent
                .update(cx, |view, _| view.transcript_prepend_count = 0);
        }
        if item_count == 0 && !conversation_opening {
            div()
                .id("timeline-empty")
                .test_support()
                .role(Role::Log)
                .aria_label("Session transcript")
                .size_full()
                .p_6()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .id("timeline-empty-status")
                                .test_support()
                                .role(Role::Status)
                                .aria_label("Ready when you are")
                                .w_full()
                                .max_w(TIMELINE_CONTENT_MAX_WIDTH)
                                .p_5()
                                .rounded_lg()
                                .bg(rgb(0x171c25))
                                .border_1()
                                .border_color(rgb(0x293244))
                                .text_size(gpui_kit::rems(13. / BASE_FONT_SIZE))
                                .text_color(rgb(0xb7c0d0))
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(15. / BASE_FONT_SIZE))
                                        .text_color(rgb(0xf3f4f6))
                                        .child("Ready when you are"),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .text_size(gpui_kit::rems(13. / BASE_FONT_SIZE))
                                        .text_color(rgb(0x8f98a6))
                                        .child("Describe a task below and Loom will keep the work, decisions, and results together."),
                                )
                                .child(
                                    div()
                                        .mt_3()
                                        .flex()
                                        .gap_3()
                                        .text_size(gpui_kit::rems(12. / BASE_FONT_SIZE))
                                        .text_color(rgb(0x64748b))
                                        .child("/ commands")
                                        .child("@ files")
                                        .child(format!(
                                            "{} palette",
                                            command_palette_shortcut_label()
                                        )),
                                ),
                        ),
                )
                .into_any_element()
        } else if conversation_opening {
            // The open session's conversation is still being rebuilt, so this
            // paints one neutral frame instead of each partial transcript the
            // reopen ladder applies. The list above is still synced, so the
            // final frame lands on the scroller state the whole roster implies.
            div()
                .id("timeline-opening-log")
                .test_support()
                .role(Role::Log)
                .aria_label("Session transcript")
                .size_full()
                .p_6()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .id("timeline-opening")
                                .test_support()
                                .role(Role::Status)
                                .aria_label("Opening the conversation")
                                .w_full()
                                .max_w(TIMELINE_CONTENT_MAX_WIDTH)
                                .p_5()
                                .rounded_lg()
                                .bg(rgb(0x171c25))
                                .border_1()
                                .border_color(rgb(0x293244))
                                .text_size(gpui_kit::rems(13. / BASE_FONT_SIZE))
                                .text_color(rgb(0xb7c0d0))
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(15. / BASE_FONT_SIZE))
                                        .text_color(rgb(0xf3f4f6))
                                        .child("Opening the conversation"),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .text_size(gpui_kit::rems(13. / BASE_FONT_SIZE))
                                        .text_color(rgb(0x8f98a6))
                                        .child("Reading the transcript. Loom shows it once the whole conversation is loaded."),
                                ),
                        ),
                )
                .into_any_element()
        } else {
            // Load older pages automatically once the reader scrolls up to the
            // oldest loaded row, instead of surfacing a manual button. The row
            // renderer only runs for visible rows, so index 0 marks the top.
            let scrolled_up = self.scroller.read(cx).is_scrolled_up();
            let transcript_has_older = self.parent.read(cx).transcript_has_older;
            let autoload_older = transcript_has_older && scrolled_up;
            // The row renderer needs the layout so a question component can
            // pick finger-sized controls on a phone.
            let layout = responsive_layout(window.bounds().size.width);
            let parent_for_rows = self.parent.clone();
            let timeline = TimelineScroller::new(
                "timeline-scroller",
                self.scroller.clone(),
                move |index, _window, cx| {
                    if index == 0 && autoload_older {
                        let parent = parent_for_rows.clone();
                        cx.defer(move |cx| {
                            parent.update(cx, |view, cx| view.autoload_older_transcript(cx));
                        });
                    }
                    let view = parent_for_rows.read(cx);
                    let item = &view.timeline[index];
                    let accessible_name = view.timeline_item_accessible_name(item);
                    div()
                        .id(("timeline-item", index))
                        .test_support()
                        .role(Role::Article)
                        .aria_label(accessible_name)
                        .aria_position_in_set(index)
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(div().w_full().max_w(TIMELINE_CONTENT_MAX_WIDTH).child(
                            view.render_timeline_item(item, index, layout, &parent_for_rows),
                        ))
                        .into_any_element()
                },
            )
            .with_jump_button_label("Jump to latest")
            .with_bottom_fade(gpui_kit::Hsla::from(rgb(0x111318)));
            div()
                .size_full()
                .relative()
                .child(timeline)
                .into_any_element()
        }
    }
}

/// Maximum number of characters included in the summary portion of a transcript
/// entry's accessible name.
const ACCESSIBLE_PREVIEW_MAX_CHARS: usize = 120;

/// Speaker label shown on a user message row.
pub(crate) fn user_message_speaker(
    text: &str,
    collaboration: &crate::collaboration::CollaborationState,
) -> String {
    match collaboration.author_of(text) {
        Some(author @ loom_core::Author::Participant { .. }) => {
            format!("{} -> Agent", crate::collaboration::author_label(author))
        }
        _ => "You".to_owned(),
    }
}

impl LoomView {
    /// Concise accessible name for one rendered transcript row, matching the
    /// parts currently visible under `output_detail`, `show_reasoning`, and
    /// recorded `ask_user` questions.
    pub(crate) fn timeline_item_accessible_name(&self, item: &TimelineItem) -> String {
        timeline_item_accessible_name_with(
            item,
            &self.collaboration,
            self.output_detail,
            self.show_reasoning,
            |part| self.question_for(part).map(|question| question.prompt),
        )
    }
}

/// Concise accessible name for one transcript row, combining speaker/kind with
/// a single-line preview of what the row actually paints.
pub(crate) fn timeline_item_accessible_name_with(
    item: &TimelineItem,
    collaboration: &crate::collaboration::CollaborationState,
    output_detail: OutputDetail,
    show_reasoning: bool,
    mut question_prompt: impl FnMut(&ToolPart) -> Option<String>,
) -> String {
    match item {
        TimelineItem::User(text) => {
            let speaker = user_message_speaker(text, collaboration);
            let preview = accessible_preview(text);
            if preview.is_empty() {
                speaker
            } else {
                format!("{speaker}: {preview}")
            }
        }
        TimelineItem::Assistant(turn) => assistant_turn_accessible_name(
            turn,
            output_detail,
            show_reasoning,
            &mut question_prompt,
        ),
        TimelineItem::Collaboration(entry) => {
            let heading = crate::collaboration::entry_heading(entry);
            let preview = accessible_preview(&crate::collaboration::entry_text(entry));
            let is_peer = matches!(
                entry.kind,
                loom_core::CollaborationEntryKind::PeerMessage { .. }
            );
            let speaker = if is_peer {
                format!("{heading} (Peer chat)")
            } else {
                heading
            };
            if preview.is_empty() {
                speaker
            } else {
                format!("{speaker}: {preview}")
            }
        }
        TimelineItem::System(note) => {
            let preview = accessible_preview(&note.text);
            match note
                .heading
                .as_deref()
                .map(str::trim)
                .filter(|heading| !heading.is_empty())
            {
                Some(heading) if !preview.is_empty() => format!("System: {heading}: {preview}"),
                Some(heading) => format!("System: {heading}"),
                None if !preview.is_empty() => format!("System: {preview}"),
                None => "System".to_owned(),
            }
        }
    }
}

fn assistant_turn_accessible_name(
    turn: &AssistantTurn,
    output_detail: OutputDetail,
    show_reasoning: bool,
    question_prompt: &mut impl FnMut(&ToolPart) -> Option<String>,
) -> String {
    if let Some(preview) = turn.parts.iter().find_map(|part| match part {
        AssistantPart::Text(text) => {
            let preview = accessible_preview(text);
            (!preview.is_empty()).then_some(preview)
        }
        _ => None,
    }) {
        return format!("Agent: {preview}");
    }
    if let Some(prompt) = turn.parts.iter().find_map(|part| match part {
        AssistantPart::Tool(tool) if tool.name == ASK_USER_TOOL => {
            let prompt = question_prompt(tool)
                .map(|text| accessible_preview(&text))
                .unwrap_or_default();
            Some(prompt)
        }
        _ => None,
    }) {
        return if prompt.is_empty() {
            "Agent asks".to_owned()
        } else {
            format!("Agent asks: {prompt}")
        };
    }
    let tools = run_tool_parts(&turn.parts)
        .into_iter()
        .filter(|tool| tool.name != ASK_USER_TOOL)
        .collect::<Vec<_>>();
    if !tools.is_empty() && (output_detail.shows_activity() || tool_needs_attention(&tools)) {
        let status = tool_group_status(&tools);
        return format!(
            "Agent: Tools used: {}, {}",
            tool_usage_summary(&tools),
            status.label()
        );
    }
    if output_detail.shows_activity()
        && show_reasoning
        && let Some(preview) = turn.parts.iter().find_map(|part| match part {
            AssistantPart::Reasoning(text) => {
                let preview = accessible_preview(text);
                (!preview.is_empty()).then_some(format!("Agent: Reasoning: {preview}"))
            }
            _ => None,
        })
    {
        return preview;
    }
    if turn
        .parts
        .iter()
        .any(|part| matches!(part, AssistantPart::Evidence(links) if !links.is_empty()))
    {
        return "Agent: Evidence".to_owned();
    }
    "Agent".to_owned()
}

pub(crate) fn accessible_preview(text: &str) -> String {
    let mut words = text.split_whitespace().peekable();
    let Some(first) = words.next() else {
        return String::new();
    };
    let first_len = first.chars().count();
    if first_len > ACCESSIBLE_PREVIEW_MAX_CHARS {
        let mut out: String = first
            .chars()
            .take(ACCESSIBLE_PREVIEW_MAX_CHARS - 3)
            .collect();
        out.push_str("...");
        return out;
    }
    let mut out = first.to_owned();
    let mut count = first_len;
    while let Some(word) = words.next() {
        let word_len = word.chars().count();
        let has_more = words.peek().is_some();
        let limit = if has_more {
            ACCESSIBLE_PREVIEW_MAX_CHARS - 3
        } else {
            ACCESSIBLE_PREVIEW_MAX_CHARS
        };
        if count + 1 + word_len <= limit {
            out.push(' ');
            out.push_str(word);
            count += 1 + word_len;
        } else {
            if count + 3 > ACCESSIBLE_PREVIEW_MAX_CHARS {
                out = out.chars().take(ACCESSIBLE_PREVIEW_MAX_CHARS - 3).collect();
            }
            out.push_str("...");
            return out;
        }
    }
    out
}
