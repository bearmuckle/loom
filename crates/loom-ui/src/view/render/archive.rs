use super::*;
use crate::view::archive::{
    ArchivedSessionRow, RetentionPolicyState, RetentionPolicyText, project_row_consequence,
    retention_window_choices, sub_agent_retention_window_choices,
};
use gpui_kit::WeakEntity;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};

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
    /// The archived-session management surface: one row per project the known
    /// workspaces resolve, plus one row per archived session that belongs to no
    /// project, with a delete action and the active backend's retention policy,
    /// which can be edited when the worker behind that backend can store a
    /// change.
    pub(crate) fn render_archived_sessions(
        &self,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let phone = layout.phone;
        let now = Timestamp::now().as_unix_millis();
        let groups = self.grouped_archived_sessions(now);
        let mut content = div().w_full().flex().flex_col().gap_4();

        content = content.child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("RETENTION POLICY"))
                .child(settings_card().child(self.render_archive_retention_card(cx))),
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
                        force: force.contains(&row.session_id),
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
                                        .child(group.heading_detail()),
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

    /// The retention-policy card.
    ///
    /// It is an editor when the worker behind the active backend can store a
    /// change; a worker that reports no policy, and one that advertises no
    /// `ConfigureArchiveRetention`, keep the read-only text with a short
    /// explanation instead of controls.
    fn render_archive_retention_card(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let state = &self.archived_sessions;
        let stored = match &state.retention {
            RetentionPolicyState::Available(policy) => Some(*policy),
            RetentionPolicyState::Loading | RetentionPolicyState::Unavailable => None,
        };
        // The card is an editor only when the worker reported a policy and can
        // replace it; the load keeps a draft exactly then, so all three facts
        // have to agree before the controls are drawn.
        let draft = match (state.retention_editable, stored, state.retention_draft) {
            (true, Some(_), Some(draft)) => Some(draft),
            _ => None,
        };
        let Some(draft) = draft else {
            let reason = stored.map(|_| {
                "This worker does not store policy changes, so it keeps the policy it started with."
                    .to_owned()
            });
            return render_retention_policy_text(RetentionPolicyText::of(&state.retention), reason);
        };

        let (choices, selected) = retention_window_choices(draft);
        let (sub_agent_choices, sub_agent_selected) = sub_agent_retention_window_choices(draft);
        let saving = state.retention_saving;
        let worker = self
            .node_names
            .get(&self.default_backend_node_id)
            .cloned()
            .unwrap_or_else(|| self.default_backend_node_id.clone());
        let status = if saving {
            div()
                .id("archive-retention-saving")
                .test_support()
                .text_xs()
                .text_color(rgb(0x8f98a6))
                .child("Storing the policy…")
        } else if state.retention_saved {
            div()
                .id("archive-retention-saved")
                .test_support()
                .text_xs()
                .text_color(rgb(0x8f98a6))
                .child("Stored.")
        } else if self.archive_retention_draft_is_dirty() {
            div()
                .id("archive-retention-pending")
                .test_support()
                .text_xs()
                .text_color(rgb(0x8f98a6))
                .child("Not stored yet.")
        } else {
            div()
                .id("archive-retention-current")
                .test_support()
                .text_xs()
                .text_color(rgb(0x8f98a6))
                .child("Stored on the worker.")
        };

        let mut card = div()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf3f4f6))
                    .child("Delete archived sessions after"),
            )
            .child(RetentionWindowSelect::new(
                ("archive-retention-window", 0usize),
                cx.entity().downgrade(),
                choices,
                selected,
                saving,
                false,
            ))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf3f4f6))
                    .child("Delete archived sub-agent sessions after"),
            )
            .child(RetentionWindowSelect::new(
                ("archive-retention-window", 1usize),
                cx.entity().downgrade(),
                sub_agent_choices,
                sub_agent_selected,
                saving,
                true,
            ))
            .child(
                Checkbox::new("archive-retention-force")
                    .checked(draft.force_discard_worktrees)
                    .disabled(saving)
                    .label("Force delete, discarding dirty or locked worktrees")
                    .on_change({
                        let view = cx.entity();
                        move |checked, _, cx| {
                            let checked = *checked;
                            view.update(cx, |view, cx| {
                                view.set_archive_retention_force(checked, cx);
                            });
                        }
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("apply-archive-retention")
                            .label("Apply")
                            .small()
                            .disabled(saving || !self.archive_retention_draft_is_dirty())
                            .accessibility_label("Apply the archive retention policy")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.save_archive_retention_policy(cx);
                            })),
                    )
                    .child(status),
            )
            .child(
                div()
                    .id("archive-retention-worker")
                    .test_support()
                    .text_xs()
                    .text_color(rgb(0x64748b))
                    .child(format!(
                        "Stores on {worker} · the change applies from the next sweep."
                    )),
            );
        if let Some(refusal) = &state.retention_refusal {
            card = card.child(
                div()
                    .id("archive-retention-refusal")
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
                    .child(format!("Change refused: {refusal}")),
            );
        }
        card.into_any_element()
    }

    /// One archived-session row: its heading, worker, archive age, the force
    /// choice, and the delete action. A project row lists the whole tree and
    /// deletes it behind a confirmation; a plain row deletes itself.
    fn render_archived_session_row(
        &self,
        row: &ArchivedSessionRow,
        style: ArchivedSessionRowStyle,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let index = style.index;
        let session_id = row.session_id;
        let is_project = row.is_project;
        let delete_in_flight = style.delete_in_flight;
        let phone = style.phone;
        let delete_label = if is_project {
            "Delete project"
        } else {
            "Delete"
        };
        let consequence = if is_project {
            project_row_consequence(row.session_count)
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
                    .accessibility_label(format!("{delete_label}: {}", row.label))
                    .on_click({
                        let view = cx.entity();
                        move |_, _, cx| {
                            view.update(cx, |view, cx| {
                                if is_project {
                                    // A whole tree is never removed without
                                    // the confirmation.
                                    view.begin_project_delete(session_id, cx);
                                } else {
                                    view.delete_archived_session(session_id, cx);
                                }
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
                        Icon::new(if is_project {
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
                            .when(is_project, |element| {
                                element.font_weight(FontWeight::SEMIBOLD)
                            })
                            .child(row.label.clone()),
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

/// The read-only retention text: the headline, its supporting line, and an
/// optional explanation of why the card offers no editor.
fn render_retention_policy_text(
    text: RetentionPolicyText,
    reason: Option<String>,
) -> gpui_kit::AnyElement {
    let mut card = div().px_4().py_3().flex().flex_col().gap_1().child(
        div()
            .id("archive-retention-policy")
            .test_support()
            .text_sm()
            .child(text.headline),
    );
    if !text.detail.is_empty() {
        card = card.child(
            div()
                .id("archive-retention-detail")
                .test_support()
                .text_xs()
                .text_color(rgb(0x8f98a6))
                .child(text.detail),
        );
    }
    if let Some(reason) = reason {
        card = card.child(
            div()
                .id("archive-retention-read-only")
                .test_support()
                .text_xs()
                .text_color(rgb(0x64748b))
                .child(reason),
        );
    }
    card.into_any_element()
}

/// The retention-window Select of the card's editor.
///
/// A `SelectState` is created with a window, and the archived-sessions dialog
/// renders without one, so the control's state lives in the window that draws
/// it, keyed by this element's identity, and is created on the first frame that
/// draws the editor. Its confirmation goes to the view, which owns the draft.
#[derive(IntoElement)]
struct RetentionWindowSelect {
    id: (&'static str, usize),
    view: WeakEntity<LoomView>,
    choices: Vec<String>,
    selected: String,
    disabled: bool,
    sub_agent: bool,
}

/// The window-owned state of one retention-window Select: the control plus what
/// it was last given, so its items and value are only rewritten when they differ.
struct RetentionWindowSelectState {
    select: Entity<SelectState<SearchableVec<String>>>,
    choices: Vec<String>,
    selected: String,
    /// Held so the control keeps reporting confirmations while the window keeps
    /// this state.
    _confirm: Subscription,
}

impl RetentionWindowSelect {
    fn new(
        id: (&'static str, usize),
        view: WeakEntity<LoomView>,
        choices: Vec<String>,
        selected: String,
        disabled: bool,
        sub_agent: bool,
    ) -> Self {
        Self {
            id,
            view,
            choices,
            selected,
            disabled,
            sub_agent,
        }
    }

    /// The row of `selected` in `choices`, which the control seeds its cursor
    /// from.
    fn selected_row(choices: &[String], selected: &str) -> Option<IndexPath> {
        choices
            .iter()
            .position(|choice| choice == selected)
            .map(|row| IndexPath::default().row(row))
    }
}

impl RenderOnce for RetentionWindowSelect {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id, cx, |window, cx| {
            let view = self.view.clone();
            let sub_agent = self.sub_agent;
            let select = cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(self.choices.clone()),
                    RetentionWindowSelect::selected_row(&self.choices, &self.selected),
                    window,
                    cx,
                )
            });
            let confirm = cx.subscribe(
                &select,
                move |_, _, event: &SelectEvent<SearchableVec<String>>, cx| {
                    let SelectEvent::Confirm(Some(choice)) = event else {
                        return;
                    };
                    let choice = choice.clone();
                    if let Some(view) = view.upgrade() {
                        view.update(cx, |view, cx| {
                            if sub_agent {
                                view.choose_archive_sub_agent_retention_window(&choice, cx);
                            } else {
                                view.choose_archive_retention_window(&choice, cx);
                            }
                        });
                    }
                },
            );
            RetentionWindowSelectState {
                select,
                choices: self.choices.clone(),
                selected: self.selected.clone(),
                _confirm: confirm,
            }
        });

        let up_to_date = {
            let state = state.read(cx);
            state.choices == self.choices && state.selected == self.selected
        };
        if !up_to_date {
            state.update(cx, |state, cx| {
                state.select.update(cx, |select, cx| {
                    // The items come first: the value is matched against them,
                    // so a new custom entry has to exist before it is selected.
                    select.set_items(SearchableVec::new(self.choices.clone()), window, cx);
                    select.set_selected_value(&self.selected, window, cx);
                });
                state.choices = self.choices.clone();
                state.selected = self.selected.clone();
            });
        }

        let select = state.read(cx).select.clone();
        Select::new(&select)
            .id(self.id)
            .small()
            .disabled(self.disabled)
            .accessibility_label(if self.sub_agent {
                "Delete archived sub-agent sessions after"
            } else {
                "Delete archived sessions after"
            })
            .into_any_element()
    }
}
