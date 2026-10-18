//! The Run tab: one compact view of the active run.
//!
//! The plan, token usage, and context budget used to be three separate tabs
//! that repeated the same session facts. They are run-scoped status about the
//! same thing, so they share one column here and only the figures that change
//! what the user would do next are shown.

use super::*;

use gpui_kit::component::progress::Progress;
use gpui_kit::component::tag::{Tag, TagVariant};
use loom_model::ProviderUsageSummary;

impl LoomView {
    pub(crate) fn render_inspector_run(
        &self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        inspector_scroll()
            .child(self.render_run_card())
            .child(self.render_plan_card(cx))
            .child(self.render_usage_card())
            .child(self.render_context_card(cx))
    }

    /// The run's state, model, task, and elapsed time.
    fn render_run_card(&self) -> impl IntoElement {
        let model = self
            .active_run
            .as_ref()
            .map(|run| run.model.as_str())
            .unwrap_or_else(|| self.model.as_str())
            .to_owned();
        let task = self
            .active_run
            .as_ref()
            .map(|run| run.task.clone())
            .or_else(|| {
                self.session_task_cache
                    .get(&self.active_session.id)
                    .cloned()
            });
        let mut body = div()
            .id("run-status")
            .test_support()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(status_tag(self.session_state))
                    .when_some(self.run_elapsed_ms(), |row, elapsed| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .font_family(mono_font())
                                .text_xs()
                                .text_color(rgb(0x8f98a6))
                                .child(format_elapsed_ms(elapsed)),
                        )
                    }),
            )
            .child(
                div()
                    .w_full()
                    .truncate()
                    .font_family(mono_font())
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child(model),
            );
        if let Some(task) = task {
            body = body.child(
                div()
                    .w_full()
                    .text_size(card_value_font())
                    .text_color(rgb(0xb7c0d0))
                    .child(task),
            );
        }
        card("Status", body)
    }

    /// Plan progress with the checklist behind a disclosure.
    fn render_plan_card(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let Some(plan) = self.plan.as_ref().filter(|plan| !plan.is_empty()) else {
            return card("Plan", empty_note("No plan for the active run.")).into_any_element();
        };
        let collapsed = self.plan_collapsed;
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
                        .text_size(card_value_font())
                        .text_color(text_color)
                        .child(
                            div()
                                .w(px(14.))
                                .flex_shrink_0()
                                .child(Icon::new(status.icon()).small().text_color(marker_color)),
                        )
                        .child(div().flex_1().min_w(px(0.)).child(step.clone())),
                );
            }
        }

        let view_handle = cx.entity().downgrade();
        let action = div()
            .flex()
            .items_center()
            .gap_2()
            .child(mono_note(format!("{done}/{total}")))
            .child(
                Button::new("toggle-plan")
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
                        view_handle
                            .update(cx, |view, cx| {
                                view.plan_collapsed = !view.plan_collapsed;
                                cx.notify();
                            })
                            .ok();
                    }),
            );

        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                Progress::new("plan-progress")
                    .value(plan.fraction() * 100.)
                    .xsmall()
                    .w_full()
                    .accessibility_label(format!("{done} of {total} plan steps complete")),
            )
            .child(metric_caption(plan_active_line(plan)))
            .child(
                div()
                    .id("plan-checklist")
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(steps),
            );
        card_with_action("Plan", action, body).into_any_element()
    }

    /// Token, cost, and duration accounting for the run, falling back to the
    /// session totals when no run is active.
    pub(crate) fn render_usage_card(&self) -> impl IntoElement {
        let mut body = div().flex().flex_col().gap_2();
        match self.review.usage.total() {
            Some(usage) => {
                body = body
                    .child(
                        metric_row()
                            .id("run-usage")
                            .test_support()
                            .child(metric(
                                "In / out",
                                format!(
                                    "{} / {}",
                                    format_tokens(usage.input_tokens),
                                    format_tokens(usage.output_tokens)
                                ),
                            ))
                            .child(metric(
                                "Cost",
                                usage_cost(self.review.usage.provider(), usage.cost_micros),
                            ))
                            .child(metric(
                                "Time",
                                if usage.elapsed_ms > 0 {
                                    format_elapsed_ms(usage.elapsed_ms)
                                } else {
                                    "-".to_owned()
                                },
                            )),
                    )
                    .when_some(usage_caption(&self.review.usage, usage), |body, caption| {
                        body.child(metric_caption(caption))
                    });
            }
            None if self.review.usage.loading => {
                body = body.child(empty_note("Loading usage…"));
            }
            None => body = body.child(empty_note("No usage recorded for this session.")),
        }
        if let Some(error) = &self.review.usage.error {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        }
        card("Usage", body)
    }

    /// The context the next request would carry, as a budget bar.
    fn render_context_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let refresh = Button::new("refresh-context")
            .icon(Icon::new(IconName::RefreshCw))
            .ghost()
            .xsmall()
            .tooltip("Refresh context")
            .accessibility_label("Refresh context")
            .disabled(self.active_run_id.is_none())
            .on_click(cx.listener(|this, _, _, cx| this.refresh_context(cx)));
        let body: gpui_kit::AnyElement = match self.context_inspection.as_ref() {
            Some(inspection) => {
                let budget = inspection.budget.effective_input_tokens;
                let over_budget = budget.is_some_and(|budget| inspection.included_tokens > budget);
                let fraction = budget
                    .filter(|budget| *budget > 0)
                    .map(|budget| (inspection.included_tokens as f32 / budget as f32).clamp(0., 1.))
                    .unwrap_or(0.);
                let mut body = div()
                    .id("run-context")
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        metric_row()
                            .child(metric(
                                "Used",
                                format!("~{}", format_tokens(inspection.included_tokens)),
                            ))
                            .child(metric(
                                "Budget",
                                budget
                                    .map(format_tokens)
                                    .unwrap_or_else(|| "unknown".to_owned()),
                            )),
                    )
                    .child(
                        Progress::new("context-budget")
                            .value(fraction * 100.)
                            .xsmall()
                            .w_full()
                            .color(if over_budget {
                                rgb(0xfca5a5)
                            } else {
                                rgb(0x60a5fa)
                            })
                            .accessibility_label("Context budget used"),
                    )
                    .child(metric_caption(format!(
                        "{} omitted · {} reserved for output",
                        format_tokens(inspection.omitted_tokens),
                        format_tokens(inspection.budget.reserved_output_tokens)
                    )));
                if over_budget {
                    body = body.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0xfca5a5))
                            .child("Over the model's input budget."),
                    );
                }
                body.into_any_element()
            }
            None => empty_note("No context inspection for this run.").into_any_element(),
        };
        card_with_action("Context", refresh, body)
    }

    /// How long the active run has been running, from usage when the backend
    /// reported it and from the run timestamps otherwise.
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

/// A status pill for the session state, using the component tag so the colors
/// follow the active theme.
fn status_tag(state: AgentSessionState) -> Tag {
    let variant = match state {
        AgentSessionState::Executing
        | AgentSessionState::Planning
        | AgentSessionState::Evaluating
        | AgentSessionState::Queued => TagVariant::Info,
        AgentSessionState::AwaitingApproval
        | AgentSessionState::NeedsInput
        | AgentSessionState::Paused => TagVariant::Warning,
        AgentSessionState::Completed => TagVariant::Success,
        AgentSessionState::Failed | AgentSessionState::Cancelled => TagVariant::Danger,
        AgentSessionState::Idle | AgentSessionState::Archived => TagVariant::Secondary,
    };
    Tag::new()
        .with_variant(variant)
        .outline()
        .xsmall()
        .child(session_status_label(state))
}

/// The one-line plan status shown under the progress bar.
fn plan_active_line(plan: &PlanState) -> String {
    match plan.active_step() {
        Some(step) => format!("Now: {step}"),
        None if plan.done_count() >= plan.total() => "All steps complete".to_owned(),
        None => "Waiting to start".to_owned(),
    }
}

/// A cost string for the usage card, or a dash when no figure was recorded.
fn usage_cost(provider: Option<&ProviderUsageSummary>, cost_micros: u64) -> String {
    let known = match provider {
        Some(provider) => Some(provider.cost_micros),
        None => (cost_micros > 0).then_some(cost_micros),
    };
    known
        .map(format_cost_micros)
        .unwrap_or_else(|| "-".to_owned())
}

/// The secondary counts that would otherwise need their own cards.
fn usage_caption(state: &UsageState, usage: &UsageSnapshot) -> Option<String> {
    let mut parts = Vec::new();
    if usage.tool_calls > 0 {
        parts.push(format!("{} tool calls", usage.tool_calls));
    }
    if usage.cached_input_tokens > 0 {
        parts.push(format!(
            "{} cached",
            format_tokens(usage.cached_input_tokens)
        ));
    }
    if let Some(provider) = state.provider() {
        parts.push(format!("{} requests", provider.requests));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}
