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
        let envelope = ServerEventEnvelope {
            protocol_version,
            sequence,
            session_id,
            event,
        };
        if matches!(
            envelope.event,
            loom_protocol::ServerEvent::SessionRosterChanged { .. }
        ) {
            // Presence churn is live state, not history: only the latest
            // roster of a session is kept, outside the retained window, and it
            // is never written to the durable feed. It therefore never evicts
            // a history event; `GetSessionRoster` reads the same state.
            self.rosters.insert(session_id, envelope);
            return sequence;
        }
        self.append_event(envelope);
        sequence
    }

    /// The latest roster event of a session, if it is newer than `after`.
    fn roster_since(
        &self,
        session_id: AgentSessionId,
        after_sequence: Option<EventSequence>,
    ) -> Option<&ServerEventEnvelope> {
        self.rosters
            .get(&session_id)
            .filter(|event| Self::is_after(event.sequence, after_sequence))
    }

    fn append_event(&mut self, event: ServerEventEnvelope) {
        self.pending_events.push(event.clone());
        self.events
            .entry(event.session_id)
            .or_default()
            .push_back(event);
        self.prune_events();
        Self::prune_pending_events(&mut self.pending_events, self.retention_limit);
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

    /// Drops retained events beyond the per-session retention limit.
    ///
    /// Every session owns a separate window, so pruning one session never
    /// evicts another session's events, and the work is proportional to the
    /// number of sessions instead of to the total retained history.
    fn prune_events(&mut self) {
        let limit = self.retention_limit;
        self.events.retain(|_, events| {
            while events.len() > limit {
                events.pop_front();
            }
            !events.is_empty()
        });
    }

    /// Retention pruning for the globally ordered pending feed. That vector is
    /// the unpersisted durable feed prefix, so it stays sequence ordered and is
    /// only ever a few flushes long.
    fn prune_pending_events(events: &mut Vec<ServerEventEnvelope>, limit: usize) {
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

    /// Whether `sequence` is newer than the optional resume cursor.
    fn is_after(sequence: EventSequence, after_sequence: Option<EventSequence>) -> bool {
        after_sequence.is_none_or(|after| sequence > after)
    }

    /// Retained events of one session in ascending sequence order.
    fn session_events(
        &self,
        session_id: AgentSessionId,
    ) -> impl Iterator<Item = &ServerEventEnvelope> {
        self.events.get(&session_id).into_iter().flatten()
    }

    /// Oldest retained event of one session, if any.
    fn session_oldest(&self, session_id: AgentSessionId) -> Option<&ServerEventEnvelope> {
        self.events
            .get(&session_id)
            .and_then(|events| events.front())
    }

    /// Latest retained event of one session, if any.
    fn session_latest(&self, session_id: AgentSessionId) -> Option<&ServerEventEnvelope> {
        self.events
            .get(&session_id)
            .and_then(|events| events.back())
    }

    /// Whether every retained window is sequence ordered and stays inside the
    /// global cursor. Restored journals carry no retained window because the
    /// durable feed owns the events, so this only rejects a malformed journal.
    pub(crate) fn sequences_are_valid(&self) -> bool {
        self.events.values().all(|events| {
            events
                .iter()
                .zip(events.range(1..))
                .all(|(previous, next)| previous.sequence < next.sequence)
                && events
                    .back()
                    .is_none_or(|event| event.sequence <= self.next_sequence)
        })
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
        match session_id {
            Some(session_id) => self.session_oldest(session_id),
            // The un-scoped feed is ordered by global sequence, so a global
            // read merges the per-session windows instead of one flat vector.
            None => self
                .events
                .values()
                .filter_map(|events| events.front())
                .min_by_key(|event| event.sequence),
        }
    }

    pub(crate) fn latest_sequence(
        &self,
        session_id: Option<AgentSessionId>,
    ) -> Option<EventSequence> {
        match session_id {
            Some(session_id) => self.session_latest(session_id).map(|event| event.sequence),
            None => self
                .events
                .values()
                .filter_map(|events| events.back())
                .map(|event| event.sequence)
                .max(),
        }
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
        self.prune_events();
        Self::prune_pending_events(&mut self.pending_events, limit);
        Self::prune_workspace_events(&mut self.workspace_events, limit);
        Self::prune_workspace_events(&mut self.pending_workspace_events, limit);
    }

    pub(crate) fn events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Vec<ServerEventEnvelope> {
        match session_id {
            Some(session_id) => {
                let mut events = self
                    .session_events(session_id)
                    .filter(|event| Self::is_after(event.sequence, after_sequence))
                    .cloned()
                    .collect::<Vec<_>>();
                if let Some(roster) = self.roster_since(session_id, after_sequence) {
                    events.push(roster.clone());
                    events.sort_by_key(|event| event.sequence);
                }
                events
            }
            None => {
                let mut events = self
                    .events
                    .values()
                    .flatten()
                    .chain(self.rosters.values())
                    .filter(|event| Self::is_after(event.sequence, after_sequence))
                    .cloned()
                    .collect::<Vec<_>>();
                events.sort_by_key(|event| event.sequence);
                events
            }
        }
    }

    pub(crate) fn workspace_events_since(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Vec<WorkspaceFeedEvent> {
        let mut events = session_ids
            .iter()
            .flat_map(|session_id| {
                self.session_events(*session_id)
                    .chain(self.rosters.get(session_id))
            })
            .filter(|event| Self::is_after(event.sequence, after_sequence))
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
        session_ids
            .iter()
            .filter_map(|session_id| self.session_oldest(*session_id))
            .map(|event| event.sequence)
            .min()
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
        session_ids
            .iter()
            .filter_map(|session_id| self.session_latest(*session_id))
            .map(|event| event.sequence)
            .max()
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
        let Some(events) = self.events.get(&session_id) else {
            return Vec::new();
        };
        events
            .range(events.len().saturating_sub(limit)..)
            .cloned()
            .collect()
    }
}
