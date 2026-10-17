//! Multi-user access requests: identity, roster, and owner-only grants.

use loom_protocol::{AccessRequest, AccessResponse};

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
                participant: self.participant().cloned(),
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
        };
        Ok(ServerResponse::Access(response))
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
