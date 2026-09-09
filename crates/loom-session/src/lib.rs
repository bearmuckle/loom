use std::collections::BTreeMap;

use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, EventSequence, LoomError, ProjectId,
    Result, SessionEvent, SessionEventRecord, Timestamp,
};

#[derive(Debug, Default)]
pub struct SessionManager {
    sessions: BTreeMap<AgentSessionId, AgentSessionSnapshot>,
    events: Vec<SessionEventRecord>,
    next_sequence: EventSequence,
}

impl SessionManager {
    pub fn create(
        &mut self,
        project_id: ProjectId,
        name: impl Into<String>,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent session name must not be empty",
            ));
        }

        let now = Timestamp::now();
        let snapshot = AgentSessionSnapshot {
            id: AgentSessionId::new(),
            project_id,
            name,
            state: AgentSessionState::Idle,
            created_at: now,
            updated_at: now,
        };
        self.sessions.insert(snapshot.id, snapshot.clone());
        let record = self.record(
            snapshot.id,
            now,
            SessionEvent::AgentSessionCreated {
                snapshot: snapshot.clone(),
            },
        );
        Ok((snapshot, record))
    }

    pub fn get(&self, session_id: AgentSessionId) -> Result<AgentSessionSnapshot> {
        self.sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("agent session", session_id))
    }

    pub fn transition(
        &mut self,
        session_id: AgentSessionId,
        state: AgentSessionState,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        let snapshot = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| LoomError::not_found("agent session", session_id))?;
        let previous = snapshot.state;
        if previous == state {
            return Err(LoomError::invalid_request(
                "agent session is already in the requested state",
            ));
        }

        snapshot.state = state;
        snapshot.updated_at = Timestamp::now();
        let snapshot = snapshot.clone();
        let record = self.record(
            session_id,
            snapshot.updated_at,
            SessionEvent::AgentSessionStateChanged {
                session_id,
                previous,
                current: state,
            },
        );
        Ok((snapshot, record))
    }

    pub fn events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> impl Iterator<Item = &SessionEventRecord> {
        self.events.iter().filter(move |record| {
            session_id.is_none_or(|id| record.session_id == id)
                && after_sequence.is_none_or(|sequence| record.sequence > sequence)
        })
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    fn record(
        &mut self,
        session_id: AgentSessionId,
        occurred_at: Timestamp,
        event: SessionEvent,
    ) -> SessionEventRecord {
        self.next_sequence = self.next_sequence.next();
        let record = SessionEventRecord {
            sequence: self.next_sequence,
            session_id,
            occurred_at,
            event,
        };
        self.events.push(record.clone());
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_idle_session_and_journals_creation() {
        let project_id = ProjectId::new();
        let mut manager = SessionManager::default();

        let (snapshot, event) = manager.create(project_id, "Foundation demo").unwrap();

        assert_eq!(snapshot.project_id, project_id);
        assert_eq!(snapshot.state, AgentSessionState::Idle);
        assert_eq!(event.sequence, EventSequence::new(1));
        assert_eq!(manager.session_count(), 1);
    }

    #[test]
    fn rejects_blank_session_names() {
        let mut manager = SessionManager::default();

        let result = manager.create(ProjectId::new(), "  ");

        assert_eq!(
            result.unwrap_err().code,
            loom_core::ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn transitions_are_journaled() {
        let mut manager = SessionManager::default();
        let (snapshot, _) = manager.create(ProjectId::new(), "Transition demo").unwrap();

        let (updated, event) = manager
            .transition(snapshot.id, AgentSessionState::Planning)
            .unwrap();

        assert_eq!(updated.state, AgentSessionState::Planning);
        assert_eq!(event.sequence, EventSequence::new(2));
        assert_eq!(manager.events_since(Some(snapshot.id), None).count(), 2);
    }
}
