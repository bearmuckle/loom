use super::*;

impl InProcessConnection {
    pub(crate) fn session_events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        let mut events = match &self.backend.persistence {
            Some(persistence) => persistence.load_feed_events_since(session_id, after_sequence)?,
            None => Vec::new(),
        };
        events.extend(
            self.backend
                .journal()?
                .events_since(session_id, after_sequence),
        );
        Ok(deduplicate_events(events))
    }

    pub(crate) fn workspace_events_since(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<WorkspaceFeedEvent>> {
        let session_ids = self
            .backend
            .sessions()?
            .list_in_workspace(Some(workspace_id), true)
            .into_iter()
            .map(|session| session.id)
            .collect::<BTreeSet<_>>();
        let mut events = match &self.backend.persistence {
            Some(persistence) => {
                persistence.load_feed_workspace_events_since(workspace_id, after_sequence)?
            }
            None => Vec::new(),
        };
        events.extend(self.backend.journal()?.workspace_events_since(
            &session_ids,
            workspace_id,
            after_sequence,
        ));
        events.sort_by_key(|event| match event {
            WorkspaceFeedEvent::Session(event) => event.sequence,
            WorkspaceFeedEvent::Workspace(event) => event.sequence,
        });
        events.dedup_by_key(|event| match event {
            WorkspaceFeedEvent::Session(event) => event.sequence,
            WorkspaceFeedEvent::Workspace(event) => event.sequence,
        });
        Ok(events)
    }

    pub(crate) fn session_events_with_safe_cursor(
        &self,
        session_id: AgentSessionId,
        after_sequence: Option<EventSequence>,
    ) -> Result<(Vec<ServerEventEnvelope>, EventSequence)> {
        let cursor_before = self.latest_session_event_sequence(session_id)?;
        let events = self.session_events_since(Some(session_id), after_sequence)?;
        let latest_in_batch = events
            .iter()
            .map(|event| event.sequence)
            .max()
            .unwrap_or_default();
        Ok((events, cursor_before.max(latest_in_batch)))
    }

    pub(crate) fn recent_session_events(
        &self,
        session_id: AgentSessionId,
        limit: usize,
    ) -> Result<Vec<ServerEventEnvelope>> {
        // A deleted session must be reported as missing instead of as a session
        // without events.
        self.backend.sessions()?.get(session_id)?;
        let mut events = match &self.backend.persistence {
            Some(persistence) => persistence.load_recent_feed_events(session_id, limit)?,
            None => Vec::new(),
        };
        events.extend(self.backend.journal()?.recent_events(session_id, limit));
        let mut events = deduplicate_events(events);
        let excess = events.len().saturating_sub(limit);
        if excess > 0 {
            events.drain(..excess);
        }
        Ok(events)
    }

    pub(crate) fn feed_session_cursor(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFeedSessionCursor>> {
        self.backend
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_feed_session_cursor(session_id))
            .transpose()
            .map(Option::flatten)
    }

    pub(crate) fn feed_workspace_cursor(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<DurableFeedWorkspaceCursor>> {
        self.backend
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_feed_workspace_cursor(workspace_id))
            .transpose()
            .map(Option::flatten)
    }

    pub(crate) fn latest_workspace_event_sequence(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<EventSequence> {
        let session_ids = self
            .backend
            .sessions()?
            .list_in_workspace(Some(workspace_id), true)
            .into_iter()
            .map(|session| session.id)
            .collect::<BTreeSet<_>>();
        let durable = self
            .feed_workspace_cursor(workspace_id)?
            .map(|cursor| cursor.latest_sequence)
            .unwrap_or_default();
        let live = self
            .backend
            .journal()?
            .workspace_latest_sequence(&session_ids, workspace_id)
            .unwrap_or_default();
        Ok(durable.max(live))
    }

    pub(crate) fn latest_session_event_sequence(
        &self,
        session_id: AgentSessionId,
    ) -> Result<EventSequence> {
        if let Some(cursor) = self.feed_session_cursor(session_id)? {
            return Ok(cursor.latest_sequence);
        }
        Ok(self
            .backend
            .journal()?
            .latest_sequence(Some(session_id))
            .unwrap_or_default())
    }
}
