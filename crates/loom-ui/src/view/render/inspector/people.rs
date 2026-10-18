//! The People tab: roster and presence, owner access controls, the run
//! credential, and repository access.

use loom_protocol::{MAX_ACCESS_REQUEST_NOTE_BYTES, SessionRole};

use super::*;
use crate::collaboration::{
    access_outcome_text, credential_label, pin_label, presence_label, role_label,
};

impl LoomView {
    pub(crate) fn render_inspector_people(
        &self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        inspector_scroll()
            .child(self.render_roster_card(cx))
            .when_some(self.render_access_request_card(cx), |scroll, card| {
                scroll.child(card)
            })
            .when_some(self.render_pending_requests_card(cx), |scroll, card| {
                scroll.child(card)
            })
            .child(self.render_credential_card(cx))
            .child(self.render_repository_access_card())
    }

    /// The requester's "Request access" control, pending state, and outcome.
    fn render_access_request_card(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let state = &self.collaboration;
        let access = &state.access;
        if !state.can_request_access() && access.outcome.is_none() {
            return None;
        }
        let mut body = div().flex().flex_col().gap_2();
        if let Some(outcome) = &access.outcome {
            body = body.child(
                div()
                    .id("access-request-outcome")
                    .test_support()
                    .text_size(card_value_font())
                    .text_color(rgb(0xe5e7eb))
                    .child(access_outcome_text(outcome)),
            );
        }
        if let Some(own) = &access.own {
            let view = cx.entity();
            body = body
                .child(
                    div()
                        .id("access-request-pending")
                        .test_support()
                        .text_size(card_value_font())
                        .text_color(rgb(0xe5e7eb))
                        .child(format!(
                            "Requested {} access · waiting for an owner",
                            role_label(own.requested_role).to_lowercase()
                        )),
                )
                .when_some(own.note.clone(), |body, note| {
                    body.child(div().text_xs().text_color(rgb(0x8f98a6)).child(note))
                })
                .child(
                    Button::new("withdraw-access-request")
                        .label("Withdraw")
                        .outline()
                        .xsmall()
                        .disabled(access.in_flight)
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| this.withdraw_session_access_request(cx));
                        }),
                );
        } else if state.can_request_access() {
            let role = access.draft_role();
            let role_view = cx.entity();
            let send_view = cx.entity();
            let note_input = self.access_note_input.clone();
            let mut controls = div().flex().items_center().gap_2().child(
                Button::new("access-request-role")
                    .label(role_label(role))
                    .ghost()
                    .xsmall()
                    .accessibility_label("Requested role")
                    .dropdown_menu(move |mut menu, _, _| {
                        for candidate in [SessionRole::Participant, SessionRole::Owner] {
                            let view = role_view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(role_label(candidate))
                                    .checked(candidate == role)
                                    .on_click(move |_, _, cx| {
                                        view.update(cx, |this, cx| {
                                            this.set_access_request_role(candidate, cx);
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            );
            controls = controls.child(
                Button::new("send-access-request")
                    .label("Request access")
                    .primary()
                    .xsmall()
                    .disabled(access.in_flight)
                    .on_click(move |_, _, cx| {
                        let note = note_input
                            .as_ref()
                            .map(|input| input.read(cx).value().to_string())
                            .unwrap_or_default();
                        send_view.update(cx, |this, cx| this.request_session_access(&note, cx));
                    }),
            );
            if let Some(input) = self.access_note_input.as_ref() {
                body = body.child(KitInput::new(input).id("access-request-note").small());
            }
            body = body
                .child(controls)
                .child(div().text_xs().text_color(rgb(0x64748b)).child(format!(
                    "Owners see your request and note (at most {MAX_ACCESS_REQUEST_NOTE_BYTES} \
                     bytes)."
                )));
        }
        Some(card("Request access", body).into_any_element())
    }

    /// Creates a deny-reason input for each pending request that lacks one
    /// and drops the inputs of requests that are no longer pending.
    pub(crate) fn sync_access_deny_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending: Vec<_> = self
            .collaboration
            .access
            .pending
            .iter()
            .map(|request| request.id)
            .collect();
        self.access_deny_inputs
            .retain(|request_id, _| pending.contains(request_id));
        for request_id in pending {
            self.access_deny_inputs
                .entry(request_id)
                .or_insert_with(|| {
                    cx.new(|cx| InputState::new(window, cx).placeholder("Optional reason to deny"))
                });
        }
    }

    /// The owner's list of pending access requests.
    fn render_pending_requests_card(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let state = &self.collaboration;
        if !state.manages_access_requests() {
            return None;
        }
        let access = &state.access;
        let mut body = div().flex().flex_col().gap_2();
        if access.pending.is_empty() {
            body = body.child(empty_note("No pending requests."));
        }
        for (index, request) in access.pending.iter().enumerate() {
            let request_id = request.id;
            let approve_role = access.approve_role(request);
            let role_view = cx.entity();
            let approve_view = cx.entity();
            let deny_view = cx.entity();
            let deny_input = self.access_deny_inputs.get(&request_id).cloned();
            body = body.child(
                div()
                    .id(("access-request", index))
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(card_value_font())
                            .text_color(rgb(0xe5e7eb))
                            .child(format!(
                                "{} asks to be {}",
                                request.participant.display_name,
                                role_label(request.requested_role).to_lowercase()
                            )),
                    )
                    .when_some(request.note.clone(), |row, note| {
                        row.child(div().text_xs().text_color(rgb(0x8f98a6)).child(note))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new(("access-request-approve-role", index))
                                    .label(role_label(approve_role))
                                    .ghost()
                                    .xsmall()
                                    .accessibility_label("Role to grant")
                                    .dropdown_menu(move |mut menu, _, _| {
                                        for candidate in [
                                            SessionRole::Observer,
                                            SessionRole::Participant,
                                            SessionRole::Owner,
                                        ] {
                                            let view = role_view.clone();
                                            menu = menu.item(
                                                PopupMenuItem::new(role_label(candidate))
                                                    .checked(candidate == approve_role)
                                                    .on_click(move |_, _, cx| {
                                                        view.update(cx, |this, cx| {
                                                            this.collaboration
                                                                .access
                                                                .set_approve_role(
                                                                    request_id, candidate,
                                                                );
                                                            cx.notify();
                                                        });
                                                    }),
                                            );
                                        }
                                        menu
                                    }),
                            )
                            .child(
                                Button::new(("access-request-approve", index))
                                    .label("Approve")
                                    .primary()
                                    .xsmall()
                                    .disabled(access.in_flight)
                                    .on_click(move |_, _, cx| {
                                        approve_view.update(cx, |this, cx| {
                                            this.approve_session_access_request(request_id, cx);
                                        });
                                    }),
                            )
                            .child(
                                Button::new(("access-request-deny", index))
                                    .label("Deny")
                                    .outline()
                                    .xsmall()
                                    .disabled(access.in_flight)
                                    .on_click(move |_, _, cx| {
                                        let reason = deny_input
                                            .as_ref()
                                            .map(|input| input.read(cx).value().to_string())
                                            .unwrap_or_default();
                                        deny_view.update(cx, |this, cx| {
                                            this.deny_session_access_request(
                                                request_id, &reason, cx,
                                            );
                                        });
                                    }),
                            ),
                    )
                    .when_some(self.access_deny_inputs.get(&request_id), |row, input| {
                        row.child(
                            KitInput::new(input)
                                .id(("access-deny-reason", index))
                                .small(),
                        )
                    }),
            );
        }
        Some(card("Access requests", body).into_any_element())
    }

    fn render_roster_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.collaboration;
        let sign_in_required = self.awaits_sign_in();
        let manage = self.session_permissions().can_manage && state.is_participant_bound();
        let mut body = div().flex().flex_col().gap_2();
        if sign_in_required {
            body = body
                .child(
                    div()
                        .id("roster-sign-in-required")
                        .test_support()
                        .text_size(card_value_font())
                        .text_color(rgb(0xe5e7eb))
                        .child("You are not signed in to this worker."),
                )
                .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                    "Signing in with GitHub puts you in the roster. You start read-only, and an \
                     owner grants the role you need.",
                ));
            if self.offers_github_sign_in() {
                let node_id = self.active_node_id().to_owned();
                body = body.child(
                    Button::new("people-sign-in-github")
                        .label("Sign in with GitHub")
                        .primary()
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.begin_identity_sign_in(node_id.clone(), cx);
                        })),
                );
            }
        } else if !state.is_participant_bound() && state.roster.is_empty() {
            if self.offers_github_sign_in() {
                body = body
                    .child(empty_note(
                        "This connection reports no person, so the session has no roster. \
                         Signing in with GitHub is what puts a person in it; an owner then \
                         grants a role.",
                    ))
                    .child({
                        let node_id = self.active_node_id().to_owned();
                        Button::new("roster-sign-in-github")
                            .label("Sign in with GitHub")
                            .outline()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.begin_identity_sign_in(node_id.clone(), cx);
                            }))
                    });
            } else {
                body = body.child(empty_note(
                    "This connection reports no person and the worker does not offer GitHub \
                     sign-in, so the session has no roster.",
                ));
            }
        } else if state.roster.is_empty() {
            body = body.child(empty_note("Loading people…"));
        }
        if let Some(identity) = &state.identity {
            body = body.child(
                div()
                    .id("signed-in-identity")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child(format!("Signed in as {}", identity.display_name)),
                    )
                    .child(
                        Button::new("sign-out-github")
                            .label("Sign out")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| this.sign_out_github(cx))),
                    ),
            );
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
        if self.session_permissions().can_manage && !state.credentials.is_empty() {
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
