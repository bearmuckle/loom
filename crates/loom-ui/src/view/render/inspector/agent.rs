use super::*;

impl LoomView {
    pub(crate) fn render_inspector_agent(
        &self,
        _window: &Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut content = inspector_scroll();

        let mut session_rows = div().flex().flex_col().gap_2();
        session_rows = session_rows
            .child(info_row(
                "Session",
                div().truncate().child(self.active_session.name.clone()),
            ))
            .child(info_row(
                "State",
                div().child(session_status_label(self.session_state).to_owned()),
            ))
            .child(info_row(
                "Model",
                div().truncate().font_family(mono_font()).child(
                    self.active_run
                        .as_ref()
                        .map(|run| run.model.as_str())
                        .unwrap_or_else(|| self.model.as_str())
                        .to_owned(),
                ),
            ));
        if let Some(task) = self
            .active_run
            .as_ref()
            .map(|run| run.task.clone())
            .or_else(|| {
                self.session_task_cache
                    .get(&self.active_session.id)
                    .cloned()
            })
        {
            session_rows = session_rows.child(info_row("Task", div().child(task)));
        }
        if let Some(elapsed) = self.run_elapsed_ms() {
            session_rows = session_rows.child(info_row("Elapsed", format_elapsed_ms(elapsed)));
        }
        if let Some(plan) = self.plan.as_ref().filter(|plan| !plan.is_empty()) {
            session_rows = session_rows.child(info_row("Plan", plan_summary_line(plan)));
        }
        content = content.child(card("Run", session_rows));

        content = content.child(self.render_usage_card());
        content = content.child(self.render_agent_tools_card());
        content = content.child(self.render_agent_evidence_card());

        content
    }

    fn render_agent_tools_card(&self) -> impl IntoElement {
        let mut counts = BTreeMap::<&'static str, usize>::new();
        for item in &self.timeline {
            if let TimelineItem::Assistant(turn) = item {
                for part in &turn.parts {
                    if let AssistantPart::Tool(tool) = part {
                        *counts.entry(tool.status.label()).or_default() += 1;
                    }
                }
            }
        }
        let body = if counts.is_empty() {
            empty_note("No tool activity recorded for this session.").into_any_element()
        } else {
            let mut list = div().flex().flex_col().gap_1();
            for (label, count) in counts {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .text_size(card_value_font())
                        .child(div().text_color(rgb(0xb7c0d0)).child(label))
                        .child(
                            div()
                                .font_family(mono_font())
                                .text_color(rgb(0xe5e7eb))
                                .child(count.to_string()),
                        ),
                );
            }
            list.into_any_element()
        };
        card("Tool activity", body)
    }

    fn render_agent_evidence_card(&self) -> impl IntoElement {
        let evidence = self
            .active_run
            .as_ref()
            .map(|run| run.evidence.clone())
            .unwrap_or_default();
        let body = if evidence.is_empty() {
            empty_note("No evidence links attached.").into_any_element()
        } else {
            let mut list = div().flex().flex_col().gap_1();
            for link in evidence {
                list = list.child(
                    div()
                        .flex()
                        .items_start()
                        .gap_2()
                        .text_size(card_value_font())
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(0x93c5fd))
                                .child(link.label.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(rgb(0x8f98a6))
                                .child(link.uri),
                        ),
                );
            }
            list.into_any_element()
        };
        card("Evidence", body)
    }

    fn run_elapsed_ms(&self) -> Option<u64> {
        if let Some(usage) = self.review.usage.run.as_ref()
            && usage.elapsed_ms > 0
        {
            return Some(usage.elapsed_ms);
        }
        let run = self.active_run.as_ref()?;
        let end = run
            .completed_at
            .map(|timestamp| timestamp.as_unix_millis())
            .unwrap_or_else(|| Timestamp::now().as_unix_millis());
        Some(end.saturating_sub(run.started_at.as_unix_millis()))
    }
}

/// A single-line plan summary for the Run card: completed count, the active
/// step, and a mini progress bar. The full checklist lives in the Plan tab.
fn plan_summary_line(plan: &PlanState) -> impl IntoElement {
    let detail = if let Some(step) = plan.active_step() {
        format!("active: {step}")
    } else if plan.done_count() >= plan.total() {
        "complete".to_owned()
    } else {
        "not started".to_owned()
    };
    div()
        .id("run-plan-summary")
        .test_support()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_shrink_0()
                .font_family(mono_font())
                .child(format!("{}/{}", plan.done_count(), plan.total())),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(rgb(0x8f98a6))
                .child(detail),
        )
        .child(
            div()
                .flex_shrink_0()
                .w(px(54.))
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
}
