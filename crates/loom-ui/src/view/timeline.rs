use super::*;

impl TimelineView {
    pub(crate) fn new(parent: Entity<LoomView>, scroller: Entity<MessageScrollerState>) -> Self {
        Self {
            parent,
            scroller,
            parent_subscription: None,
            scroller_subscription: None,
            session_id: None,
            timeline_revision: (0, 0),
        }
    }

    fn sync_list(
        &mut self,
        item_count: usize,
        timeline_revision: (usize, usize),
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.parent_subscription.is_none() {
            let parent = self.parent.clone();
            self.parent_subscription = Some(cx.observe(&parent, |_, _, cx| cx.notify()));
        }
        if self.scroller_subscription.is_none() {
            let scroller = self.scroller.clone();
            self.scroller_subscription = Some(cx.observe(&scroller, |_, _, cx| cx.notify()));
        }

        let (item_count, session_changed, timeline_revision, prepend_count) = {
            let parent_state = self.parent.read(cx);
            let item_count = parent_state.timeline.len();
            let session_id = parent_state.active_session.id;
            let session_changed = self.session_id != Some(session_id);
            if session_changed {
                self.session_id = Some(session_id);
            }
            let timeline_revision = (
                item_count,
                parent_state
                    .timeline
                    .last()
                    .map(|item| format!("{item:?}").len())
                    .unwrap_or_default(),
            );
            (
                item_count,
                session_changed,
                timeline_revision,
                parent_state.transcript_prepend_count,
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
        if item_count == 0 {
            div()
                .size_full()
                .p_6()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .w_full()
                                .max_w(TIMELINE_CONTENT_MAX_WIDTH)
                                .p_5()
                                .rounded_lg()
                                .bg(rgb(0x171c25))
                                .border_1()
                                .border_color(rgb(0x293244))
                                .text_size(gpui_kit::rems(14. / BASE_FONT_SIZE))
                                .text_color(rgb(0xb7c0d0))
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(16. / BASE_FONT_SIZE))
                                        .text_color(rgb(0xf3f4f6))
                                        .child("Ready when you are"),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .text_size(gpui_kit::rems(14. / BASE_FONT_SIZE))
                                        .text_color(rgb(0x8f98a6))
                                        .child("Describe a task below and Loom will keep the work, decisions, and results together."),
                                )
                                .child(
                                    div()
                                        .mt_3()
                                        .flex()
                                        .gap_3()
                                        .text_size(gpui_kit::rems(13. / BASE_FONT_SIZE))
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
        } else {
            // Load older pages automatically once the reader scrolls up to the
            // oldest loaded row, instead of surfacing a manual button. The row
            // renderer only runs for visible rows, so index 0 marks the top.
            let scrolled_up = self.scroller.read(cx).is_scrolled_up();
            let transcript_has_older = self.parent.read(cx).transcript_has_older;
            let autoload_older = transcript_has_older && scrolled_up;
            let parent_for_rows = self.parent.clone();
            let row_style = gpui_kit::StyleRefinement {
                padding: gpui_kit::EdgesRefinement {
                    top: Some(px(0.).into()),
                    right: Some(px(0.).into()),
                    bottom: Some(px(0.).into()),
                    left: Some(px(0.).into()),
                },
                ..Default::default()
            };
            let timeline = MessageScroller::new(
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
                    div()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .w_full()
                                .max_w(TIMELINE_CONTENT_MAX_WIDTH)
                                .child(view.render_timeline_item(item, index, &parent_for_rows)),
                        )
                        .into_any_element()
                },
            )
            .with_row_style(row_style)
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
