//! The People tab: roster and presence, owner access controls, the run
//! credential, and repository access.

use loom_protocol::SessionRole;

use super::*;
use crate::collaboration::{credential_label, pin_label, presence_label, role_label};

impl LoomView {
    pub(crate) fn render_inspector_people(
        &self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        inspector_scroll()
            .child(self.render_roster_card(cx))
            .child(self.render_credential_card(cx))
            .child(self.render_repository_access_card())
    }

    fn render_roster_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.collaboration;
        let manage = state.permissions().can_manage && state.is_participant_bound();
        let mut body = div().flex().flex_col().gap_2();
        if !state.is_participant_bound() && state.roster.is_empty() {
            body = body.child(empty_note(
                "This connection is not bound to a person, so the session has no roster. \
                 Start the worker with --participants-file to share it.",
            ));
        } else if state.roster.is_empty() {
            body = body.child(empty_note("Loading people…"));
        }
        let me = state.identity.as_ref().map(|identity| identity.id);
        for (index, entry) in state.roster.iter().enumerate() {
            let participant_id = entry.participant.id;
            let connections = match entry.connections {
                0 => "no connections".to_owned(),
                1 => "1 connection".to_owned(),
                count => format!("{count} connections"),
            };
            let you = if me == Some(participant_id) {
                " (you)"
            } else {
                ""
            };
            let mut row = div()
                .id(("roster-entry", index))
                .test_support()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(8.))
                        .h(px(8.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(rgb(match entry.presence {
                            loom_protocol::PresenceStatus::Online => 0x34d399,
                            loom_protocol::PresenceStatus::Away => 0xfbbf24,
                            loom_protocol::PresenceStatus::Offline => 0x475569,
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            div()
                                .text_size(card_value_font())
                                .text_color(rgb(0xe5e7eb))
                                .truncate()
                                .child(format!(
                                    "{}{you} · {}",
                                    entry.participant.display_name,
                                    role_label(entry.role)
                                )),
                        )
                        .child(div().text_xs().text_color(rgb(0x8f98a6)).child(format!(
                            "{} · {connections}",
                            presence_label(entry.presence)
                        ))),
                );
            if manage && state.can_change(participant_id) {
                let view = cx.entity();
                let role = entry.role;
                row = row.child(
                    Button::new(("roster-role", index))
                        .label(role_label(role))
                        .ghost()
                        .xsmall()
                        .accessibility_label("Change role or revoke")
                        .dropdown_menu(move |mut menu, _, _| {
                            for candidate in [
                                SessionRole::Observer,
                                SessionRole::Participant,
                                SessionRole::Owner,
                            ] {
                                let view = view.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(role_label(candidate))
                                        .checked(candidate == role)
                                        .on_click(move |_, _, cx| {
                                            view.update(cx, |this, cx| {
                                                this.set_session_role(
                                                    participant_id,
                                                    candidate,
                                                    cx,
                                                );
                                            });
                                        }),
                                );
                            }
                            let view = view.clone();
                            menu.separator()
                                .item(PopupMenuItem::new("Revoke access").on_click(
                                    move |_, _, cx| {
                                        view.update(cx, |this, cx| {
                                            this.revoke_session_access(participant_id, cx);
                                        });
                                    },
                                ))
                        }),
                );
            }
            body = body.child(row);
        }
        if manage {
            let grantable = state.grantable().into_iter().cloned().collect::<Vec<_>>();
            if !grantable.is_empty() {
                let view = cx.entity();
                body = body.child(
                    Button::new("grant-session-access")
                        .label("Add person…")
                        .outline()
                        .xsmall()
                        .dropdown_menu(move |mut menu, _, _| {
                            for person in &grantable {
                                let view = view.clone();
                                let id = person.id;
                                menu = menu.item(
                                    PopupMenuItem::new(person.display_name.clone()).on_click(
                                        move |_, _, cx| {
                                            view.update(cx, |this, cx| {
                                                this.grant_session_access(id, cx);
                                            });
                                        },
                                    ),
                                );
                            }
                            menu
                        }),
                );
            }
            body = body.child(div().text_xs().text_color(rgb(0x64748b)).child(
                "New people join as observers (read-only). Promote them to participant to let \
                 them chat, direct the agent, and approve actions.",
            ));
        } else if let Some(role) = state.role() {
            let note = match role {
                SessionRole::Observer => {
                    "You are an observer: you can read this session but not act in it."
                }
                SessionRole::Participant => {
                    "You can chat, direct the agent, approve actions, and pause or stop the run. \
                     The owner manages access, credentials, and repositories."
                }
                SessionRole::Owner => "",
            };
            if !note.is_empty() {
                body = body.child(div().text_xs().text_color(rgb(0x8f98a6)).child(note));
            }
        }
        card("Participants", body)
    }

    fn render_credential_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.collaboration;
        let mut body = div().flex().flex_col().gap_2();
        let selected = match &state.session_credential {
            Some(credential) => credential_label(credential),
            None if state.credential_loaded => "Worker default".to_owned(),
            None => "Loading…".to_owned(),
        };
        body = body.child(info_row(
            "Next runs",
            div()
                .id("session-credential")
                .test_support()
                .truncate()
                .child(selected),
        ));
        if let Some((run_id, pin)) = &state.run_pin
            && self.active_run_id == Some(*run_id)
        {
            body = body.child(info_row(
                "This run",
                div().truncate().child(match pin {
                    Some(pin) => format!("{} · {}", pin_label(pin), pin.model),
                    None => "Not pinned (started before credential pinning)".to_owned(),
                }),
            ));
        }
        if state.permissions().can_manage && !state.credentials.is_empty() {
            let view = cx.entity();
            let choices = state
                .credentials
                .iter()
                .filter(|credential| !credential.revoked)
                .cloned()
                .collect::<Vec<_>>();
            let current = state.selected_credential_id();
            body = body.child(
                Button::new("select-session-credential")
                    .label("Change credential…")
                    .outline()
                    .xsmall()
                    .dropdown_menu(move |mut menu, _, _| {
                        for credential in &choices {
                            let view = view.clone();
                            let id = credential.id;
                            menu = menu.item(
                                PopupMenuItem::new(credential_label(credential))
                                    .checked(current == Some(id))
                                    .on_click(move |_, _, cx| {
                                        view.update(cx, |this, cx| {
                                            this.select_session_credential(id, cx);
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            );
        }
        body = body.child(div().text_xs().text_color(rgb(0x64748b)).child(
            "The secret stays on the worker. A run keeps the credential it started with; a \
             change applies to later runs.",
        ));
        card("Run credential", body)
    }

    fn render_repository_access_card(&self) -> impl IntoElement {
        let mut body = div().flex().flex_col().gap_2();
        if self.session_repositories.is_empty() && self.session_directories.is_empty() {
            body = body.child(empty_note("No sources attached."));
        }
        for repository in &self.session_repositories {
            body = body.child(info_row(
                "Attached",
                div().truncate().child(repository.source.clone()),
            ));
        }
        for directory in &self.session_directories {
            body = body.child(info_row(
                "Folder",
                div().truncate().child(directory.source.clone()),
            ));
        }
        body = body.child(div().text_xs().text_color(rgb(0x64748b)).child(
            if self.can_manage_sources() {
                "You control browsing and attaching. Others use this session's checkout."
            } else {
                "The owner controls browsing and attaching. You use this session's checkout."
            },
        ));
        card("Repository access", body)
    }
}
