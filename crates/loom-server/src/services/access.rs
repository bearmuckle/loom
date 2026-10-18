//! Multi-user identity, session grants, and presence.
//!
//! The service owns three things:
//!
//! - the participant directory: people the backend knows, keyed by the stable
//!   [`ParticipantId`] the server derived from their credential;
//! - session grants: each person's [`SessionRole`] in each session, saved with
//!   the rest of the durable state so they survive a restart;
//! - the presence registry: one entry per live authenticated connection, which
//!   the roster aggregates into one entry per person.
//!
//! Presence policy (also documented on [`PresenceStatus`]): a connection is
//! live while it shows transport liveness within
//! [`PresenceStatus::STALE_AFTER`]; the remote listener refreshes liveness on
//! every inbound frame, ping, and pong, and an in-process connection on every
//! request. A stale connection counts as gone even if its socket was never
//! closed, so a dropped network cannot leave a person shown online. A
//! reconnect registers a new connection for the same person, which the roster
//! merges with any other, so it never duplicates an entry.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Instant,
};

use loom_core::{
    AccessRequestId, AgentSessionId, ErrorCode, LoomError, MAX_ACCESS_REQUEST_NOTE_BYTES,
    ParticipantId, ParticipantIdentity, PresenceStatus, Result, RosterEntry, SessionAccessRequest,
    SessionGrant, SessionRole, Timestamp,
};
use tokio::sync::Notify;

#[derive(Default)]
pub(crate) struct AccessService {
    state: Mutex<AccessState>,
    /// Called, outside the state lock, with the sessions whose roster changed
    /// because a person connected or disconnected.
    roster_listener: Mutex<Option<RosterListener>>,
}

pub(crate) type RosterListener = Arc<dyn Fn(Vec<AgentSessionId>) + Send + Sync>;

#[derive(Default)]
struct AccessState {
    participants: BTreeMap<ParticipantId, String>,
    grants: BTreeMap<AgentSessionId, BTreeMap<ParticipantId, SessionGrant>>,
    /// Pending access requests; at most one per person and session.
    requests: BTreeMap<AccessRequestId, SessionAccessRequest>,
    connections: BTreeMap<u64, ConnectionState>,
    last_active: BTreeMap<ParticipantId, Timestamp>,
    next_connection: u64,
}

struct ConnectionState {
    participant: ParticipantId,
    last_seen: Instant,
    last_request: Option<Instant>,
    revoked: Arc<Notify>,
}

/// One live connection bound to a person.
///
/// Dropping the last clone of the connection that owns it removes it from the
/// presence registry, which is how a disconnect updates the roster.
pub struct ConnectionPresence {
    id: u64,
    participant: ParticipantIdentity,
    service: Arc<AccessService>,
    revoked: Arc<Notify>,
}

impl std::fmt::Debug for ConnectionPresence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionPresence")
            .field("id", &self.id)
            .field("participant", &self.participant)
            .finish_non_exhaustive()
    }
}

impl ConnectionPresence {
    pub fn participant(&self) -> &ParticipantIdentity {
        &self.participant
    }

    /// Records transport liveness (a frame, ping, or pong).
    pub fn touch(&self) {
        self.service.touch(self.id, false);
    }

    /// Records a request, which is both liveness and activity.
    pub fn touch_request(&self) {
        self.service.touch(self.id, true);
    }

    /// Signalled when the person loses access to a session, so a transport can
    /// end this connection and every live subscription it carries.
    pub fn revoked(&self) -> Arc<Notify> {
        Arc::clone(&self.revoked)
    }
}

impl Drop for ConnectionPresence {
    fn drop(&mut self) {
        let sessions = {
            let mut state = self.service.state();
            state.connections.remove(&self.id);
            state.sessions_of(self.participant.id)
        };
        self.service.notify_roster(sessions);
    }
}

impl AccessState {
    fn sessions_of(&self, participant: ParticipantId) -> Vec<AgentSessionId> {
        self.grants
            .iter()
            .filter(|(_, grants)| grants.contains_key(&participant))
            .map(|(session_id, _)| *session_id)
            .collect()
    }
}

impl AccessService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Installs the callback that pushes roster changes to session feeds.
    pub(crate) fn set_roster_listener(&self, listener: RosterListener) {
        *self
            .roster_listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(listener);
    }

    fn notify_roster(&self, sessions: Vec<AgentSessionId>) {
        if sessions.is_empty() {
            return;
        }
        let listener = self
            .roster_listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(listener) = listener {
            listener(sessions);
        }
    }

    /// The directory entry for a person, falling back to the id as the name.
    pub(crate) fn identity(&self, participant: ParticipantId) -> ParticipantIdentity {
        ParticipantIdentity {
            id: participant,
            display_name: self
                .state()
                .participants
                .get(&participant)
                .cloned()
                .unwrap_or_else(|| participant.to_string()),
        }
    }

    fn state(&self) -> MutexGuard<'_, AccessState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a person to the directory, or refreshes their display name.
    pub(crate) fn register_participant(&self, identity: &ParticipantIdentity) {
        self.state()
            .participants
            .insert(identity.id, identity.display_name.clone());
    }

    /// Registers a live connection for `identity`.
    pub(crate) fn connect(self: &Arc<Self>, identity: &ParticipantIdentity) -> ConnectionPresence {
        let revoked = Arc::new(Notify::new());
        let (id, sessions) = {
            let mut state = self.state();
            state
                .participants
                .insert(identity.id, identity.display_name.clone());
            state.next_connection += 1;
            let id = state.next_connection;
            state.connections.insert(
                id,
                ConnectionState {
                    participant: identity.id,
                    last_seen: Instant::now(),
                    last_request: None,
                    revoked: Arc::clone(&revoked),
                },
            );
            (id, state.sessions_of(identity.id))
        };
        self.notify_roster(sessions);
        ConnectionPresence {
            id,
            participant: identity.clone(),
            service: Arc::clone(self),
            revoked,
        }
    }

    fn touch(&self, connection: u64, request: bool) {
        let now = Instant::now();
        let mut state = self.state();
        let Some(entry) = state.connections.get_mut(&connection) else {
            return;
        };
        entry.last_seen = now;
        if request {
            entry.last_request = Some(now);
            let participant = entry.participant;
            state.last_active.insert(participant, Timestamp::now());
        }
    }

    pub(crate) fn participants(&self) -> Vec<ParticipantIdentity> {
        let mut participants = self
            .state()
            .participants
            .iter()
            .map(|(id, name)| ParticipantIdentity {
                id: *id,
                display_name: name.clone(),
            })
            .collect::<Vec<_>>();
        participants.sort_by(|left, right| {
            left.display_name
                .cmp(&right.display_name)
                .then(left.id.cmp(&right.id))
        });
        participants
    }

    pub(crate) fn role(
        &self,
        session_id: AgentSessionId,
        participant: ParticipantId,
    ) -> Option<SessionRole> {
        self.state()
            .grants
            .get(&session_id)
            .and_then(|grants| grants.get(&participant))
            .map(|grant| grant.role)
    }

    /// Records `participant` as an owner of a session they just created.
    pub(crate) fn add_owner(&self, session_id: AgentSessionId, participant: ParticipantId) {
        let now = Timestamp::now();
        self.state().grants.entry(session_id).or_default().insert(
            participant,
            SessionGrant {
                session_id,
                participant_id: participant,
                role: SessionRole::Owner,
                granted_by: None,
                granted_at: now,
                updated_at: now,
            },
        );
    }

    /// Makes `participant` the owner of every candidate session that has no
    /// grant at all, and returns the sessions it adopted.
    ///
    /// The caller passes the sessions because this service holds no session
    /// manager: it owns grants, not the catalog those grants hang off. A
    /// session with any grant already has an owner, because the last owner of a
    /// session can neither be demoted nor revoked (see [`Self::set_role`] and
    /// [`Self::revoke`]), so only a session with no grant entry adopts a first
    /// owner here.
    pub(crate) fn adopt_ownerless_sessions(
        &self,
        participant: ParticipantId,
        sessions: impl IntoIterator<Item = AgentSessionId>,
    ) -> Vec<AgentSessionId> {
        let now = Timestamp::now();
        let mut state = self.state();
        let mut adopted = Vec::new();
        for session_id in sessions {
            if state
                .grants
                .get(&session_id)
                .is_some_and(|grants| !grants.is_empty())
            {
                continue;
            }
            state.grants.entry(session_id).or_default().insert(
                participant,
                SessionGrant {
                    session_id,
                    participant_id: participant,
                    role: SessionRole::Owner,
                    granted_by: None,
                    granted_at: now,
                    updated_at: now,
                },
            );
            adopted.push(session_id);
        }
        adopted
    }

    /// Grants a known person access. A new grant starts as an observer; an
    /// existing grant is returned unchanged.
    pub(crate) fn grant(
        &self,
        session_id: AgentSessionId,
        participant: ParticipantId,
        granted_by: Option<ParticipantId>,
    ) -> Result<SessionGrant> {
        let mut state = self.state();
        if !state.participants.contains_key(&participant) {
            return Err(LoomError::not_found("participant", participant));
        }
        let now = Timestamp::now();
        Ok(state
            .grants
            .entry(session_id)
            .or_default()
            .entry(participant)
            .or_insert_with(|| SessionGrant {
                session_id,
                participant_id: participant,
                role: SessionRole::DEFAULT_GRANT,
                granted_by,
                granted_at: now,
                updated_at: now,
            })
            .clone())
    }

    pub(crate) fn set_role(
        &self,
        session_id: AgentSessionId,
        participant: ParticipantId,
        role: SessionRole,
        changed_by: Option<ParticipantId>,
    ) -> Result<SessionGrant> {
        let mut state = self.state();
        let grants = state.grants.entry(session_id).or_default();
        let Some(current) = grants.get(&participant).map(|grant| grant.role) else {
            return Err(LoomError::not_found("session grant", participant));
        };
        if current == SessionRole::Owner && role != SessionRole::Owner {
            ensure_other_owner(grants, participant)?;
        }
        let grant = grants.get_mut(&participant).expect("grant was found above");
        if grant.role != role {
            grant.role = role;
            grant.granted_by = changed_by;
            grant.updated_at = Timestamp::now().max(grant.granted_at);
        }
        Ok(grant.clone())
    }

    /// Removes a grant and signals every connection of that person, so the
    /// transport ends their live subscriptions. Other people are untouched.
    pub(crate) fn revoke(
        &self,
        session_id: AgentSessionId,
        participant: ParticipantId,
    ) -> Result<()> {
        let mut state = self.state();
        let grants = state.grants.entry(session_id).or_default();
        let Some(grant) = grants.get(&participant) else {
            return Err(LoomError::not_found("session grant", participant));
        };
        if grant.role == SessionRole::Owner {
            ensure_other_owner(grants, participant)?;
        }
        grants.remove(&participant);
        for connection in state.connections.values() {
            if connection.participant == participant {
                connection.revoked.notify_one();
            }
        }
        Ok(())
    }

    /// Sessions a person holds any grant in.
    pub(crate) fn sessions_of(&self, participant: ParticipantId) -> BTreeSet<AgentSessionId> {
        self.state()
            .grants
            .iter()
            .filter(|(_, grants)| grants.contains_key(&participant))
            .map(|(session_id, _)| *session_id)
            .collect()
    }

    pub(crate) fn roster(&self, session_id: AgentSessionId) -> Vec<RosterEntry> {
        self.roster_at(session_id, Instant::now())
    }

    /// The roster as seen at `now`: one entry per person with a grant.
    pub(crate) fn roster_at(&self, session_id: AgentSessionId, now: Instant) -> Vec<RosterEntry> {
        let state = self.state();
        let Some(grants) = state.grants.get(&session_id) else {
            return Vec::new();
        };
        let mut entries = grants
            .values()
            .map(|grant| {
                let participant = grant.participant_id;
                let connections = state
                    .connections
                    .values()
                    .filter(|connection| connection.participant == participant)
                    .collect::<Vec<_>>();
                let age = |instant: Instant| now.saturating_duration_since(instant);
                let live = connections
                    .iter()
                    .filter(|connection| age(connection.last_seen) <= PresenceStatus::STALE_AFTER)
                    .collect::<Vec<_>>();
                let liveness_age = connections
                    .iter()
                    .map(|connection| age(connection.last_seen))
                    .min();
                let request_age = live
                    .iter()
                    .filter_map(|connection| connection.last_request.map(age))
                    .min();
                RosterEntry {
                    participant: ParticipantIdentity {
                        id: participant,
                        display_name: state
                            .participants
                            .get(&participant)
                            .cloned()
                            .unwrap_or_else(|| participant.to_string()),
                    },
                    role: grant.role,
                    presence: PresenceStatus::from_activity(liveness_age, request_age),
                    connections: u32::try_from(live.len()).unwrap_or(u32::MAX),
                    last_active_at: state.last_active.get(&participant).copied(),
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .role
                .cmp(&left.role)
                .then_with(|| {
                    left.participant
                        .display_name
                        .cmp(&right.participant.display_name)
                })
                .then_with(|| left.participant.id.cmp(&right.participant.id))
        });
        entries
    }

    /// Files or replaces `requester`'s pending request for `role` in a
    /// session. A person who already holds `role` or a higher one gets an
    /// error; a second request replaces the role and note of the first and
    /// keeps its id.
    pub(crate) fn request_access(
        &self,
        session_id: AgentSessionId,
        requester: &ParticipantIdentity,
        requested_role: SessionRole,
        note: Option<String>,
    ) -> Result<SessionAccessRequest> {
        let note = note
            .map(|note| note.trim().to_owned())
            .filter(|note| !note.is_empty());
        if note
            .as_ref()
            .is_some_and(|note| note.len() > MAX_ACCESS_REQUEST_NOTE_BYTES)
        {
            return Err(LoomError::invalid_request(format!(
                "access request note exceeds {MAX_ACCESS_REQUEST_NOTE_BYTES} bytes"
            )));
        }
        let mut state = self.state();
        if let Some(current) = state
            .grants
            .get(&session_id)
            .and_then(|grants| grants.get(&requester.id))
            .map(|grant| grant.role)
            && current >= requested_role
        {
            return Err(LoomError::invalid_request(format!(
                "you already hold the {} role in session {session_id}",
                current.as_str()
            )));
        }
        state
            .participants
            .entry(requester.id)
            .or_insert_with(|| requester.display_name.clone());
        let existing = state
            .requests
            .values()
            .find(|request| {
                request.session_id == session_id && request.participant.id == requester.id
            })
            .map(|request| (request.id, request.created_at));
        let (id, created_at) =
            existing.unwrap_or_else(|| (AccessRequestId::new(), Timestamp::now()));
        let request = SessionAccessRequest {
            id,
            session_id,
            participant: requester.clone(),
            requested_role,
            note,
            created_at,
        };
        state.requests.insert(id, request.clone());
        Ok(request)
    }

    /// Pending requests of a session, oldest first.
    pub(crate) fn access_requests(&self, session_id: AgentSessionId) -> Vec<SessionAccessRequest> {
        let mut requests = self
            .state()
            .requests
            .values()
            .filter(|request| request.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then(left.id.cmp(&right.id))
        });
        requests
    }

    /// One pending request of a session.
    pub(crate) fn access_request(
        &self,
        session_id: AgentSessionId,
        request_id: AccessRequestId,
    ) -> Result<SessionAccessRequest> {
        self.state()
            .requests
            .get(&request_id)
            .filter(|request| request.session_id == session_id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("access request", request_id))
    }

    /// Removes a pending request once it is decided or withdrawn.
    pub(crate) fn remove_access_request(&self, request_id: AccessRequestId) {
        self.state().requests.remove(&request_id);
    }

    /// Grants `participant` exactly `role`, creating the grant when needed.
    pub(crate) fn grant_role(
        &self,
        session_id: AgentSessionId,
        participant: ParticipantId,
        role: SessionRole,
        granted_by: Option<ParticipantId>,
    ) -> Result<SessionGrant> {
        self.grant(session_id, participant, granted_by)?;
        self.set_role(session_id, participant, role, granted_by)
    }

    /// Durable form of the pending requests.
    pub(crate) fn export_requests(&self) -> Vec<SessionAccessRequest> {
        self.state().requests.values().cloned().collect()
    }

    /// Replaces the pending requests with the durable state.
    pub(crate) fn restore_requests(&self, requests: Vec<SessionAccessRequest>) {
        let mut state = self.state();
        state.requests = requests
            .into_iter()
            .map(|request| (request.id, request))
            .collect();
    }

    /// Durable form: the directory and every grant.
    pub(crate) fn export(&self) -> (BTreeMap<ParticipantId, String>, Vec<SessionGrant>) {
        let state = self.state();
        (
            state.participants.clone(),
            state
                .grants
                .values()
                .flat_map(|grants| grants.values().cloned())
                .collect(),
        )
    }

    /// Replaces the directory and grants with the durable state.
    pub(crate) fn restore(
        &self,
        participants: BTreeMap<ParticipantId, String>,
        grants: Vec<SessionGrant>,
    ) {
        let mut state = self.state();
        for (id, name) in participants {
            state.participants.entry(id).or_insert(name);
        }
        state.grants.clear();
        for grant in grants {
            state
                .grants
                .entry(grant.session_id)
                .or_default()
                .insert(grant.participant_id, grant);
        }
    }
}

fn ensure_other_owner(
    grants: &BTreeMap<ParticipantId, SessionGrant>,
    participant: ParticipantId,
) -> Result<()> {
    let other_owner = grants
        .values()
        .any(|grant| grant.role == SessionRole::Owner && grant.participant_id != participant);
    if other_owner {
        Ok(())
    } else {
        Err(LoomError::new(
            ErrorCode::Conflict,
            "a session must keep at least one owner; promote another owner first",
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn person(name: &str) -> ParticipantIdentity {
        ParticipantIdentity {
            id: ParticipantId::new(),
            display_name: name.to_owned(),
        }
    }

    #[test]
    fn grants_start_as_observer_and_keep_an_owner() {
        let service = AccessService::new();
        let session = AgentSessionId::new();
        let owner = person("Owner");
        let guest = person("Guest");
        service.register_participant(&owner);
        assert_eq!(
            service.grant(session, guest.id, None).unwrap_err().code,
            ErrorCode::NotFound
        );
        service.register_participant(&guest);
        service.add_owner(session, owner.id);
        let grant = service.grant(session, guest.id, Some(owner.id)).unwrap();
        assert_eq!(grant.role, SessionRole::Observer);
        service
            .set_role(session, guest.id, SessionRole::Participant, Some(owner.id))
            .unwrap();
        // Granting again keeps the promoted role.
        assert_eq!(
            service
                .grant(session, guest.id, Some(owner.id))
                .unwrap()
                .role,
            SessionRole::Participant
        );
        assert_eq!(
            service
                .set_role(session, owner.id, SessionRole::Observer, None)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            service.revoke(session, owner.id).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            service
                .set_role(session, ParticipantId::new(), SessionRole::Owner, None)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        service.revoke(session, guest.id).unwrap();
        assert_eq!(service.role(session, guest.id), None);
        assert_eq!(
            service.revoke(session, guest.id).unwrap_err().code,
            ErrorCode::NotFound
        );
        assert_eq!(service.sessions_of(owner.id), BTreeSet::from([session]));
    }

    #[test]
    fn roster_aggregates_connections_and_drops_stale_ones() {
        let service = Arc::new(AccessService::new());
        let session = AgentSessionId::new();
        let owner = person("Owner");
        let guest = person("Guest");
        service.register_participant(&guest);
        service.add_owner(session, owner.id);
        let first = service.connect(&owner);
        let second = service.connect(&owner);
        first.touch_request();
        service.grant(session, guest.id, Some(owner.id)).unwrap();

        let roster = service.roster(session);
        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0].participant, owner);
        assert_eq!(roster[0].connections, 2);
        assert_eq!(roster[0].presence, PresenceStatus::Online);
        assert!(roster[0].last_active_at.is_some());
        assert_eq!(roster[1].presence, PresenceStatus::Offline);
        assert_eq!(roster[1].connections, 0);

        // A reconnect is merged with the person's other connections.
        drop(first);
        let reconnect = service.connect(&owner);
        let roster = service.roster(session);
        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0].connections, 2);
        assert_eq!(roster[0].presence, PresenceStatus::Away);

        // A connection silent for longer than the stale bound counts as gone.
        let later = Instant::now() + PresenceStatus::STALE_AFTER + Duration::from_secs(1);
        let roster = service.roster_at(session, later);
        assert_eq!(roster[0].presence, PresenceStatus::Offline);
        assert_eq!(roster[0].connections, 0);
        drop((second, reconnect));
        assert_eq!(service.roster(session)[0].connections, 0);
        assert!(service.roster(AgentSessionId::new()).is_empty());
    }

    #[test]
    fn revoking_signals_only_the_revoked_persons_connections() {
        let service = Arc::new(AccessService::new());
        let session = AgentSessionId::new();
        let owner = person("Owner");
        let guest = person("Guest");
        service.add_owner(session, owner.id);
        let owner_connection = service.connect(&owner);
        let guest_connection = service.connect(&guest);
        service.grant(session, guest.id, Some(owner.id)).unwrap();
        service.revoke(session, guest.id).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1),
                guest_connection.revoked().notified(),
            )
            .await
            .expect("the revoked person's connection is signalled");
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(20),
                    owner_connection.revoked().notified()
                )
                .await
                .is_err(),
                "other people's connections are not signalled"
            );
        });

        let (participants, grants) = service.export();
        let restored = AccessService::new();
        restored.restore(participants, grants);
        assert_eq!(restored.role(session, owner.id), Some(SessionRole::Owner));
        assert_eq!(restored.role(session, guest.id), None);
        assert_eq!(restored.participants().len(), 2);
    }
}
