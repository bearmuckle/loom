//! Multi-user access requests: identity, roster, and owner-only grants.

use loom_protocol::{AccessRequest, AccessRequestDecision, AccessResponse};

use super::*;

impl InProcessConnection {
    pub(super) fn access_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        let ClientRequest::Access(request) = request else {
            unreachable!("request was routed to the wrong dispatch domain");
        };
        let response = match request {
            AccessRequest::GetCurrentParticipant => AccessResponse::CurrentParticipant {
                participant: self.participant(),
            },
            AccessRequest::ListParticipants => AccessResponse::Participants {
                participants: self.backend.access.participants(),
            },
            AccessRequest::GetSessionRoster { session_id } => {
                self.backend.sessions()?.get(session_id)?;
                AccessResponse::SessionRoster {
                    session_id,
                    entries: self.backend.access.roster(session_id),
                }
            }
            AccessRequest::GrantSessionAccess {
                session_id,
                participant_id,
            } => {
                self.backend.sessions()?.get(session_id)?;
                let grant = self.backend.access.grant(
                    session_id,
                    participant_id,
                    self.participant().map(|participant| participant.id),
                )?;
                log::info!(
                    "[loom-server] granted participant {participant_id} {} access to session \
                     {session_id}",
                    grant.role.as_str()
                );
                self.record_access_change(session_id, participant_id, Some(grant.role))?;
                AccessResponse::SessionGrant(grant)
            }
            AccessRequest::SetSessionRole {
                session_id,
                participant_id,
                role,
            } => {
                self.backend.sessions()?.get(session_id)?;
                let grant = self.backend.access.set_role(
                    session_id,
                    participant_id,
                    role,
                    self.participant().map(|participant| participant.id),
                )?;
                log::info!(
                    "[loom-server] participant {participant_id} is now {} in session {session_id}",
                    grant.role.as_str()
                );
                self.record_access_change(session_id, participant_id, Some(grant.role))?;
                AccessResponse::SessionGrant(grant)
            }
            AccessRequest::RevokeSessionAccess {
                session_id,
                participant_id,
            } => {
                self.backend.sessions()?.get(session_id)?;
                self.backend.access.revoke(session_id, participant_id)?;
                log::info!(
                    "[loom-server] revoked participant {participant_id} from session {session_id}"
                );
                self.record_access_change(session_id, participant_id, None)?;
                AccessResponse::SessionAccessRevoked {
                    session_id,
                    participant_id,
                }
            }
            AccessRequest::ListProviderCredentials => AccessResponse::ProviderCredentials {
                // A person sees only their own credentials; an operator sees
                // every credential on the worker.
                credentials: self
                    .backend
                    .provider_credentials
                    .list(self.participant().map(|participant| participant.id)),
            },
            AccessRequest::SelectSessionCredential {
                session_id,
                credential_id,
            } => {
                self.backend.sessions()?.get(session_id)?;
                let selection = self.backend.provider_credentials.select(
                    self.backend.providers.instance_credentials()?.as_ref(),
                    session_id,
                    credential_id,
                    self.participant().map(|participant| participant.id),
                )?;
                log::info!(
                    "[loom-server] session {session_id} selected provider credential \
                     {credential_id} for later runs"
                );
                let credential = self.backend.provider_credentials.get(credential_id);
                AccessResponse::SessionCredential {
                    session_id,
                    selection: Some(selection),
                    credential,
                }
            }
            AccessRequest::GetSessionCredential { session_id } => {
                self.backend.sessions()?.get(session_id)?;
                let (selection, credential) = self
                    .backend
                    .provider_credentials
                    .selection(session_id)
                    .map_or((None, None), |(selection, credential)| {
                        (Some(selection), credential)
                    });
                AccessResponse::SessionCredential {
                    session_id,
                    selection,
                    credential,
                }
            }
            AccessRequest::GetRunCredential { run_id } => {
                self.run_summary(run_id)?;
                AccessResponse::RunCredential {
                    run_id,
                    pin: self.backend.provider_credentials.run_pin(run_id),
                }
            }
            AccessRequest::RevokeProviderCredential { credential_id } => {
                self.backend.provider_credentials.revoke(
                    self.backend.providers.instance_credentials()?.as_ref(),
                    credential_id,
                    self.participant().map(|participant| participant.id),
                )?;
                log::info!("[loom-server] revoked provider credential {credential_id}");
                AccessResponse::ProviderCredentialRevoked { credential_id }
            }
            AccessRequest::RequestSessionAccess {
                session_id,
                requested_role,
                note,
            } => {
                let requester = self.participant().ok_or_else(|| {
                    LoomError::invalid_request(
                        "requesting access needs a connection bound to a person",
                    )
                })?;
                self.backend.sessions()?.get(session_id)?;
                let request = self.backend.access.request_access(
                    session_id,
                    &requester,
                    requested_role,
                    note,
                )?;
                log::info!(
                    "[loom-server] participant {} requested {} access to session {session_id}",
                    requester.id,
                    requested_role.as_str()
                );
                self.backend.journal()?.append_server_event(
                    CURRENT_PROTOCOL_VERSION,
                    session_id,
                    ServerEvent::SessionAccessRequested {
                        request: request.clone(),
                    },
                );
                self.backend.persist_state()?;
                AccessResponse::SessionAccessRequestPending(request)
            }
            AccessRequest::ListSessionAccessRequests { session_id } => {
                self.backend.sessions()?.get(session_id)?;
                AccessResponse::SessionAccessRequests {
                    session_id,
                    requests: self.backend.access.access_requests(session_id),
                }
            }
            AccessRequest::ApproveSessionAccessRequest {
                session_id,
                request_id,
                role,
            } => {
                self.backend.sessions()?.get(session_id)?;
                let request = self.backend.access.access_request(session_id, request_id)?;
                let role = role.unwrap_or(request.requested_role);
                let grant = self.backend.access.grant_role(
                    session_id,
                    request.participant.id,
                    role,
                    self.participant().map(|participant| participant.id),
                )?;
                log::info!(
                    "[loom-server] approved access request {request_id}: participant {} is now \
                     {} in session {session_id}",
                    request.participant.id,
                    grant.role.as_str()
                );
                let decision = AccessRequestDecision::Approved { role: grant.role };
                self.resolve_access_request(&request, decision.clone(), true)?;
                AccessResponse::SessionAccessRequestResolved {
                    request,
                    decision,
                    grant: Some(grant),
                }
            }
            AccessRequest::DenySessionAccessRequest {
                session_id,
                request_id,
                reason,
            } => {
                self.backend.sessions()?.get(session_id)?;
                let reason = reason
                    .map(|reason| reason.trim().to_owned())
                    .filter(|reason| !reason.is_empty());
                if reason
                    .as_ref()
                    .is_some_and(|reason| reason.len() > loom_core::MAX_ACCESS_REQUEST_NOTE_BYTES)
                {
                    return Err(LoomError::invalid_request(format!(
                        "denial reason exceeds {} bytes",
                        loom_core::MAX_ACCESS_REQUEST_NOTE_BYTES
                    )));
                }
                let request = self.backend.access.access_request(session_id, request_id)?;
                log::info!(
                    "[loom-server] denied access request {request_id} in session {session_id}"
                );
                let decision = AccessRequestDecision::Denied { reason };
                self.resolve_access_request(&request, decision.clone(), true)?;
                AccessResponse::SessionAccessRequestResolved {
                    request,
                    decision,
                    grant: None,
                }
            }
            AccessRequest::WithdrawSessionAccessRequest {
                session_id,
                request_id,
            } => {
                let requester = self.participant().ok_or_else(|| {
                    LoomError::invalid_request(
                        "withdrawing a request needs a connection bound to a person",
                    )
                })?;
                let request = self.backend.access.access_request(session_id, request_id)?;
                if request.participant.id != requester.id {
                    // Someone else's request is reported as missing, so its
                    // existence does not leak.
                    return Err(LoomError::not_found("access request", request_id));
                }
                let decision = AccessRequestDecision::Withdrawn;
                self.resolve_access_request(&request, decision.clone(), false)?;
                AccessResponse::SessionAccessRequestResolved {
                    request,
                    decision,
                    grant: None,
                }
            }
            AccessRequest::StartGitHubSignIn => return self.start_github_sign_in(),
            AccessRequest::GetGitHubSignInStatus { login_id } => {
                return self.github_sign_in_status(&login_id);
            }
            AccessRequest::SignOutGitHub => return self.sign_out_github(),
        };
        Ok(ServerResponse::Access(response))
    }

    /// Removes a decided or withdrawn request, notifies the owners and the
    /// requester, and, for an owner's decision, records it as an attributed
    /// history entry.
    fn resolve_access_request(
        &self,
        request: &loom_core::SessionAccessRequest,
        decision: AccessRequestDecision,
        attributed: bool,
    ) -> Result<()> {
        let session_id = request.session_id;
        self.backend.access.remove_access_request(request.id);
        if attributed {
            self.backend
                .record_collaboration(loom_core::CollaborationEntry {
                    session_id,
                    run_id: None,
                    author: self.author(session_id)?,
                    recipient: loom_core::Recipient::People,
                    kind: loom_core::CollaborationEntryKind::AccessRequestDecided {
                        request_id: request.id,
                        participant: request.participant.clone(),
                        requested_role: request.requested_role,
                        decision: decision.clone(),
                    },
                    revision: None,
                    created_at: Timestamp::now(),
                })?;
        }
        self.backend.journal()?.append_server_event(
            CURRENT_PROTOCOL_VERSION,
            session_id,
            ServerEvent::SessionAccessRequestResolved {
                request: request.clone(),
                decision: decision.clone(),
            },
        );
        if matches!(decision, AccessRequestDecision::Approved { .. }) {
            self.backend.publish_roster(session_id);
        }
        self.backend.persist_state()
    }

    /// Records an attributed access change in the session history and pushes
    /// the new roster to the session's readers.
    fn record_access_change(
        &self,
        session_id: AgentSessionId,
        participant_id: loom_core::ParticipantId,
        role: Option<loom_core::SessionRole>,
    ) -> Result<()> {
        self.backend
            .record_collaboration(loom_core::CollaborationEntry {
                session_id,
                run_id: None,
                author: self.author(session_id)?,
                recipient: loom_core::Recipient::People,
                kind: loom_core::CollaborationEntryKind::AccessChanged {
                    participant: self.backend.access.identity(participant_id),
                    role,
                },
                revision: None,
                created_at: Timestamp::now(),
            })?;
        self.backend.publish_roster(session_id);
        self.backend.persist_state()
    }
}
