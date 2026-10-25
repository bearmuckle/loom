use std::{cmp::Reverse, collections::BTreeMap};

use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, EventSequence, LoomError, Result,
    SessionEvent, SessionEventRecord, SessionTitleSource, Timestamp, WorkspaceId,
};
mod project;
mod workspace;

pub use loom_core::{SessionManagerState, WorkspaceManagerState};
pub use project::{
    AgentMembership, MAX_AGENT_DEPTH, ProjectManager, ProjectManagerState, ProjectRecord,
};
pub use workspace::WorkspaceManager;

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
            goal: None,
            state: AgentSessionState::Idle,
            created_at: now,
            updated_at: now,
            title_source: SessionTitleSource::Auto,
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

    /// Renames a session and pins its title, so automatic titling stops.
    ///
    /// This is the manual entry point; see [`Self::rename_with_source`] for the
    /// automatic one.
    pub fn rename(
        &mut self,
        session_id: AgentSessionId,
        name: impl Into<String>,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        self.rename_with_source(session_id, name, SessionTitleSource::Manual)
    }

    /// Renames a session and records who chose the name.
    ///
    /// A `Manual` rename pins the title; an `Auto` rename is the backend
    /// choosing a project's title from the work being done.
    pub fn rename_with_source(
        &mut self,
        session_id: AgentSessionId,
        name: impl Into<String>,
        source: SessionTitleSource,
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
        snapshot.title_source = source;
        snapshot.updated_at = Timestamp::now();
        let snapshot = snapshot.clone();
        let record = self.record(
            session_id,
            snapshot.updated_at,
            SessionEvent::AgentSessionRenamed {
                session_id,
                name,
                source,
            },
        );
        Ok((snapshot, record))
    }

    /// Sets the session's goal, trimming and validating it before any change.
    ///
    /// `None` clears the goal. An archived session is immutable, as it is for a
    /// rename. Setting the goal it already has is rejected so a retried request
    /// cannot journal a duplicate change.
    pub fn set_goal(
        &mut self,
        session_id: AgentSessionId,
        goal: Option<String>,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        let goal = goal
            .map(|goal| loom_core::normalize_session_goal(&goal))
            .transpose()?;
        let snapshot = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| LoomError::not_found("agent session", session_id))?;
        if snapshot.state == AgentSessionState::Archived {
            return Err(LoomError::invalid_state(
                "archived agent sessions cannot change their goal",
            ));
        }
        if snapshot.goal == goal {
            return Err(LoomError::invalid_request(
                "agent session already has the requested goal",
            ));
        }
        snapshot.goal = goal.clone();
        snapshot.updated_at = Timestamp::now();
        let snapshot = snapshot.clone();
        let record = self.record(
            session_id,
            snapshot.updated_at,
            SessionEvent::AgentSessionGoalChanged { session_id, goal },
        );
        Ok((snapshot, record))
    }

    /// Records who chose a session's current title without renaming it.
    ///
    /// Pinning a title (`Manual`) makes a project's name the person's, so later
    /// automatic candidates are not applied; unpinning (`Auto`) lets automation
    /// resume.
    pub fn set_title_source(
        &mut self,
        session_id: AgentSessionId,
        source: SessionTitleSource,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        let snapshot = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| LoomError::not_found("agent session", session_id))?;
        if snapshot.state == AgentSessionState::Archived {
            return Err(LoomError::invalid_state(
                "archived agent sessions cannot change their title source",
            ));
        }
        snapshot.title_source = source;
        snapshot.updated_at = Timestamp::now();
        let snapshot = snapshot.clone();
        let record = self.record(
            session_id,
            snapshot.updated_at,
            SessionEvent::AgentSessionTitleSourceChanged { session_id, source },
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

    /// Returns an archived session to `Idle`.
    ///
    /// Only an archived session can be restored, and the state it held before
    /// archiving is not remembered: a restored session starts idle so a new run
    /// can begin. Nothing else about the session changes, so its transcript,
    /// checkpoints, files, and checkouts stay in place.
    pub fn restore_archived(
        &mut self,
        session_id: AgentSessionId,
    ) -> Result<(AgentSessionSnapshot, SessionEventRecord)> {
        let snapshot = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| LoomError::not_found("agent session", session_id))?;
        if snapshot.state != AgentSessionState::Archived {
            return Err(LoomError::invalid_state(
                "only archived agent sessions can be restored",
            ));
        }
        snapshot.state = AgentSessionState::Idle;
        snapshot.updated_at = Timestamp::now();
        let snapshot = snapshot.clone();
        let record = self.record(
            session_id,
            snapshot.updated_at,
            SessionEvent::AgentSessionRestored { session_id },
        );
        Ok((snapshot, record))
    }

    /// Removes a session from the catalog in any state and returns its final
    /// snapshot.
    ///
    /// The server already established that the removal is safe when it calls
    /// this: it cancels the live work of a project root before deleting its tree,
    /// so the catalog does not require an archived session and does not
    /// synthesize an archive transition only to drop the session again. The
    /// server persists the removal separately; no journal event is produced here
    /// because the deletion itself is applied by the durable store.
    pub fn remove(&mut self, session_id: AgentSessionId) -> Result<AgentSessionSnapshot> {
        self.sessions
            .remove(&session_id)
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
            goal: source.goal,
            state: AgentSessionState::Idle,
            created_at: now,
            updated_at: now,
            title_source: SessionTitleSource::Auto,
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

    #[test]
    fn renames_record_who_chose_the_name_and_pin_it() {
        let mut manager = SessionManager::default();
        let (snapshot, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Initial")
            .unwrap();
        assert_eq!(snapshot.title_source, SessionTitleSource::Auto);

        let (automatic, automatic_event) = manager
            .rename_with_source(snapshot.id, "Implement titles", SessionTitleSource::Auto)
            .unwrap();
        assert_eq!(automatic.title_source, SessionTitleSource::Auto);
        assert!(matches!(
            automatic_event.event,
            SessionEvent::AgentSessionRenamed {
                source: SessionTitleSource::Auto,
                ..
            }
        ));

        let (manual, manual_event) = manager.rename(snapshot.id, "Pinned name").unwrap();
        assert_eq!(manual.name, "Pinned name");
        assert_eq!(manual.title_source, SessionTitleSource::Manual);
        assert!(matches!(
            manual_event.event,
            SessionEvent::AgentSessionRenamed {
                source: SessionTitleSource::Manual,
                ..
            }
        ));
    }

    #[test]
    fn title_source_changes_are_journaled_and_refuse_archived_sessions() {
        let mut manager = SessionManager::default();
        let (snapshot, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Title source")
            .unwrap();

        let (pinned, event) = manager
            .set_title_source(snapshot.id, SessionTitleSource::Manual)
            .unwrap();
        assert_eq!(pinned.title_source, SessionTitleSource::Manual);
        assert!(matches!(
            event.event,
            SessionEvent::AgentSessionTitleSourceChanged {
                source: SessionTitleSource::Manual,
                ..
            }
        ));
        assert_eq!(
            manager
                .set_title_source(AgentSessionId::new(), SessionTitleSource::Auto)
                .unwrap_err()
                .code,
            loom_core::ErrorCode::NotFound
        );

        manager.archive(snapshot.id).unwrap();
        assert_eq!(
            manager
                .set_title_source(snapshot.id, SessionTitleSource::Auto)
                .unwrap_err()
                .code,
            loom_core::ErrorCode::InvalidState
        );
    }

    #[test]
    fn sets_clears_and_journals_the_goal_without_allowing_invalid_text() {
        let mut manager = SessionManager::default();
        let (snapshot, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Goal")
            .unwrap();
        assert_eq!(snapshot.goal, None);

        let (updated, event) = manager
            .set_goal(snapshot.id, Some("  Ship the goal feature  ".to_owned()))
            .unwrap();
        assert_eq!(updated.goal.as_deref(), Some("Ship the goal feature"));
        assert!(matches!(
            event.event,
            SessionEvent::AgentSessionGoalChanged { ref goal, .. }
                if goal.as_deref() == Some("Ship the goal feature")
        ));

        let (fork, _) = manager.fork(snapshot.id, "Fork").unwrap();
        assert_eq!(
            fork.goal, updated.goal,
            "a fork copies the goal with the rest of the session state"
        );

        let too_long = "x".repeat(loom_core::MAX_AGENT_SESSION_GOAL_BYTES + 1);
        assert_eq!(
            manager
                .set_goal(snapshot.id, Some("   ".to_owned()))
                .unwrap_err()
                .code,
            loom_core::ErrorCode::InvalidRequest
        );
        assert_eq!(
            manager
                .set_goal(snapshot.id, Some(too_long))
                .unwrap_err()
                .code,
            loom_core::ErrorCode::InvalidRequest
        );
        assert_eq!(
            manager.get(snapshot.id).unwrap().goal,
            updated.goal,
            "invalid goals must not change the stored value"
        );

        let (cleared, clear_event) = manager.set_goal(snapshot.id, None).unwrap();
        assert_eq!(
            cleared.goal, None,
            "clearing leaves the session without a goal"
        );
        assert!(matches!(
            clear_event.event,
            SessionEvent::AgentSessionGoalChanged { goal: None, .. }
        ));
        assert_eq!(
            manager.set_goal(snapshot.id, None).unwrap_err().code,
            loom_core::ErrorCode::InvalidRequest,
            "clearing an already clear goal journals nothing"
        );

        manager.archive(snapshot.id).unwrap();
        assert_eq!(
            manager
                .set_goal(snapshot.id, Some("A later goal".to_owned()))
                .unwrap_err()
                .code,
            loom_core::ErrorCode::InvalidState
        );
        assert_eq!(
            manager.set_goal(snapshot.id, None).unwrap_err().code,
            loom_core::ErrorCode::InvalidState,
            "an archived session cannot clear its goal either"
        );
    }

    #[test]
    fn removal_accepts_a_session_in_any_state() {
        let mut manager = SessionManager::default();
        let (snapshot, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Delete candidate")
            .unwrap();

        let unknown = AgentSessionId::new();
        assert_eq!(
            manager.remove(unknown).unwrap_err().code,
            loom_core::ErrorCode::NotFound
        );
        // Deletion does not require an archive transition first: the server
        // cancels a project root's live work before it removes the session.
        let removed = manager.remove(snapshot.id).unwrap();
        assert_eq!(removed.state, AgentSessionState::Idle);
        assert_eq!(manager.session_count(), 0);
    }

    #[test]
    fn removal_drops_the_session_from_the_catalog() {
        let workspace_id = WorkspaceId::new();
        let mut manager = SessionManager::default();
        let (deleted, _) = manager
            .create_in_workspace(workspace_id, "Deleted")
            .unwrap();
        let (survivor, _) = manager
            .create_in_workspace(workspace_id, "Survivor")
            .unwrap();
        manager.archive(deleted.id).unwrap();
        let sequence_before = manager.next_sequence();

        let removed = manager.remove(deleted.id).unwrap();

        assert_eq!(removed.id, deleted.id);
        assert_eq!(removed.state, AgentSessionState::Archived);
        assert_eq!(
            manager.get(deleted.id).unwrap_err().code,
            loom_core::ErrorCode::NotFound
        );
        assert_eq!(manager.session_count(), 1);
        assert_eq!(
            manager.list_in_workspace(Some(workspace_id), true),
            vec![survivor]
        );
        // Removal is not a journal event; the lifecycle sequence is unchanged.
        assert_eq!(manager.next_sequence(), sequence_before);
    }

    #[test]
    fn removal_round_trips_through_exported_state_without_the_session() {
        let workspace_id = WorkspaceId::new();
        let mut manager = SessionManager::default();
        let (deleted, _) = manager
            .create_in_workspace(workspace_id, "Deleted")
            .unwrap();
        let (survivor, _) = manager
            .create_in_workspace(workspace_id, "Survivor")
            .unwrap();
        manager.archive(deleted.id).unwrap();
        manager.remove(deleted.id).unwrap();

        let restored = SessionManager::from_state(manager.export_state()).unwrap();

        assert_eq!(restored.session_count(), 1);
        assert_eq!(restored.get(survivor.id).unwrap(), survivor);
        assert_eq!(restored.export_state(), manager.export_state());
    }
}
