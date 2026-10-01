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
        cx: &mut Context<Self>,
    ) {
        let content_changed = self.timeline_revision != timeline_revision;
        self.timeline_revision = timeline_revision;
        let current = self.scroller.read(cx).item_count();
        if session_changed || item_count < current {
            self.scroller
                .update(cx, |state, cx| state.reset(item_count, cx));
        } else if item_count > current {
            self.scroller
                .update(cx, |state, cx| state.append(item_count - current, cx));
        } else if content_changed {
            self.scroller.update(cx, |state, cx| state.remeasure(cx));
        }
    }

    /// The pinned plan banner shown above the transcript.
    ///
    /// A plan is run-scoped progress rather than a conversation message, so it
    /// stays out of the scrolling timeline while remaining visible as the user
    /// reads the conversation. The header collapses the step list.
    fn render_plan_banner(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let parent = self.parent.read(cx);
        let plan = parent.plan.as_ref().filter(|plan| !plan.is_empty())?;
        let collapsed = parent.plan_collapsed;
        let total = plan.total();
        let done = plan.done_count();

        let mut steps = Vec::new();
        if !collapsed {
            for (index, step) in plan.steps.iter().enumerate() {
                let status = plan.status(index as u32);
                let (text_color, marker_color) = match status {
                    PlanStepStatus::Done => (rgb(0x9ad7bd), rgb(0x86efac)),
                    PlanStepStatus::Active => (rgb(0xf3f4f6), rgb(0x93c5fd)),
                    PlanStepStatus::Pending => (rgb(0x8f98a6), rgb(0x64748b)),
                };
                steps.push(
                    div()
                        .id(("plan-step", index))
                        .test_support()
                        .flex()
                        .items_start()
                        .gap_2()
                        .text_size(gpui_kit::rems(12.5 / BASE_FONT_SIZE))
                        .text_color(text_color)
                        .child(
                            div()
                                .w(px(14.))
                                .flex_shrink_0()
                                .text_color(marker_color)
                                .child(status.marker()),
                        )
                        .child(div().flex_1().min_w(px(0.)).child(step.clone())),
                );
            }
        }

        let parent_for_toggle = self.parent.clone();
        Some(
            div()
                .w_full()
                .flex()
                .justify_center()
                .px_3()
                .pt_3()
                .child(
                    div()
                        .id("plan-banner")
                        .test_support()
                        .w_full()
                        .max_w(TIMELINE_CONTENT_MAX_WIDTH)
                        .flex_shrink_0()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(rgb(0x181c26))
                        .border_1()
                        .border_color(rgb(0x293244))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(11.5 / BASE_FONT_SIZE))
                                        .text_color(rgb(0x93c5fd))
                                        .child("Plan"),
                                )
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(11.5 / BASE_FONT_SIZE))
                                        .text_color(rgb(0x8f98a6))
                                        .child(format!("{done} / {total}")),
                                )
                                .child(div().flex_1())
                                .child(
                                    Button::new("toggle-plan-banner")
                                        .icon(Icon::new(if collapsed {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronUp
                                        }))
                                        .ghost()
                                        .xsmall()
                                        .tooltip(if collapsed {
                                            "Expand plan"
                                        } else {
                                            "Collapse plan"
                                        })
                                        .on_click(move |_, _, cx| {
                                            parent_for_toggle.update(cx, |view, cx| {
                                                view.plan_collapsed = !view.plan_collapsed;
                                                cx.notify();
                                            });
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(px(4.))
                                .rounded_full()
                                .bg(rgb(0x293244))
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .w(gpui_kit::relative(plan.fraction()))
                                        .bg(rgb(0x60a5fa)),
                                ),
                        )
                        .child(
                            div()
                                .id("plan-banner-steps")
                                .flex()
                                .flex_col()
                                .gap_1()
                                .max_h(px(220.))
                                .overflow_y_scroll()
                                .children(steps),
                        ),
                )
                .into_any_element(),
        )
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

        let (item_count, session_changed, timeline_revision) = {
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
            (item_count, session_changed, timeline_revision)
        };
        self.sync_list(item_count, timeline_revision, session_changed, cx);
        let plan_banner = self.render_plan_banner(cx);
        let body = if item_count == 0 {
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
            let (transcript_has_older, transcript_loading) = {
                let parent_state = self.parent.read(cx);
                (
                    parent_state.transcript_has_older,
                    parent_state.transcript_loading,
                )
            };
            let parent = self.parent.clone();
            let parent_for_rows = parent.clone();
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
            // Only surface paging once we know there is more history. The
            // automatic first-page load on session select reuses
            // `transcript_loading` but must not flash a "Loading older messages"
            // control.
            let content = if transcript_has_older {
                let parent_for_page = parent.clone();
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(
                        div().w_full().flex().justify_center().p_2().child(
                            Button::new("load-older-transcript")
                                .label(if transcript_loading {
                                    "Loading older messages…"
                                } else {
                                    "Load older messages"
                                })
                                .small()
                                .disabled(transcript_loading)
                                .on_click(move |_, _, cx| {
                                    parent_for_page.update(cx, |view, cx| {
                                        view.begin_transcript_page(
                                            view.transcript_before_ordinal,
                                            cx,
                                        );
                                    });
                                }),
                        ),
                    )
                    .child(div().flex_1().min_h_0().child(timeline))
                    .into_any()
            } else {
                timeline.into_any_element()
            };
            div()
                .size_full()
                .relative()
                .child(content)
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(plan_banner, |element, banner| element.child(banner))
            .child(div().flex_1().min_h(px(0.)).child(body))
    }
}
