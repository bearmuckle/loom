use std::{cmp::Reverse, collections::BTreeMap};

use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, EventSequence, LoomError, Result,
    SessionEvent, SessionEventRecord, Timestamp, WorkspaceId,
};
use serde::{Deserialize, Serialize};

mod project;
mod workspace;

pub use project::{
    AgentMembership, MAX_AGENT_DEPTH, ProjectManager, ProjectManagerState, ProjectRecord,
};
pub use workspace::{WorkspaceManager, WorkspaceManagerState};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionManagerState {
    pub sessions: BTreeMap<AgentSessionId, AgentSessionSnapshot>,
    pub next_sequence: EventSequence,
}

#[derive(Debug, Default)]
pub struct SessionManager {
    sessions: BTreeMap<AgentSessionId, AgentSessionSnapshot>,
    next_sequence: EventSequence,
}

impl SessionManager {
    pub fn from_state(state: SessionManagerState) -> Result<Self> {
        if state
            .sessions
            .iter()
            .any(|(id, snapshot)| *id != snapshot.id)
        {
            return Err(LoomError::new(
                loom_core::ErrorCode::MalformedPayload,
                "persisted session map key does not match its snapshot id",
                false,
            ));
        }

        Ok(Self {
            sessions: state.sessions,
            next_sequence: state.next_sequence,
        })
    }

    pub fn restore(state: SessionManagerState) -> Result<Self> {
        Self::from_state(state)
    }

    pub fn export_state(&self) -> SessionManagerState {
        SessionManagerState {
            sessions: self.sessions.clone(),
            next_sequence: self.next_sequence,
        }
    }

    pub fn state(&self) -> SessionManagerState {
        self.export_state()
    }

    pub fn create_in_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        name: impl Into<String>,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        self.create_in_workspace_with_id(workspace_id, AgentSessionId::new(), name)
    }

    pub fn create_in_workspace_with_id(
        &mut self,
        workspace_id: WorkspaceId,
        session_id: AgentSessionId,
        name: impl Into<String>,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent session name must not be empty",
            ));
        }
        if self.sessions.contains_key(&session_id) {
            return Err(LoomError::conflict(format!(
                "agent session {session_id} already exists"
            )));
        }

        let now = Timestamp::now();
        let snapshot = AgentSessionSnapshot {
            id: session_id,
            workspace_id,
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

    /// Returns the workspace-wide lifecycle sequence high-water mark without
    /// cloning the session catalog.
    pub fn next_sequence(&self) -> EventSequence {
        self.next_sequence
    }

    pub fn list_in_workspace(
        &self,
        workspace_id: Option<WorkspaceId>,
        include_archived: bool,
    ) -> Vec<AgentSessionSnapshot> {
        let mut sessions = self
            .sessions
            .values()
            .filter(|snapshot| {
                workspace_id.is_none_or(|workspace_id| snapshot.workspace_id == workspace_id)
                    && (include_archived || snapshot.state != AgentSessionState::Archived)
            })
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by_key(|snapshot| Reverse(snapshot.updated_at));
        sessions
    }

    pub fn rename(
        &mut self,
        session_id: AgentSessionId,
        name: impl Into<String>,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent session name must not be empty",
            ));
        }
        let snapshot = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| LoomError::not_found("agent session", session_id))?;
        if snapshot.state == AgentSessionState::Archived {
            return Err(LoomError::invalid_state(
                "archived agent sessions cannot be renamed",
            ));
        }
        if snapshot.name == name {
            return Err(LoomError::invalid_request(
                "agent session already has the requested name",
            ));
        }
        snapshot.name = name.clone();
        snapshot.updated_at = Timestamp::now();
        let snapshot = snapshot.clone();
        let record = self.record(
            session_id,
            snapshot.updated_at,
            SessionEvent::AgentSessionRenamed { session_id, name },
        );
        Ok((snapshot, record))
    }

    pub fn archive(
        &mut self,
        session_id: AgentSessionId,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        let snapshot = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| LoomError::not_found("agent session", session_id))?;
        if snapshot.state == AgentSessionState::Archived {
            return Err(LoomError::invalid_state(
                "agent session is already archived",
            ));
        }
        if matches!(
            snapshot.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            return Err(LoomError::invalid_state(
                "running agent sessions must be stopped before archiving",
            ));
        }
        snapshot.state = AgentSessionState::Archived;
        snapshot.updated_at = Timestamp::now();
        let snapshot = snapshot.clone();
        let record = self.record(
            session_id,
            snapshot.updated_at,
            SessionEvent::AgentSessionArchived { session_id },
        );
        Ok((snapshot, record))
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
        if previous == AgentSessionState::Archived {
            return Err(LoomError::invalid_state(
                "archived agent sessions cannot change state",
            ));
        }
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

    pub fn fork(
        &mut self,
        source_session_id: AgentSessionId,
        name: impl Into<String>,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        self.fork_with_id(source_session_id, name, AgentSessionId::new())
    }

    pub fn fork_with_id(
        &mut self,
        source_session_id: AgentSessionId,
        name: impl Into<String>,
        id: AgentSessionId,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        let source = self.get(source_session_id)?;
        let name = name.into();
        if name.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "forked agent session name must not be empty",
            ));
        }
        let now = Timestamp::now();
        let snapshot = AgentSessionSnapshot {
            id,
            workspace_id: source.workspace_id,
            name,
            state: AgentSessionState::Idle,
            created_at: now,
            updated_at: now,
        };
        self.sessions.insert(snapshot.id, snapshot.clone());
        let record = self.record(
            snapshot.id,
            now,
            SessionEvent::AgentSessionForked {
                source_session_id,
                snapshot: snapshot.clone(),
            },
        );
        Ok((snapshot, record))
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
        SessionEventRecord {
            sequence: self.next_sequence,
            session_id,
            occurred_at,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_idle_session_and_journals_creation() {
        let workspace_id = WorkspaceId::new();
        let mut manager = SessionManager::default();

        let (snapshot, event) = manager
            .create_in_workspace(workspace_id, "Foundation demo")
            .unwrap();

        assert_eq!(snapshot.workspace_id, workspace_id);
        assert_eq!(snapshot.state, AgentSessionState::Idle);
        assert_eq!(event.sequence, EventSequence::new(1));
        assert_eq!(manager.session_count(), 1);
    }

    #[test]
    fn rejects_blank_session_names() {
        let mut manager = SessionManager::default();

        let result = manager.create_in_workspace(WorkspaceId::new(), "  ");

        assert_eq!(
            result.unwrap_err().code,
            loom_core::ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn transitions_return_records_for_the_server_journal() {
        let mut manager = SessionManager::default();
        let (snapshot, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Transition demo")
            .unwrap();

        let (updated, event) = manager
            .transition(snapshot.id, AgentSessionState::Planning)
            .unwrap();

        assert_eq!(updated.state, AgentSessionState::Planning);
        assert_eq!(event.sequence, EventSequence::new(2));
        assert_eq!(manager.export_state().next_sequence, event.sequence);
    }

    #[test]
    fn state_round_trips_and_forks_with_an_explicit_event() {
        let mut manager = SessionManager::default();
        let (source, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Source")
            .unwrap();
        let (fork, event) = manager.fork(source.id, "Fork").unwrap();
        let restored = SessionManager::from_state(manager.export_state()).unwrap();

        assert_eq!(restored.get(fork.id).unwrap().name, "Fork");
        assert!(matches!(
            event.event,
            SessionEvent::AgentSessionForked {
                source_session_id,
                ..
            } if source_session_id == source.id
        ));
    }

    #[test]
    fn renames_and_archives_sessions_without_deleting_history() {
        let workspace_id = WorkspaceId::new();
        let mut manager = SessionManager::default();
        let (snapshot, _) = manager
            .create_in_workspace(workspace_id, "Initial")
            .unwrap();

        let (renamed, rename_event) = manager.rename(snapshot.id, "Renamed").unwrap();
        assert_eq!(renamed.name, "Renamed");
        assert!(matches!(
            rename_event.event,
            SessionEvent::AgentSessionRenamed { ref name, .. } if name == "Renamed"
        ));
        let (renamed_again, _) = manager.rename(snapshot.id, "Renamed again").unwrap();
        assert_eq!(renamed_again.name, "Renamed again");

        let (archived, archive_event) = manager.archive(snapshot.id).unwrap();
        assert_eq!(archived.state, AgentSessionState::Archived);
        let restored = SessionManager::from_state(manager.export_state()).unwrap();
        assert_eq!(restored.get(archived.id).unwrap(), archived);
        assert!(
            manager
                .list_in_workspace(Some(workspace_id), false)
                .is_empty()
        );
        assert_eq!(
            manager.list_in_workspace(Some(workspace_id), true),
            vec![archived.clone()]
        );
        assert!(matches!(
            archive_event.event,
            SessionEvent::AgentSessionArchived { session_id } if session_id == archived.id
        ));
        assert_eq!(manager.get(archived.id).unwrap(), archived);
    }
}
