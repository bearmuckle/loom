//! Multi-user requests: identity, roster and access, peer chat, ordered agent
//! directions, and the session's provider credential.

use loom_protocol::{
    AccessRequest, AccessRequestId, AccessResponse, CollaborationRequest, CollaborationResponse,
    CredentialId, ParticipantId, RunActionOutcome, SessionRole,
};

use super::*;
use crate::collaboration::{
    CollaborationSupport, ComposerRoute, ControlRoute, addressing_for, is_unsupported_error,
    route_composer, route_control,
};
use loom_core::RunControlAction;

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
                            view.collaboration.support = CollaborationSupport::Supported;
                        }
                        // An older worker without multi-user support: an
                        // operator connection with every control, using the
                        // legacy run requests.
                        Ok(_) | Err(_) => {
                            view.collaboration.identity_loaded = true;
                            view.collaboration.support = CollaborationSupport::Legacy;
                        }
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
                    // Owners see pending requests on session load.
                    if !view.collaboration.access.pending_loaded {
                        view.refresh_access_requests(cx);
                    }
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

    /// Sends composer text along the route its leading mention, role, and run
    /// state select. Returns `false` when nothing was sent, so the caller keeps
    /// the draft.
    pub(crate) fn send_composer_text(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        let permissions = self.collaboration.permissions();
        let addressing = addressing_for(&text, &self.collaboration.roster);
        let run = self
            .active_run
            .as_ref()
            .filter(|run| Some(run.id) == self.active_run_id);
        let revision = run.and_then(|run| self.collaboration.revision_for(run));
        let route = route_composer(
            permissions,
            addressing.to_people,
            run,
            self.pending_input.is_some(),
            revision,
        );
        // The mention itself is not part of an agent direction: only the text
        // after `@agent` is sent, while a people mention stays in the stored
        // peer message because it addresses it.
        let text = addressing.text;
        // Every connection that can act takes the ordered path when the worker
        // supports it, so operators are ordered against people too. Older
        // workers keep the existing run requests.
        let ordered = self.collaboration.uses_ordered_path();
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
                        self.consume_collaboration_entry(&entry);
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
        if !self.collaboration.observe_entry(entry) {
            return;
        }
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

    /// Approves, rejects, pauses, resumes, interrupts, or retries the active
    /// run. Uses the ordered `ControlRun` request against the last revision
    /// seen (read first when unknown); older workers get the legacy request.
    pub(crate) fn control_run(&mut self, action: RunControlAction, cx: &mut Context<Self>) {
        if !self.collaboration.permissions().can_act {
            self.clear_control_in_flight(&action);
            return;
        }
        let Some(run) = self
            .active_run
            .as_ref()
            .filter(|run| Some(run.id) == self.active_run_id)
            .cloned()
        else {
            self.clear_control_in_flight(&action);
            return;
        };
        let revision = self.collaboration.revision_for(&run);
        match route_control(
            self.collaboration.uses_ordered_path(),
            &run,
            revision,
            action.clone(),
        ) {
            ControlRoute::Legacy(request) => self.send_legacy_control(request, action, cx),
            ControlRoute::NeedRevision { run_id } => {
                self.fetch_revision_then_control(run_id, action, cx)
            }
            ControlRoute::Ordered(request) => {
                self.collaboration.conflict = None;
                let run = run.clone();
                self.dispatch(
                    cx,
                    ClientRequest::Collaboration(request),
                    move |view, response, cx| {
                        view.finish_ordered_control(response, &run, action, cx)
                    },
                );
            }
        }
    }

    fn send_legacy_control(
        &mut self,
        request: RunRequest,
        action: RunControlAction,
        cx: &mut Context<Self>,
    ) {
        let approval = is_approval(&action);
        self.dispatch(
            cx,
            ClientRequest::Run(request),
            move |view, response, cx| {
                if approval {
                    view.finish_approval_response(response, cx);
                } else {
                    view.finish_legacy_control_response(
                        crate::collaboration::control_label(&action),
                        response,
                        cx,
                    );
                }
            },
        );
    }

    fn fetch_revision_then_control(
        &mut self,
        run_id: RunId,
        action: RunControlAction,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::Collaboration(CollaborationRequest::GetRunRevision { run_id }),
            move |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::Collaboration(CollaborationResponse::RunRevision {
                        run_id,
                        revision,
                    })) => {
                        view.collaboration.observe_revision(run_id, revision);
                        let known = view
                            .active_run
                            .as_ref()
                            .filter(|run| run.id == run_id)
                            .and_then(|run| view.collaboration.revision_for(run))
                            .is_some();
                        if known {
                            view.control_run(action, cx);
                        } else {
                            // The run moved to another attempt meanwhile; let
                            // the person decide again.
                            view.clear_control_in_flight(&action);
                        }
                    }
                    Err(error) if is_unsupported_error(&error) => {
                        view.collaboration.support = CollaborationSupport::Legacy;
                        view.control_run(action, cx);
                    }
                    Err(error) => {
                        view.clear_control_in_flight(&action);
                        view.record_backend_error("read run revision", error);
                    }
                    Ok(response) => {
                        view.clear_control_in_flight(&action);
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

    /// Applies the outcome of an ordered run control. A conflict adopts the
    /// current revision and run and shows the notice; the action is not
    /// applied or retried.
    pub(crate) fn finish_ordered_control(
        &mut self,
        response: ResponseEnvelope,
        run: &AgentRunSnapshot,
        action: RunControlAction,
        cx: &mut Context<Self>,
    ) {
        let operation = crate::collaboration::control_label(&action);
        match response.result {
            Ok(ServerResponse::Collaboration(CollaborationResponse::RunAction(outcome))) => {
                match *outcome {
                    RunActionOutcome::Accepted {
                        entry,
                        revision,
                        run,
                    } => {
                        self.collaboration.observe_revision(run.id, revision);
                        self.consume_collaboration_entry(&entry);
                        self.adopt_run(run);
                        self.start_run_polling(cx);
                    }
                    RunActionOutcome::Conflict {
                        expected,
                        current,
                        run,
                    } => {
                        self.clear_control_in_flight(&action);
                        self.collaboration
                            .record_control_conflict(run.id, expected, current, action);
                        self.adopt_run(run);
                    }
                }
            }
            Err(error) if is_unsupported_error(&error) => {
                // The worker does not know the ordered request after all.
                self.collaboration.support = CollaborationSupport::Legacy;
                let request = crate::collaboration::legacy_control_request(run, action.clone());
                self.send_legacy_control(request, action, cx);
            }
            Err(error) => {
                self.clear_control_in_flight(&action);
                self.record_backend_error(operation, error);
            }
            Ok(response) => {
                self.clear_control_in_flight(&action);
                self.record_backend_error(operation, unexpected_response(operation, response));
            }
        }
        cx.notify();
    }

    fn clear_control_in_flight(&mut self, action: &RunControlAction) {
        if is_approval(action) {
            self.approval_request_in_flight = false;
        }
    }

    /// Reads the session's retained attributed history and applies it, so
    /// peer chat and attribution survive a reload or reconnect. Entries
    /// already applied from live events are skipped.
    pub(crate) fn load_collaboration_history(&mut self, cx: &mut Context<Self>) {
        if self.collaboration.support == CollaborationSupport::Legacy {
            return;
        }
        let session_id = self.active_session.id;
        let generation = self.transcript_generation;
        self.load_collaboration_page(session_id, generation, None, cx);
    }

    fn load_collaboration_page(
        &mut self,
        session_id: AgentSessionId,
        generation: u64,
        after_sequence: Option<loom_core::EventSequence>,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::Collaboration(CollaborationRequest::ListSessionCollaboration {
                session_id,
                after_sequence,
                limit: loom_protocol::MAX_SESSION_COLLABORATION_PAGE_SIZE,
            }),
            move |view, response, cx| {
                if view.active_session.id != session_id || view.transcript_generation != generation
                {
                    return;
                }
                // An older worker has no history read; live events still apply.
                if let Ok(ServerResponse::Collaboration(
                    CollaborationResponse::SessionCollaboration {
                        entries,
                        next_after_sequence,
                        ..
                    },
                )) = response.result
                {
                    view.apply_collaboration_history(entries.into_iter().map(|(_, entry)| entry));
                    if let Some(next) = next_after_sequence
                        && Some(next) != after_sequence
                    {
                        view.load_collaboration_page(session_id, generation, Some(next), cx);
                    }
                }
                cx.notify();
            },
        );
    }

    /// Applies attributed entries read from history, oldest first.
    pub(crate) fn apply_collaboration_history(
        &mut self,
        entries: impl IntoIterator<Item = loom_core::CollaborationEntry>,
    ) {
        for entry in entries {
            self.consume_collaboration_entry(&entry);
        }
    }

    /// Refreshes everything the People tab shows.
    pub(crate) fn refresh_people(&mut self, cx: &mut Context<Self>) {
        let session_id = self.active_session.id;
        self.refresh_roster(session_id, cx);
        self.refresh_participant_directory(cx);
        self.refresh_session_credential(cx);
        self.refresh_access_requests(cx);
    }

    /// Reads the pending access requests an owner decides on.
    pub(crate) fn refresh_access_requests(&mut self, cx: &mut Context<Self>) {
        if !self.collaboration.manages_access_requests() {
            return;
        }
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::Access(AccessRequest::ListSessionAccessRequests { session_id }),
            move |view, response, cx| {
                if view.active_session.id != session_id {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::Access(AccessResponse::SessionAccessRequests {
                        requests,
                        ..
                    })) => {
                        view.collaboration.access.pending_loaded = true;
                        view.collaboration.access.replace_pending(requests);
                    }
                    // An older worker: hide the access request controls.
                    Err(error) if is_unsupported_error(&error) => {
                        view.collaboration.access.unsupported = true;
                    }
                    Err(error) => view.record_backend_error("list access requests", error),
                    Ok(response) => view.record_backend_error(
                        "list access requests",
                        unexpected_response("list access requests", response),
                    ),
                }
                cx.notify();
            },
        );
    }

    pub(crate) fn set_access_request_role(&mut self, role: SessionRole, cx: &mut Context<Self>) {
        self.collaboration.access.draft_role = Some(role);
        cx.notify();
    }

    /// Asks the session's owners for the chosen role, with an optional note.
    pub(crate) fn request_session_access(&mut self, note: &str, cx: &mut Context<Self>) {
        if !self.collaboration.can_request_access() || self.collaboration.access.in_flight {
            return;
        }
        let note = match crate::collaboration::access_request_note(note) {
            Ok(note) => note,
            Err(message) => {
                self.record_backend_error(
                    "request access",
                    loom_core::LoomError::invalid_request(message),
                );
                cx.notify();
                return;
            }
        };
        let session_id = self.active_session.id;
        self.collaboration.access.in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::Access(AccessRequest::RequestSessionAccess {
                session_id,
                requested_role: self.collaboration.access.draft_role(),
                note,
            }),
            move |view, response, cx| {
                view.collaboration.access.in_flight = false;
                match response.result {
                    Ok(ServerResponse::Access(AccessResponse::SessionAccessRequestPending(
                        request,
                    ))) => {
                        if view.active_session.id == session_id {
                            view.collaboration.access.record_own(request);
                            view.clear_access_note = true;
                        }
                    }
                    Err(error) if is_unsupported_error(&error) => {
                        view.collaboration.access.unsupported = true;
                    }
                    Err(error) => view.record_backend_error("request access", error),
                    Ok(response) => view.record_backend_error(
                        "request access",
                        unexpected_response("request access", response),
                    ),
                }
                cx.notify();
            },
        );
    }

    /// Withdraws the caller's pending request.
    pub(crate) fn withdraw_session_access_request(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.collaboration.access.own.clone() else {
            return;
        };
        self.send_access_decision(
            "withdraw request",
            AccessRequest::WithdrawSessionAccessRequest {
                session_id: request.session_id,
                request_id: request.id,
            },
            cx,
        );
    }

    /// Approves a pending request with the role the owner picked.
    pub(crate) fn approve_session_access_request(
        &mut self,
        request_id: AccessRequestId,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.pending_access_request(request_id) else {
            return;
        };
        let role = self.collaboration.access.approve_role(&request);
        self.send_access_decision(
            "approve request",
            AccessRequest::ApproveSessionAccessRequest {
                session_id: request.session_id,
                request_id,
                role: Some(role),
            },
            cx,
        );
    }

    /// Denies a pending request with an optional reason.
    pub(crate) fn deny_session_access_request(
        &mut self,
        request_id: AccessRequestId,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.pending_access_request(request_id) else {
            return;
        };
        let reason = reason.trim();
        self.send_access_decision(
            "deny request",
            AccessRequest::DenySessionAccessRequest {
                session_id: request.session_id,
                request_id,
                reason: (!reason.is_empty()).then(|| reason.to_owned()),
            },
            cx,
        );
    }

    fn pending_access_request(
        &self,
        request_id: AccessRequestId,
    ) -> Option<loom_protocol::SessionAccessRequest> {
        if !self.collaboration.manages_access_requests() {
            return None;
        }
        self.collaboration
            .access
            .pending
            .iter()
            .find(|request| request.id == request_id)
            .cloned()
    }

    fn send_access_decision(
        &mut self,
        operation: &'static str,
        request: AccessRequest,
        cx: &mut Context<Self>,
    ) {
        if self.collaboration.access.in_flight {
            return;
        }
        self.collaboration.access.in_flight = true;
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::Access(request),
            move |view, response, cx| {
                view.collaboration.access.in_flight = false;
                view.finish_access_decision(operation, session_id, response, cx);
            },
        );
    }

    /// Applies the answer to an approve, deny, or withdraw request.
    pub(crate) fn finish_access_decision(
        &mut self,
        operation: &'static str,
        session_id: AgentSessionId,
        response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        match response.result {
            Ok(ServerResponse::Access(AccessResponse::SessionAccessRequestResolved {
                request,
                decision,
                ..
            })) => {
                self.collaboration.apply_access_resolved(
                    self.active_session.id,
                    &request,
                    &decision,
                );
                self.clear_access_deny_reason = Some(request.id);
                if self.active_session.id == session_id
                    && matches!(
                        decision,
                        loom_protocol::AccessRequestDecision::Approved { .. }
                    )
                {
                    self.refresh_roster(session_id, cx);
                }
            }
            Err(error) => {
                // The request may have been decided elsewhere; re-read the list.
                if self.active_session.id == session_id {
                    self.refresh_access_requests(cx);
                }
                self.record_backend_error(operation, error);
            }
            Ok(response) => {
                self.record_backend_error(operation, unexpected_response(operation, response))
            }
        }
        cx.notify();
    }

    /// Whether a repository may be searched or attached from this connection.
    pub(crate) fn can_manage_sources(&self) -> bool {
        self.collaboration.permissions().can_manage
    }
}

/// Whether a control answers a pending approval, which has its own in-flight
/// flag.
fn is_approval(action: &RunControlAction) -> bool {
    matches!(
        action,
        RunControlAction::Approve { .. } | RunControlAction::Reject { .. }
    )
}
