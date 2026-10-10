use super::*;
use crate::view::archive::{ArchivedSessionRow, RetentionPolicyText, archived_session_groups};
use gpui_kit::component::checkbox::Checkbox;

/// How one archived-session row is drawn: its position in the list, the force
/// choice the user made for it, and the layout it renders into.
struct ArchivedSessionRowStyle {
    index: usize,
    force: bool,
    delete_in_flight: Option<AgentSessionId>,
    first_in_group: bool,
    phone: bool,
}

impl LoomView {
    /// The archived-session management surface: every archived session the
    /// known workspaces hold, grouped by project, with a delete action and the
    /// active retention policy read-only.
    pub(crate) fn render_archived_sessions(
        &self,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let phone = layout.phone;
        let now = Timestamp::now().as_unix_millis();
        let groups = archived_session_groups(
            &self.archived_sessions.entries,
            &self.archived_sessions.projects,
            now,
        );
        let retention = RetentionPolicyText::of(&self.archived_sessions.retention);
        let mut content = div().w_full().flex().flex_col().gap_4();

        let mut policy_card = div().px_4().py_3().flex().flex_col().gap_1().child(
            div()
                .id("archive-retention-policy")
                .test_support()
                .text_sm()
                .child(retention.headline.clone()),
        );
        if !retention.detail.is_empty() {
            policy_card = policy_card.child(
                div()
                    .id("archive-retention-detail")
                    .test_support()
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child(retention.detail.clone()),
            );
        }
        content = content.child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("RETENTION POLICY"))
                .child(
                    settings_card().child(
                        policy_card.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x64748b))
                                .child("Set by the operator; this view is read-only."),
                        ),
                    ),
                ),
        );

        if let Some(refusal) = &self.archived_sessions.refusal {
            content = content.child(
                div()
                    .id("archived-session-refusal")
                    .test_support()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(rgb(0x2a1c22))
                    .border_1()
                    .border_color(rgb(0x7f1d1d))
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child(format!("Delete refused: {refusal}")),
            );
        }
        if let Some(error) = &self.archived_sessions.error {
            content = content.child(
                div()
                    .id("archived-session-load-error")
                    .test_support()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(rgb(0x2a241a))
                    .border_1()
                    .border_color(rgb(0x78350f))
                    .text_sm()
                    .text_color(rgb(0xfcd34d))
                    .child(format!(
                        "Some archived sessions could not be listed: {error}"
                    )),
            );
        }

        if groups.is_empty() {
            content = content.child(
                div()
                    .id("archived-sessions-empty")
                    .test_support()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child(if self.archived_sessions.loading {
                        "Loading archived sessions…"
                    } else {
                        "Nothing is archived. Archiving a session keeps it here until it is deleted."
                    }),
            );
        } else {
            let force = self.archived_sessions.force.clone();
            let delete_in_flight = self.archived_sessions.delete_in_flight;
            let mut row_index = 0usize;
            for group in &groups {
                let mut card = settings_card();
                let mut first_in_group = true;
                for row in &group.rows {
                    let style = ArchivedSessionRowStyle {
                        index: row_index,
                        force: force.contains(&row.session.id),
                        delete_in_flight,
                        first_in_group,
                        phone,
                    };
                    row_index += 1;
                    card = card.child(self.render_archived_session_row(row, style, cx));
                    first_in_group = false;
                }
                content = content.child(
                    div()
                        .w_full()
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
                                        .id(("archived-session-group", row_index))
                                        .test_support()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_sm()
                                        .text_color(rgb(0xf3f4f6))
                                        .child(group.label.clone()),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_xs()
                                        .text_color(rgb(0x64748b))
                                        .child(format!(
                                            "{} archived",
                                            match group.rows.len() {
                                                1 => "1 session".to_owned(),
                                                count => format!("{count} sessions"),
                                            }
                                        )),
                                ),
                        )
                        .child(card),
                );
            }
        }

        div()
            .id("archived-sessions-dialog")
            .test_support()
            .occlude()
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .when(!phone, |element| element.px_6().py_4())
                    .when(phone, |element| element.py_3())
                    .border_b_1()
                    .border_color(rgb(0x242833))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child("Archived sessions"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("archived-sessions-refresh")
                                    .label("Refresh")
                                    .ghost()
                                    .small()
                                    .disabled(self.archived_sessions.loading)
                                    .accessibility_label("Refresh archived sessions")
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.reload_archived_sessions(cx);
                                    })),
                            )
                            .child(
                                Button::new("close-archived-sessions")
                                    .icon(Icon::new(IconName::Close))
                                    .ghost()
                                    .small()
                                    .accessibility_label("Close archived sessions")
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.close_archived_sessions(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id("archived-sessions-content")
                    .test_support()
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .p_4()
                    .when(!phone, |element| element.p_6())
                    .overflow_y_scroll()
                    .child(content),
            )
            .into_any()
    }

    /// One archived-session row: its name, worker, archive age, the force
    /// choice, and the delete action.
    fn render_archived_session_row(
        &self,
        row: &ArchivedSessionRow,
        style: ArchivedSessionRowStyle,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let index = style.index;
        let session_id = row.session.id;
        let delete_in_flight = style.delete_in_flight;
        let phone = style.phone;
        let delete_label = if row.is_project_root {
            "Delete project"
        } else {
            "Delete"
        };
        let consequence = if row.is_project_root {
            match row.descendant_count {
                0 => "Deletes this project and its stored history.".to_owned(),
                1 => "Deletes this project, 1 descendant session, and their worktrees.".to_owned(),
                count => format!(
                    "Deletes this project, {count} descendant sessions, and their worktrees."
                ),
            }
        } else {
            "Deletes this session, its stored history, and its worktrees.".to_owned()
        };
        let worker = self
            .node_names
            .get(&row.node_id)
            .cloned()
            .unwrap_or_else(|| row.node_id.clone());
        let controls = div()
            .flex()
            .when(phone, |element| element.flex_col().items_start().gap_2())
            .when(!phone, |element| element.items_center().gap_3())
            .child(
                Checkbox::new(("archived-session-force", index))
                    .checked(style.force)
                    .label("Force delete, discarding dirty or locked worktrees")
                    .on_change({
                        let view = cx.entity();
                        move |checked, _, cx| {
                            let checked = *checked;
                            view.update(cx, |view, cx| {
                                view.set_archived_session_force(session_id, checked, cx);
                            });
                        }
                    }),
            )
            .child(
                Button::new(("archived-session-delete", index))
                    .label(delete_label)
                    .danger()
                    .small()
                    .disabled(delete_in_flight.is_some())
                    .accessibility_label(format!("{delete_label}: {}", row.session.name))
                    .on_click({
                        let view = cx.entity();
                        move |_, _, cx| {
                            view.update(cx, |view, cx| {
                                view.delete_archived_session(session_id, cx);
                            });
                        }
                    }),
            );
        div()
            .id(("archived-session-row", index))
            .test_support()
            .when(!style.first_in_group, |element| {
                element.border_t_1().border_color(rgb(0x242833))
            })
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(if row.is_project_root {
                            AssetIconName::Folder
                        } else {
                            AssetIconName::BotMessageSquare
                        })
                        .size(px(15.))
                        .flex_shrink_0()
                        .text_color(rgb(0x8f98a6)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_sm()
                            .text_color(rgb(0xe5e7eb))
                            .when(row.is_project_root, |element| {
                                element.font_weight(FontWeight::SEMIBOLD)
                            })
                            .child(row.session.name.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child(row.age.clone()),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x64748b))
                    .child(format!("{worker} · {consequence}")),
            )
            .child(controls)
    }
}
