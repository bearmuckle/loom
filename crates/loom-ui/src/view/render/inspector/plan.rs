use super::*;

impl LoomView {
    /// The full plan checklist for the active run.
    ///
    /// A plan is run-scoped progress rather than a conversation message, so it
    /// lives in the inspector instead of the transcript. The header collapses
    /// the step list; the progress bar stays visible either way.
    pub(crate) fn render_inspector_plan(
        &self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let content = inspector_scroll();
        let Some(plan) = self.plan.as_ref().filter(|plan| !plan.is_empty()) else {
            return content.child(empty_note("No plan for the active run."));
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
        let body = div()
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
                            .text_size(card_title_font())
                            .text_color(rgb(0x93c5fd))
                            .child("Plan"),
                    )
                    .child(
                        div()
                            .text_size(card_label_font())
                            .text_color(rgb(0x8f98a6))
                            .child(format!("{done} / {total}")),
                    )
                    .child(div().flex_1())
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
                    .id("plan-checklist")
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(steps),
            );
        content.child(body)
    }
}
