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
                AccessResponse::SessionAccessRevoked {
                    session_id,
                    participant_id,
                }
            }
        };
        Ok(ServerResponse::Access(response))
    }
}
