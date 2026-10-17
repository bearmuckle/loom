//! Multi-user requests: identity, roster and access, peer chat, ordered agent
//! directions, and the session's provider credential.

use loom_protocol::{
    AccessRequest, AccessResponse, CollaborationRequest, CollaborationResponse, CredentialId,
    ParticipantId, RunActionOutcome, SessionRole,
};

use super::*;
use crate::collaboration::{ComposerRecipient, ComposerRoute, route_composer};

impl LoomView {
    /// Loads who this connection is, then the active session's roster and
    /// credential. Called on session selection.
    pub(crate) fn refresh_collaboration(&mut self, cx: &mut Context<Self>) {
        let session_id = self.active_session.id;
        if !self.collaboration.identity_loaded {
            self.dispatch(
                cx,
                ClientRequest::Access(AccessRequest::GetCurrentParticipant),
                |view, response, cx| {
                    match response.result {
                        Ok(ServerResponse::Access(AccessResponse::CurrentParticipant {
                            participant,
                        })) => {
                            view.collaboration.identity_loaded = true;
                            view.collaboration.identity = participant;
                        }
                        // An older worker without multi-user support: an
                        // operator connection with every control.
                        Ok(_) | Err(_) => view.collaboration.identity_loaded = true,
                    }
                    cx.notify();
                },
            );
        }
        self.refresh_roster(session_id, cx);
        self.refresh_session_credential(cx);
    }

    pub(crate) fn refresh_roster(&mut self, session_id: AgentSessionId, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::Access(AccessRequest::GetSessionRoster { session_id }),
            move |view, response, cx| {
                if view.active_session.id != session_id {
                    return;
                }
                if let Ok(ServerResponse::Access(AccessResponse::SessionRoster {
                    entries, ..
                })) = response.result
                {
                    view.collaboration.replace_roster(session_id, entries);
                    view.schedule_run_poll(cx);
                }
                cx.notify();
            },
        );
    }

    /// Loads the people directory an owner grants access from.
    pub(crate) fn refresh_participant_directory(&mut self, cx: &mut Context<Self>) {
        if !self.collaboration.permissions().can_manage {
            return;
        }
        self.dispatch(
            cx,
            ClientRequest::Access(AccessRequest::ListParticipants),
            |view, response, cx| {
                if let Ok(ServerResponse::Access(AccessResponse::Participants { participants })) =
                    response.result
                {
                    view.collaboration.directory = participants;
                }
                cx.notify();
            },
        );
    }

    pub(crate) fn grant_session_access(
        &mut self,
        participant_id: ParticipantId,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.active_session.id;
        self.send_access_change(
            "grant access",
            AccessRequest::GrantSessionAccess {
                session_id,
                participant_id,
            },
            cx,
        );
    }

    pub(crate) fn set_session_role(
        &mut self,
        participant_id: ParticipantId,
        role: SessionRole,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.active_session.id;
        self.send_access_change(
            "change role",
            AccessRequest::SetSessionRole {
                session_id,
                participant_id,
                role,
            },
            cx,
        );
    }

    pub(crate) fn revoke_session_access(
        &mut self,
        participant_id: ParticipantId,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.active_session.id;
        self.send_access_change(
            "revoke access",
            AccessRequest::RevokeSessionAccess {
                session_id,
                participant_id,
            },
            cx,
        );
    }

    fn send_access_change(
        &mut self,
        operation: &'static str,
        request: AccessRequest,
        cx: &mut Context<Self>,
    ) {
        if !self.collaboration.permissions().can_manage {
            return;
        }
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::Access(request),
            move |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::Access(
                        AccessResponse::SessionGrant(_)
                        | AccessResponse::SessionAccessRevoked { .. },
                    )) => {}
                    Err(error) => view.record_backend_error(operation, error),
                    Ok(response) => view
                        .record_backend_error(operation, unexpected_response(operation, response)),
                }
                // The roster event follows too; reading it here keeps the panel
                // current on workers that do not push one to this connection.
                if view.active_session.id == session_id {
                    view.refresh_roster(session_id, cx);
                }
                cx.notify();
            },
        );
    }

    /// Reads the session's selected credential, the credentials an owner may
    /// pick, and the active run's pinned credential.
    pub(crate) fn refresh_session_credential(&mut self, cx: &mut Context<Self>) {
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::Access(AccessRequest::GetSessionCredential { session_id }),
            move |view, response, cx| {
                if view.active_session.id != session_id {
                    return;
                }
                if let Ok(ServerResponse::Access(AccessResponse::SessionCredential {
                    credential,
                    ..
                })) = response.result
                {
                    view.collaboration.session_credential = credential;
                    view.collaboration.credential_loaded = true;
                }
                cx.notify();
            },
        );
        if let Some(run_id) = self.active_run_id {
            self.dispatch(
                cx,
                ClientRequest::Access(AccessRequest::GetRunCredential { run_id }),
                move |view, response, cx| {
                    if let Ok(ServerResponse::Access(AccessResponse::RunCredential {
                        run_id,
                        pin,
                    })) = response.result
                        && view.active_run_id == Some(run_id)
                    {
                        view.collaboration.run_pin = Some((run_id, pin));
                    }
                    cx.notify();
                },
            );
        }
        if self.collaboration.permissions().can_manage {
            self.dispatch(
                cx,
                ClientRequest::Access(AccessRequest::ListProviderCredentials),
                |view, response, cx| {
                    if let Ok(ServerResponse::Access(AccessResponse::ProviderCredentials {
                        credentials,
                    })) = response.result
                    {
                        view.collaboration.credentials = credentials;
                    }
                    cx.notify();
                },
            );
        }
    }

    pub(crate) fn select_session_credential(
        &mut self,
        credential_id: CredentialId,
        cx: &mut Context<Self>,
    ) {
        if !self.collaboration.permissions().can_manage {
            return;
        }
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::Access(AccessRequest::SelectSessionCredential {
                session_id,
                credential_id,
            }),
            move |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::Access(AccessResponse::SessionCredential {
                        credential,
                        ..
                    })) if view.active_session.id == session_id => {
                        view.collaboration.session_credential = credential;
                        view.collaboration.credential_loaded = true;
                    }
                    Ok(_) => {}
                    Err(error) => view.record_backend_error("select credential", error),
                }
                cx.notify();
            },
        );
    }

    pub(crate) fn set_composer_recipient(
        &mut self,
        recipient: ComposerRecipient,
        cx: &mut Context<Self>,
    ) {
        self.collaboration.recipient = recipient;
        cx.notify();
    }

    /// Sends composer text along the route the recipient, role, and run state
    /// select. Returns `false` when nothing was sent, so the caller keeps the
    /// draft.
    pub(crate) fn send_composer_text(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        let permissions = self.collaboration.permissions();
        let run = self
            .active_run
            .as_ref()
            .filter(|run| Some(run.id) == self.active_run_id);
        let revision = run.and_then(|run| self.collaboration.revision_for(run));
        let route = route_composer(
            permissions,
            self.collaboration.recipient,
            run,
            self.pending_input.is_some(),
            revision,
        );
        // Only a person sharing the session needs the ordered path; an
        // operator alone keeps the existing run requests.
        let ordered = self.collaboration.is_participant_bound();
        match route {
            ComposerRoute::ReadOnly => {
                self.record_status("Observers can read this session but not send messages.");
                false
            }
            ComposerRoute::PeerMessage => {
                self.send_peer_message(text, cx);
                true
            }
            ComposerRoute::StartRun => {
                self.send_message(text, cx);
                true
            }
            _ if !ordered => {
                self.send_message(text, cx);
                true
            }
            ComposerRoute::NeedRevision { run_id } => {
                self.fetch_revision_then_send(run_id, text, cx);
                true
            }
            ComposerRoute::DirectAgent { run_id, expected } => {
                self.send_ordered(
                    CollaborationRequest::DirectAgent {
                        run_id,
                        expected,
                        text: text.clone(),
                    },
                    text,
                    cx,
                );
                true
            }
            ComposerRoute::AnswerAgentQuestion { run_id, expected } => {
                self.send_ordered(
                    CollaborationRequest::AnswerAgentQuestion {
                        run_id,
                        expected,
                        text: text.clone(),
                    },
                    text,
                    cx,
                );
                true
            }
        }
    }

    pub(crate) fn send_peer_message(&mut self, text: String, cx: &mut Context<Self>) {
        let session_id = self.active_session.id;
        self.collaboration.request_in_flight = true;
        let draft = text.clone();
        self.dispatch(
            cx,
            ClientRequest::Collaboration(CollaborationRequest::SendPeerMessage {
                session_id,
                text,
            }),
            move |view, response, cx| {
                view.collaboration.request_in_flight = false;
                match response.result {
                    Ok(ServerResponse::Collaboration(CollaborationResponse::PeerMessageSent {
                        entry,
                    })) => {
                        if view.active_session.id == session_id {
                            view.consume_collaboration_entry(&entry);
                        }
                    }
                    Err(error) => {
                        view.collaboration.restore_draft = Some(draft);
                        view.record_backend_error("send to people", error);
                    }
                    Ok(response) => view.record_backend_error(
                        "send to people",
                        unexpected_response("send to people", response),
                    ),
                }
                cx.notify();
            },
        );
    }

    fn fetch_revision_then_send(&mut self, run_id: RunId, text: String, cx: &mut Context<Self>) {
        self.collaboration.request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::Collaboration(CollaborationRequest::GetRunRevision { run_id }),
            move |view, response, cx| {
                view.collaboration.request_in_flight = false;
                match response.result {
                    Ok(ServerResponse::Collaboration(CollaborationResponse::RunRevision {
                        run_id,
                        revision,
                    })) => {
                        view.collaboration.observe_revision(run_id, revision);
                        let has_revision = view
                            .active_run
                            .as_ref()
                            .and_then(|run| view.collaboration.revision_for(run))
                            .is_some();
                        if has_revision {
                            view.send_composer_text(text, cx);
                        } else {
                            view.collaboration.restore_draft = Some(text);
                        }
                    }
                    Err(error) => {
                        view.collaboration.restore_draft = Some(text);
                        view.record_backend_error("read run revision", error);
                    }
                    Ok(response) => {
                        view.collaboration.restore_draft = Some(text);
                        view.record_backend_error(
                            "read run revision",
                            unexpected_response("read run revision", response),
                        );
                    }
                }
                cx.notify();
            },
        );
    }

    fn send_ordered(
        &mut self,
        request: CollaborationRequest,
        draft: String,
        cx: &mut Context<Self>,
    ) {
        self.sending_message = true;
        self.collaboration.conflict = None;
        self.optimistic_messages.push(draft.clone());
        self.timeline.push(TimelineItem::User(draft.clone()));
        self.dispatch(
            cx,
            ClientRequest::Collaboration(request),
            move |view, response, cx| view.finish_ordered_send(response, draft, cx),
        );
    }

    pub(crate) fn finish_ordered_send(
        &mut self,
        response: ResponseEnvelope,
        draft: String,
        cx: &mut Context<Self>,
    ) {
        self.sending_message = false;
        match response.result {
            Ok(ServerResponse::Collaboration(CollaborationResponse::RunAction(outcome))) => {
                match *outcome {
                    RunActionOutcome::Accepted {
                        entry,
                        revision,
                        run,
                    } => {
                        self.collaboration.observe_revision(run.id, revision);
                        self.collaboration.observe_entry(&entry);
                        self.adopt_run(run);
                        self.pending_input = None;
                        self.start_run_polling(cx);
                    }
                    RunActionOutcome::Conflict {
                        expected,
                        current,
                        run,
                    } => {
                        self.withdraw_optimistic(&draft);
                        self.collaboration
                            .record_conflict(run.id, expected, current, draft);
                        self.adopt_run(run);
                    }
                }
            }
            Err(error) => {
                self.withdraw_optimistic(&draft);
                self.collaboration.restore_draft = Some(draft);
                self.record_backend_error("send to agent", error);
            }
            Ok(response) => {
                self.withdraw_optimistic(&draft);
                self.collaboration.restore_draft = Some(draft);
                self.record_backend_error(
                    "send to agent",
                    unexpected_response("send to agent", response),
                );
            }
        }
        cx.notify();
    }

    /// Removes the optimistic row for a send the server did not apply.
    fn withdraw_optimistic(&mut self, draft: &str) {
        if let Some(index) = self
            .optimistic_messages
            .iter()
            .rposition(|pending| pending == draft)
        {
            self.optimistic_messages.remove(index);
        }
        if let Some(index) = self
            .timeline
            .iter()
            .rposition(|item| matches!(item, TimelineItem::User(text) if text == draft))
        {
            self.timeline.remove(index);
        }
    }

    fn adopt_run(&mut self, run: AgentRunSnapshot) {
        if self.active_run_id.is_some_and(|id| id != run.id) {
            return;
        }
        self.active_run_id = Some(run.id);
        self.run_state = Some(run.state);
        self.session_state = session_state_for_run(run.state);
        self.active_session.state = self.session_state;
        self.active_run = Some(run);
    }

    /// Applies one attributed history entry: agent-directed messages
    /// attribute the matching user row, everything else becomes its own row.
    pub(crate) fn consume_collaboration_entry(&mut self, entry: &loom_core::CollaborationEntry) {
        if entry.session_id != self.active_session.id {
            return;
        }
        self.collaboration.observe_entry(entry);
        match &entry.kind {
            loom_core::CollaborationEntryKind::AgentDirection { .. }
            | loom_core::CollaborationEntryKind::AgentAnswer { .. } => {}
            _ => {
                let duplicate = self.timeline.iter().any(|item| {
                    matches!(item, TimelineItem::Collaboration(existing) if **existing == *entry)
                });
                if !duplicate {
                    self.timeline
                        .push(TimelineItem::Collaboration(Box::new(entry.clone())));
                }
            }
        }
    }

    /// Refreshes everything the People tab shows.
    pub(crate) fn refresh_people(&mut self, cx: &mut Context<Self>) {
        let session_id = self.active_session.id;
        self.refresh_roster(session_id, cx);
        self.refresh_participant_directory(cx);
        self.refresh_session_credential(cx);
    }

    /// Whether a repository may be searched or attached from this connection.
    pub(crate) fn can_manage_sources(&self) -> bool {
        self.collaboration.permissions().can_manage
    }
}
