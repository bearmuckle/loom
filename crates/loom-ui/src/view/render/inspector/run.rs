//! The Run tab: one compact view of the active run.
//!
//! The plan, token usage, and context budget used to be three separate tabs
//! that repeated the same session facts. They are run-scoped status about the
//! same thing, so they share one column here and only the figures that change
//! what the user would do next are shown.

use super::*;

use gpui_kit::component::progress::Progress;
use gpui_kit::component::tag::{Tag, TagVariant};
use loom_model::{ProviderUsageSummary, UsageLimitWindow, UsageLimits};

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
            .when_some(self.review.usage.limits.as_ref(), |column, limits| {
                column.child(render_limits_card(limits))
            })
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

/// The account plan limits the provider last reported, one bar per window.
fn render_limits_card(limits: &UsageLimits) -> impl IntoElement {
    let now_secs = Timestamp::now().as_unix_millis() / 1_000;
    let mut body = div()
        .id("plan-limits")
        .test_support()
        .flex()
        .flex_col()
        .gap_2();
    for (index, window) in limits.windows.iter().enumerate() {
        let label = limit_window_label(&window.name);
        let mut row = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(metric_caption(format!(
                "{label} · {}",
                limit_window_caption(window, now_secs)
            )));
        // A window that reset since the report no longer has this usage, so
        // only its caption remains.
        let reset = window.resets_at.is_some_and(|at| at <= now_secs);
        if let Some(percent) = window.used_percent.filter(|_| !reset) {
            row = row.child(
                Progress::new(("plan-limit", index))
                    .value(percent.min(100) as f32)
                    .xsmall()
                    .w_full()
                    .accessibility_label(format!("{label} limit {percent}% used")),
            );
        }
        body = body.child(row);
    }
    if let Some(note) = limits_note(limits) {
        body = body.child(div().text_xs().text_color(rgb(0xfca5a5)).child(note));
    }
    if let Some(age) = limits_age_caption(limits, now_secs) {
        body = body.child(metric_caption(age));
    }
    card("Plan limits", body)
}

/// How old stored limits are, once they are older than a minute. Live limits
/// arrive during a run, so only a kept copy from an earlier run says this.
fn limits_age_caption(limits: &UsageLimits, now_secs: u64) -> Option<String> {
    let age = now_secs.checked_sub(limits.reported_at?)?;
    (age >= 60).then(|| {
        format!(
            "Reported {} ago",
            format_elapsed_ms(age.saturating_mul(1_000))
        )
    })
}

/// A readable name for a provider limit window.
fn limit_window_label(name: &str) -> String {
    match name {
        "five_hour" => "5-hour".to_owned(),
        "seven_day" => "Weekly".to_owned(),
        "seven_day_opus" => "Weekly (Opus)".to_owned(),
        "seven_day_sonnet" => "Weekly (Sonnet)".to_owned(),
        other => other.replace('_', " "),
    }
}

/// The used share and reset time of one limit window.
fn limit_window_caption(window: &UsageLimitWindow, now_secs: u64) -> String {
    let used = window
        .used_percent
        .map(|percent| format!("{percent}% used"))
        .unwrap_or_else(|| "usage unknown".to_owned());
    match window.resets_at {
        Some(at) if at > now_secs => format!(
            "{used} · resets in {}",
            format_elapsed_ms((at - now_secs).saturating_mul(1_000))
        ),
        Some(_) => format!("{used} · resetting"),
        None => used,
    }
}

/// A warning when the account is limited or drawing on overage.
fn limits_note(limits: &UsageLimits) -> Option<String> {
    match limits.status.as_str() {
        "rejected" => Some("Plan limit reached.".to_owned()),
        "allowed_warning" => Some("Approaching the plan limit.".to_owned()),
        _ if limits.using_overage => Some("Using paid overage.".to_owned()),
        _ => None,
    }
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

#[cfg(test)]
mod limit_tests {
    use super::*;

    #[test]
    fn limit_windows_read_as_names_shares_and_reset_times() {
        assert_eq!(limit_window_label("five_hour"), "5-hour");
        assert_eq!(limit_window_label("seven_day"), "Weekly");
        assert_eq!(limit_window_label("seven_day_opus"), "Weekly (Opus)");
        assert_eq!(limit_window_label("new_kind"), "new kind");
        let window = |used_percent, resets_at| UsageLimitWindow {
            name: "five_hour".to_owned(),
            used_percent,
            resets_at,
            status: None,
        };
        assert_eq!(
            limit_window_caption(&window(Some(7), Some(1_000 + 3 * 3_600 + 720)), 1_000),
            "7% used · resets in 3h 12m"
        );
        assert_eq!(
            limit_window_caption(&window(Some(100), Some(10)), 20),
            "100% used · resetting"
        );
        assert_eq!(
            limit_window_caption(&window(None, None), 0),
            "usage unknown"
        );
    }

    #[test]
    fn stored_limits_say_how_old_they_are() {
        let limits = |reported_at| UsageLimits {
            status: "allowed".to_owned(),
            windows: Vec::new(),
            using_overage: false,
            reported_at,
        };
        assert_eq!(limits_age_caption(&limits(None), 1_000), None);
        assert_eq!(limits_age_caption(&limits(Some(990)), 1_000), None);
        assert_eq!(limits_age_caption(&limits(Some(2_000)), 1_000), None);
        assert_eq!(
            limits_age_caption(&limits(Some(1_000)), 1_000 + 2 * 3_600 + 300).as_deref(),
            Some("Reported 2h 5m ago")
        );
    }

    #[test]
    fn the_newest_stored_limits_fill_the_card_unless_live_ones_are_newer() {
        let limits = |status: &str, reported_at| UsageLimits {
            status: status.to_owned(),
            windows: Vec::new(),
            using_overage: false,
            reported_at: Some(reported_at),
        };
        let provider = |id: &str, usage_limits| loom_model::ProviderSummary {
            id: loom_model::ProviderId::new(id),
            kind: loom_model::ProviderKind::ClaudeCode,
            display_name: id.to_owned(),
            models: Vec::new(),
            credential_id: None,
            api_key_configurable: false,
            health: loom_model::ProviderHealth::default(),
            usage_limits,
        };
        let providers = vec![
            provider("none", None),
            provider("older", Some(limits("older", 10))),
            provider("newer", Some(limits("newer", 20))),
        ];
        let mut usage = crate::state::UsageState::default();
        usage.adopt_stored_limits(&[provider("none", None)]);
        assert_eq!(usage.limits, None);
        usage.adopt_stored_limits(&providers);
        assert_eq!(
            usage.limits.as_ref().map(|l| l.status.as_str()),
            Some("newer")
        );
        usage.limits = Some(limits("live", 30));
        usage.adopt_stored_limits(&providers);
        assert_eq!(
            usage.limits.as_ref().map(|l| l.status.as_str()),
            Some("live")
        );
    }

    #[test]
    fn limit_notes_warn_only_when_the_account_is_constrained() {
        let limits = |status: &str, using_overage| UsageLimits {
            status: status.to_owned(),
            windows: Vec::new(),
            using_overage,
            reported_at: None,
        };
        assert_eq!(limits_note(&limits("allowed", false)), None);
        assert_eq!(
            limits_note(&limits("allowed", true)).as_deref(),
            Some("Using paid overage.")
        );
        assert_eq!(
            limits_note(&limits("allowed_warning", false)).as_deref(),
            Some("Approaching the plan limit.")
        );
        assert_eq!(
            limits_note(&limits("rejected", true)).as_deref(),
            Some("Plan limit reached.")
        );
    }
}
