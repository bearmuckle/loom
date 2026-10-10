use super::*;

impl EventJournal {
    /// Capture only the pending session rows owned by one run checkpoint. The
    /// workspace feed is committed by full-state saves, never worker saves.
    pub(crate) fn capture_session_feed(
        &self,
        session_id: AgentSessionId,
    ) -> (DurableFeedState, BTreeSet<EventSequence>) {
        let events = self
            .pending_events
            .iter()
            .filter(|event| event.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        let sequences = events.iter().map(|event| event.sequence).collect();
        (
            DurableFeedState {
                next_sequence: self.next_sequence,
                retention_limit: self.retention_limit,
                events,
                workspace_events: Vec::new(),
            },
            sequences,
        )
    }

    /// Acknowledge precisely the rows included in a successfully committed
    /// checkpoint; unrelated sessions and workspace events remain pending.
    pub(crate) fn acknowledge_session_feed(&mut self, sequences: &BTreeSet<EventSequence>) {
        self.pending_events
            .retain(|event| !sequences.contains(&event.sequence));
    }

    pub(crate) fn append_session(&mut self, record: SessionEventRecord) {
        let sequence = self.next();
        let event =
            ServerEventEnvelope::from_session_event(sequence, record.session_id, record.event);
        self.append_event(event);
    }

    pub(crate) fn append_agent(&mut self, session_id: AgentSessionId, event: AgentEvent) {
        let sequence = self.next();
        let event = ServerEventEnvelope::from_agent_event(sequence, session_id, event);
        self.append_event(event);
    }

    pub(crate) fn next(&mut self) -> EventSequence {
        self.next_sequence = self.next_sequence.next();
        if self.retention_limit == 0 {
            self.retention_limit = DEFAULT_EVENT_RETENTION;
        }
        self.next_sequence
    }

    /// Appends a server event with a newly allocated global sequence.
    ///
    /// The sequence is allocated and the envelope pushed while the caller holds
    /// the journal lock, so concurrent appenders cannot interleave a lower
    /// sequence after a higher one. Splitting allocation (`next`) from insertion
    /// (`append_event`) across two lock acquisitions would allow that, and the
    /// persistence layer rejects a non-monotonic pending feed.
    pub(crate) fn append_server_event(
        &mut self,
        protocol_version: loom_core::ProtocolVersion,
        session_id: AgentSessionId,
        event: loom_protocol::ServerEvent,
    ) -> EventSequence {
        let sequence = self.next();
        self.append_event(ServerEventEnvelope {
            protocol_version,
            sequence,
            session_id,
            event,
        });
        sequence
    }

    fn append_event(&mut self, event: ServerEventEnvelope) {
        self.events.push(event.clone());
        self.pending_events.push(event);
        Self::prune_events(&mut self.events, self.retention_limit);
        Self::prune_events(&mut self.pending_events, self.retention_limit);
    }

    pub(crate) fn append_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        event: WorkspaceEvent,
    ) -> EventSequence {
        let envelope = WorkspaceEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence: self.next(),
            workspace_id,
            event,
        };
        let sequence = envelope.sequence;
        self.workspace_events.push(envelope.clone());
        self.pending_workspace_events.push(envelope);
        Self::prune_workspace_events(&mut self.workspace_events, self.retention_limit);
        Self::prune_workspace_events(&mut self.pending_workspace_events, self.retention_limit);
        sequence
    }

    pub(crate) fn discard_pending_workspace(&mut self, sequence: EventSequence) {
        self.pending_workspace_events
            .retain(|event| event.sequence != sequence);
        self.workspace_events
            .retain(|event| event.sequence != sequence);
    }

    pub(crate) fn prune_workspace_events(events: &mut Vec<WorkspaceEventEnvelope>, limit: usize) {
        let mut counts = BTreeMap::<WorkspaceId, usize>::new();
        for event in events.iter() {
            *counts.entry(event.workspace_id).or_default() += 1;
        }
        for (workspace_id, count) in counts {
            let mut excess = count.saturating_sub(limit);
            if excess > 0 {
                events.retain(|event| {
                    if excess > 0 && event.workspace_id == workspace_id {
                        excess -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    pub(crate) fn prune_events(events: &mut Vec<ServerEventEnvelope>, limit: usize) {
        let mut counts = BTreeMap::<AgentSessionId, usize>::new();
        for event in events.iter() {
            *counts.entry(event.session_id).or_default() += 1;
        }
        for (session_id, count) in counts {
            let mut excess = count.saturating_sub(limit);
            if excess > 0 {
                events.retain(|event| {
                    if excess > 0 && event.session_id == session_id {
                        excess -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    pub(crate) fn oldest_sequence(
        &self,
        session_id: Option<AgentSessionId>,
    ) -> Option<EventSequence> {
        self.oldest_event(session_id).map(|event| event.sequence)
    }

    pub(crate) fn oldest_event(
        &self,
        session_id: Option<AgentSessionId>,
    ) -> Option<&ServerEventEnvelope> {
        self.events
            .iter()
            .find(|event| session_id.is_none_or(|id| event.session_id == id))
    }

    pub(crate) fn latest_sequence(
        &self,
        session_id: Option<AgentSessionId>,
    ) -> Option<EventSequence> {
        self.events
            .iter()
            .rev()
            .find(|event| session_id.is_none_or(|id| event.session_id == id))
            .map(|event| event.sequence)
    }

    pub(crate) fn is_cursor_stale(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> bool {
        let Some(after_sequence) = after_sequence else {
            return false;
        };
        let Some(oldest) = self.oldest_event(session_id) else {
            return false;
        };
        if after_sequence.next() >= oldest.sequence {
            return false;
        }
        let Some(session_id) = session_id else {
            return true;
        };
        !matches!(
            &oldest.event,
            loom_protocol::ServerEvent::AgentSessionCreated { snapshot }
                | loom_protocol::ServerEvent::AgentSessionForked { snapshot, .. }
                if snapshot.id == session_id
        )
    }

    pub(crate) fn set_retention(&mut self, limit: usize) {
        self.retention_limit = limit;
        Self::prune_events(&mut self.events, limit);
        Self::prune_events(&mut self.pending_events, limit);
        Self::prune_workspace_events(&mut self.workspace_events, limit);
        Self::prune_workspace_events(&mut self.pending_workspace_events, limit);
    }

    pub(crate) fn events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Vec<ServerEventEnvelope> {
        self.events
            .iter()
            .filter(|event| {
                session_id.is_none_or(|id| event.session_id == id)
                    && after_sequence.is_none_or(|sequence| event.sequence > sequence)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn workspace_events_since(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Vec<WorkspaceFeedEvent> {
        let mut events = self
            .events
            .iter()
            .filter(|event| {
                session_ids.contains(&event.session_id)
                    && after_sequence.is_none_or(|sequence| event.sequence > sequence)
            })
            .cloned()
            .map(WorkspaceFeedEvent::Session)
            .collect::<Vec<_>>();
        events.extend(
            self.workspace_events
                .iter()
                .filter(|event| {
                    event.workspace_id == workspace_id
                        && after_sequence.is_none_or(|sequence| event.sequence > sequence)
                })
                .cloned()
                .map(WorkspaceFeedEvent::Workspace),
        );
        events.sort_by_key(|event| match event {
            WorkspaceFeedEvent::Session(event) => event.sequence,
            WorkspaceFeedEvent::Workspace(event) => event.sequence,
        });
        events
    }

    pub(crate) fn workspace_oldest_sequence(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        workspace_id: WorkspaceId,
    ) -> Option<EventSequence> {
        self.events
            .iter()
            .find(|event| session_ids.contains(&event.session_id))
            .map(|event| event.sequence)
            .into_iter()
            .chain(
                self.workspace_events
                    .iter()
                    .find(|event| event.workspace_id == workspace_id)
                    .map(|event| event.sequence),
            )
            .min()
    }

    pub(crate) fn workspace_latest_sequence(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        workspace_id: WorkspaceId,
    ) -> Option<EventSequence> {
        self.events
            .iter()
            .rev()
            .find(|event| session_ids.contains(&event.session_id))
            .map(|event| event.sequence)
            .into_iter()
            .chain(
                self.workspace_events
                    .iter()
                    .rev()
                    .find(|event| event.workspace_id == workspace_id)
                    .map(|event| event.sequence),
            )
            .max()
    }

    pub(crate) fn workspace_cursor_is_stale(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> bool {
        let (Some(after), Some(oldest)) = (
            after_sequence,
            self.workspace_oldest_sequence(session_ids, workspace_id),
        ) else {
            return false;
        };
        after.next() < oldest
    }

    pub(crate) fn recent_events(
        &self,
        session_id: AgentSessionId,
        limit: usize,
    ) -> Vec<ServerEventEnvelope> {
        self.events
            .iter()
            .filter(|event| event.session_id == session_id)
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}
