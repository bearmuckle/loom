use super::*;

/// Compact typography for inspector cards. Sizes are logical pixels converted
/// through the app's rem base so user font scaling still applies.
const CARD_TITLE_FONT_SIZE: f32 = 11.;
const CARD_LABEL_FONT_SIZE: f32 = 10.5;
const CARD_VALUE_FONT_SIZE: f32 = 12.;

fn card_title_font() -> gpui_kit::Rems {
    gpui_kit::rems(CARD_TITLE_FONT_SIZE / BASE_FONT_SIZE)
}

fn card_label_font() -> gpui_kit::Rems {
    gpui_kit::rems(CARD_LABEL_FONT_SIZE / BASE_FONT_SIZE)
}

/// The standard body text size inside an inspector card.
pub(crate) fn card_value_font() -> gpui_kit::Rems {
    gpui_kit::rems(CARD_VALUE_FONT_SIZE / BASE_FONT_SIZE)
}

mod agent;
mod changes;
mod context;
mod files;
mod plan;

impl LoomView {
    pub(crate) fn render_inspector(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let layout = responsive_layout(window.bounds().size.width);
        let child_review = self.project_child_review.is_some();
        let title = if child_review && self.review.tab == InspectorTab::Changes {
            "Project child review".to_owned()
        } else {
            self.review.tab.label().to_owned()
        };
        let header = div()
            .w_full()
            .px_2()
            .pt_2()
            .flex()
            .flex_col()
            .gap_2()
            .border_b_1()
            .border_color(rgb(0x293244))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .when(layout.phone, |element| {
                                element.child(
                                    Button::new("close-inspector")
                                        .icon(Icon::new(IconName::ChevronLeft))
                                        .ghost()
                                        .large()
                                        .h(layout.control_size())
                                        .w(layout.control_size())
                                        .accessibility_label("Back")
                                        .on_click(cx.listener(Self::close_review)),
                                )
                            })
                            .child(div().text_xs().text_color(rgb(0x93c5fd)).child(title)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(!layout.phone, |element| {
                                element.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x8f98a6))
                                        .child(self.inspector_status_summary()),
                                )
                            })
                            .when(!layout.phone, |element| {
                                element.child(
                                    Button::new("close-inspector")
                                        .label("Close")
                                        .ghost()
                                        .small()
                                        .accessibility_label("Close")
                                        .on_click(cx.listener(Self::close_review)),
                                )
                            }),
                    ),
            )
            .child(self.render_inspector_tabs(cx));
        let content = match self.review.tab {
            InspectorTab::Changes => self.render_inspector_changes(window, cx).into_any_element(),
            InspectorTab::Plan => self.render_inspector_plan(window, cx).into_any_element(),
            InspectorTab::Agent => self.render_inspector_agent(window, cx).into_any_element(),
            InspectorTab::Context => self.render_inspector_context(window, cx).into_any_element(),
            InspectorTab::Files => self.render_inspector_files(window, cx).into_any_element(),
        };
        div()
            .id("inspector-layer")
            .when(layout.phone, |element| {
                element
                    .size_full()
                    .absolute()
                    .top(px(0.))
                    .left(px(0.))
                    // The phone inspector covers the panes, so it must capture
                    // taps instead of letting them reach the header behind it.
                    .occlude()
            })
            .when(!layout.phone, |element| element.size_full())
            .flex()
            .flex_col()
            .bg(rgb(0x17191f))
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(content),
            )
    }

    fn render_inspector_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let changed_files = self.github_changed_file_count();
        let pending_attention =
            usize::from(self.pending_approval.is_some() || self.pending_input.is_some());
        div().w_full().child(
            TabBar::new("inspector-tabs")
                .underline()
                .menu(true)
                .selected_index(self.review.tab.index())
                .on_click(move |index, _window, cx| {
                    if let Some(tab) = InspectorTab::ALL.get(*index).copied() {
                        view.update(cx, |this, cx| this.select_inspector_tab(tab, cx))
                            .ok();
                    }
                })
                .children(InspectorTab::ALL.into_iter().map(|tab| {
                    let count = match tab {
                        InspectorTab::Changes if changed_files > 0 => {
                            Some(changed_files.to_string())
                        }
                        InspectorTab::Agent if pending_attention > 0 => Some("•".to_owned()),
                        InspectorTab::Plan => self
                            .plan
                            .as_ref()
                            .filter(|plan| !plan.is_empty())
                            .map(|plan| format!("{}/{}", plan.done_count(), plan.total())),
                        _ => None,
                    };
                    let unread = match tab {
                        InspectorTab::Changes => self.review.unread.changes,
                        InspectorTab::Plan => self.review.unread.plan,
                        InspectorTab::Agent | InspectorTab::Context | InspectorTab::Files => false,
                    };
                    let mut item = Tab::new().label(tab.label());
                    if count.is_some() || unread {
                        let mut suffix = div().flex().items_center().gap_1();
                        if let Some(count) = count {
                            suffix = suffix.child(
                                div()
                                    .px_1()
                                    .rounded_full()
                                    .bg(rgb(0x293244))
                                    .text_xs()
                                    .text_color(rgb(0x93c5fd))
                                    .child(count),
                            );
                        }
                        if unread {
                            suffix = suffix.child(
                                div()
                                    .id(("tab-unread", tab.index()))
                                    .test_support()
                                    .flex_shrink_0()
                                    .w(px(6.))
                                    .h(px(6.))
                                    .child(Badge::new().dot()),
                            );
                        }
                        item = item.suffix(suffix);
                    }
                    item
                })),
        )
    }

    /// The shared token/cost card shown by the Agent and Context tabs.
    pub(crate) fn render_usage_card(&self) -> impl IntoElement {
        let mut body = div().flex().flex_col().gap_2();
        match self.review.usage.total() {
            Some(usage) => {
                body = body
                    .child(info_row(
                        "Tokens",
                        format!(
                            "{} in · {} out · {} cached",
                            format_tokens(usage.input_tokens),
                            format_tokens(usage.output_tokens),
                            format_tokens(usage.cached_input_tokens)
                        ),
                    ))
                    .child(info_row("Tool calls", usage.tool_calls.to_string()));
                if let Some(provider) = self.review.usage.provider() {
                    body = body.child(info_row(
                        "Cost",
                        format!(
                            "{} · {} requests",
                            format_cost_micros(provider.cost_micros),
                            provider.requests
                        ),
                    ));
                } else if usage.cost_micros > 0 {
                    body = body.child(info_row("Cost", format_cost_micros(usage.cost_micros)));
                }
                if usage.elapsed_ms > 0 {
                    body = body.child(info_row("Duration", format_elapsed_ms(usage.elapsed_ms)));
                }
            }
            None if self.review.usage.loading => {
                body = body.child(empty_note("Loading usage…"));
            }
            None => {
                body = body.child(empty_note("No usage recorded for this session."));
            }
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

    /// A compact status line shown at the trailing edge of the inspector header.
    fn inspector_status_summary(&self) -> String {
        match self.review.tab {
            InspectorTab::Changes => self
                .review
                .vcs
                .as_ref()
                .map(|status| {
                    format!(
                        "{}  ·  {}",
                        status.branch.as_deref().unwrap_or("detached"),
                        if status.clean { "clean" } else { "modified" }
                    )
                })
                .unwrap_or_else(|| "VCS unavailable".to_owned()),
            InspectorTab::Plan => match self.plan.as_ref().filter(|plan| !plan.is_empty()) {
                Some(plan) => format!("{} / {} steps", plan.done_count(), plan.total()),
                None => "No plan".to_owned(),
            },
            InspectorTab::Agent => session_status_label(self.session_state).to_owned(),
            InspectorTab::Context => self
                .context_inspection
                .as_ref()
                .map(|inspection| format!("~ {} tokens", format_tokens(inspection.included_tokens)))
                .unwrap_or_else(|| "No inspection yet".to_owned()),
            InspectorTab::Files => {
                if self.review.files.loaded {
                    format!("{} files", self.workspace_file_count())
                } else {
                    "Not loaded".to_owned()
                }
            }
        }
    }

    fn github_changed_file_count(&self) -> usize {
        self.review
            .vcs
            .as_ref()
            .map(|status| status.files.len())
            .unwrap_or(0)
    }

    fn workspace_file_count(&self) -> usize {
        self.review
            .files
            .entries
            .iter()
            .filter(|entry| entry.kind == WorkspaceEntryKind::File)
            .count()
    }
}

/// The uppercase short label for a session state, matching the sidebar pills.
pub(crate) fn session_status_label(state: AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Idle | AgentSessionState::Archived => "Ready",
        AgentSessionState::Queued => "Queued",
        AgentSessionState::Planning => "Planning",
        AgentSessionState::Executing => "Working",
        AgentSessionState::Evaluating => "Reviewing",
        AgentSessionState::AwaitingApproval => "Approval",
        AgentSessionState::NeedsInput => "Input",
        AgentSessionState::Paused => "Paused",
        AgentSessionState::Completed => "Done",
        AgentSessionState::Failed => "Failed",
        AgentSessionState::Cancelled => "Cancelled",
    }
}

/// A one-character status marker for a Git file status.
pub(crate) fn git_status_glyph(kind: GitFileStatusKind) -> &'static str {
    match kind {
        GitFileStatusKind::Added => "A",
        GitFileStatusKind::Modified => "M",
        GitFileStatusKind::Deleted => "D",
        GitFileStatusKind::Renamed => "R",
        GitFileStatusKind::Copied => "C",
        GitFileStatusKind::Untracked => "?",
        GitFileStatusKind::Ignored => "!",
        GitFileStatusKind::Conflicted => "U",
        GitFileStatusKind::Unknown => "·",
    }
}

/// The foreground color for a Git file status marker.
pub(crate) fn git_status_color(kind: GitFileStatusKind) -> Rgba {
    match kind {
        GitFileStatusKind::Added | GitFileStatusKind::Untracked => rgb(0x86efac),
        GitFileStatusKind::Modified => rgb(0xfcd34d),
        GitFileStatusKind::Deleted | GitFileStatusKind::Conflicted => rgb(0xfca5a5),
        GitFileStatusKind::Renamed | GitFileStatusKind::Copied => rgb(0x93c5fd),
        GitFileStatusKind::Ignored | GitFileStatusKind::Unknown => rgb(0x64748b),
    }
}

/// A small uppercase heading inside the inspector.
pub(crate) fn section_heading(label: &str) -> impl IntoElement {
    div()
        .text_size(card_title_font())
        .text_color(rgb(0x93c5fd))
        .child(label.to_owned())
}

/// A muted note used for empty and informational states.
pub(crate) fn empty_note(text: &str) -> impl IntoElement {
    div()
        .p_3()
        .text_size(card_value_font())
        .text_color(rgb(0x8f98a6))
        .child(text.to_owned())
}

/// A bordered card with a heading, used by the Agent, Context, and Files tabs.
pub(crate) fn card(title: &str, body: impl IntoElement) -> impl IntoElement {
    div()
        .w_full()
        .rounded_lg()
        .bg(rgb(0x1b1d24))
        .border_1()
        .border_color(rgb(0x293244))
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            div()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(rgb(0x293244))
                .text_size(card_title_font())
                .text_color(rgb(0x93c5fd))
                .child(title.to_owned()),
        )
        .child(div().p_3().flex().flex_col().gap_2().child(body))
}

/// A label/value row used inside cards.
pub(crate) fn info_row(label: &str, value: impl IntoElement) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_3()
        .child(
            div()
                .w(px(96.))
                .flex_shrink_0()
                .text_size(card_label_font())
                .text_color(rgb(0x8f98a6))
                .child(label.to_owned()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(card_value_font())
                .text_color(rgb(0xe5e7eb))
                .child(value),
        )
}

/// A scrollable column used as the root of a tab's content.
pub(crate) fn inspector_scroll() -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id("inspector-tab-scroll")
        .flex_1()
        .min_h(px(0.))
        .w_full()
        .overflow_y_scroll()
        .p_3()
        .flex()
        .flex_col()
        .gap_3()
}

/// Formats a token count compactly for the usage surfaces.
pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Formats a micro-dollar cost as a short dollar string.
pub(crate) fn format_cost_micros(cost_micros: u64) -> String {
    if cost_micros == 0 {
        return "$0.00".to_owned();
    }
    let dollars = cost_micros as f64 / 1_000_000.0;
    if dollars < 0.01 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.2}")
    }
}

/// Formats a millisecond duration as a compact human string.
pub(crate) fn format_elapsed_ms(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_and_cost_formatting_is_compact() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(12_300), "12k");
        assert_eq!(format_tokens(1_500_000), "1.5M");
        assert_eq!(format_cost_micros(0), "$0.00");
        assert_eq!(format_cost_micros(5_000), "$0.0050");
        assert_eq!(format_cost_micros(1_500_000), "$1.50");
    }

    #[test]
    fn elapsed_formatting_covers_seconds_minutes_and_hours() {
        assert_eq!(format_elapsed_ms(5_000), "5s");
        assert_eq!(format_elapsed_ms(65_000), "1m 5s");
        assert_eq!(format_elapsed_ms(3_600_000), "1h 0m");
    }

    #[test]
    fn git_status_markers_are_distinct() {
        let kinds = [
            GitFileStatusKind::Added,
            GitFileStatusKind::Modified,
            GitFileStatusKind::Deleted,
            GitFileStatusKind::Renamed,
            GitFileStatusKind::Copied,
            GitFileStatusKind::Untracked,
            GitFileStatusKind::Ignored,
            GitFileStatusKind::Conflicted,
            GitFileStatusKind::Unknown,
        ];
        for kind in kinds {
            assert!(!git_status_glyph(kind).is_empty());
            assert!(git_status_color(kind).a > 0.0);
        }
        assert_eq!(git_status_glyph(GitFileStatusKind::Added), "A");
        assert_eq!(git_status_glyph(GitFileStatusKind::Conflicted), "U");
    }

    #[test]
    fn session_status_labels_cover_every_state() {
        for (state, label) in [
            (AgentSessionState::Idle, "Ready"),
            (AgentSessionState::Executing, "Working"),
            (AgentSessionState::AwaitingApproval, "Approval"),
            (AgentSessionState::NeedsInput, "Input"),
            (AgentSessionState::Completed, "Done"),
            (AgentSessionState::Failed, "Failed"),
        ] {
            assert_eq!(session_status_label(state), label);
        }
    }
}
