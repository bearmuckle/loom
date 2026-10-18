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

mod changes;
mod people;
mod run;

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
            .px_3()
            .pt_3()
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
                    .gap_2()
                    .when(layout.phone, |element| {
                        element.child(
                            Button::new("back-inspector")
                                .icon(Icon::new(IconName::ChevronLeft))
                                .ghost()
                                .large()
                                .h(layout.control_size())
                                .w(layout.control_size())
                                .accessibility_label("Back")
                                .on_click(cx.listener(Self::close_review)),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .text_color(rgb(0xe5e7eb))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child(self.inspector_status_summary()),
                            ),
                    )
                    .child(
                        Button::new("close-inspector")
                            .icon(Icon::new(IconName::Close))
                            .ghost()
                            .small()
                            .tooltip("Close")
                            .accessibility_label("Close")
                            .on_click(cx.listener(Self::close_review)),
                    ),
            )
            .child(self.render_inspector_tabs(cx));
        let content = match self.review.tab {
            InspectorTab::Changes => self.render_inspector_changes(window, cx).into_any_element(),
            InspectorTab::Run => self.render_inspector_run(window, cx).into_any_element(),
            InspectorTab::People => self.render_inspector_people(window, cx).into_any_element(),
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
        let pending_attention = self.pending_approval.is_some() || self.pending_input.is_some();
        div().w_full().child(
            TabBar::new("inspector-tabs")
                .underline()
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
                        InspectorTab::Run => self
                            .plan
                            .as_ref()
                            .filter(|plan| !plan.is_empty())
                            .map(|plan| format!("{}/{}", plan.done_count(), plan.total())),
                        InspectorTab::People if !self.collaboration.roster.is_empty() => {
                            Some(self.collaboration.roster.len().to_string())
                        }
                        _ => None,
                    };
                    let unread = match tab {
                        InspectorTab::Changes => self.review.unread.changes,
                        InspectorTab::Run => self.review.unread.plan,
                        InspectorTab::People => false,
                    };
                    // A run blocked on the user is the one figure the tab dot
                    // should carry: it is the only state that needs an answer.
                    let attention = tab == InspectorTab::Run && pending_attention;
                    let mut item = Tab::new().label(tab.label());
                    if count.is_some() || unread || attention {
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
                        if attention {
                            suffix = suffix.child(
                                div()
                                    .id("tab-attention")
                                    .test_support()
                                    .flex_shrink_0()
                                    .w(px(6.))
                                    .h(px(6.))
                                    .child(Badge::new().dot().color(rgb(0xfcd34d))),
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

    /// A compact status line under the inspector title.
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
            InspectorTab::Run => {
                let state = session_status_label(self.session_state);
                match self.plan.as_ref().filter(|plan| !plan.is_empty()) {
                    Some(plan) => {
                        format!("{state}  ·  {}/{} steps", plan.done_count(), plan.total())
                    }
                    None => state.to_owned(),
                }
            }
            InspectorTab::People => match self.collaboration.roster.len() {
                1 => "1 person".to_owned(),
                count => format!("{count} people"),
            },
        }
    }

    fn github_changed_file_count(&self) -> usize {
        self.review
            .vcs
            .as_ref()
            .map(|status| status.files.len())
            .unwrap_or(0)
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

/// A bordered card with a heading, used by every inspector tab.
pub(crate) fn card(title: &str, body: impl IntoElement) -> impl IntoElement {
    card_with_action(title, div().into_any_element(), body)
}

/// The same card with a trailing control in its heading row, such as a
/// disclosure toggle or a refresh button.
pub(crate) fn card_with_action(
    title: &str,
    action: impl IntoElement,
    body: impl IntoElement,
) -> impl IntoElement {
    div()
        .w_full()
        .rounded_md()
        .bg(rgb(0x1b1d24))
        .border_1()
        .border_color(rgb(0x293244))
        .p_3()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(section_heading(title))
                .child(div().flex_shrink_0().child(action)),
        )
        .child(body)
}

/// A labelled figure inside a card, used instead of a label/value list so the
/// run figures line up in one row.
pub(crate) fn metric(label: &str, value: impl IntoElement) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .truncate()
                .text_size(card_label_font())
                .text_color(rgb(0x8f98a6))
                .child(label.to_owned()),
        )
        .child(
            div()
                .truncate()
                .text_size(card_value_font())
                .text_color(rgb(0xe5e7eb))
                .child(value),
        )
}

/// A row of [`metric`] figures.
pub(crate) fn metric_row() -> gpui_kit::Div {
    div().w_full().flex().items_start().gap_3()
}

/// A muted line under a card's figures, for counts that do not deserve a row.
pub(crate) fn metric_caption(text: impl Into<String>) -> impl IntoElement {
    div()
        .w_full()
        .text_size(card_label_font())
        .text_color(rgb(0x8f98a6))
        .child(text.into())
}

/// A monospaced muted value, used for counts and identifiers in headings.
pub(crate) fn mono_note(text: impl Into<String>) -> impl IntoElement {
    div()
        .font_family(mono_font())
        .text_size(card_label_font())
        .text_color(rgb(0x8f98a6))
        .child(text.into())
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
