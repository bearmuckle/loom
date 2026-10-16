use super::*;

/// Bytes of one response reserved for everything that is not an event.
///
/// A batch travels inside the response envelope, which carries the protocol
/// version, the request id, the response tag, the stream epoch, and the
/// continuation cursor. The reservation is an upper bound on that overhead, so
/// the events it leaves room for always fit the configured frame limit.
const EVENT_BATCH_ENVELOPE_BYTES: usize = 512;

/// One bounded page of journaled session events.
#[derive(Debug)]
pub(crate) struct BoundedEventBatch {
    /// The events this page carries, in sequence order.
    pub(crate) events: Vec<ServerEventEnvelope>,
    /// The sequence to continue from when [`Self::events`] does not reach the
    /// latest journaled event; `None` when the page did.
    pub(crate) next_after_sequence: Option<EventSequence>,
}

/// Bounds one batch of journaled events to a single response frame.
///
/// The events response is the one response that grows with the journal, so it is
/// delivered as a sequence of pages a caller follows: each page stops before the
/// event count (`MAX_EVENT_BATCH_EVENTS`) or the configured response byte budget
/// would be crossed and names the sequence to continue from.
///
/// An event that alone exceeds the byte budget cannot be delivered in any frame
/// under the configured limit. When such an event is the first of the batch it is
/// reported as [`ErrorCode::PayloadTooLarge`] naming its sequence and size,
/// instead of the batch being sent and the caller's own frame rule dropping the
/// event without telling anyone which one was lost. Later in a batch it ends the
/// page before it, so the caller reaches it as the first event of the next page
/// and is told about it there.
///
/// `max_response_bytes` is the frame limit this connection's responses must fit
/// (`None` for an in-process caller, which has no frame): the batch is then
/// bounded by event count alone.
pub(crate) fn bound_event_batch(
    events: Vec<ServerEventEnvelope>,
    max_response_bytes: Option<usize>,
) -> Result<BoundedEventBatch> {
    // The events themselves must fit what the response envelope leaves over.
    let content_budget =
        max_response_bytes.map(|limit| limit.saturating_sub(EVENT_BATCH_ENVELOPE_BYTES));
    let mut page = Vec::new();
    let mut total = 0usize;
    let mut next_after_sequence = None;
    for event in events {
        let size = encoded_event_bytes(&event)?;
        if let Some(budget) = content_budget
            && size > budget
        {
            // The event cannot fit any frame under the configured limit, so the
            // page never includes it. As the first event it is reported by name
            // instead of being sent and dropped; later it ends this page and is
            // reported as the first event of the next one.
            if page.is_empty() {
                return Err(event_too_large_error(
                    event.sequence,
                    size,
                    // A budget only exists with a limit, so the diagnostic always
                    // has one to name.
                    max_response_bytes.unwrap_or_default(),
                ));
            }
        } else if page.len() < MAX_EVENT_BATCH_EVENTS
            && content_budget.is_none_or(|budget| total + size <= budget)
        {
            total += size;
            page.push(event);
            continue;
        }
        next_after_sequence = last_sequence(&page);
        break;
    }
    Ok(BoundedEventBatch {
        events: page,
        next_after_sequence,
    })
}

/// The sequence a follow-up page continues from: the last event of the page.
///
/// Every page either carries an event or reports the unfit one it could not fit,
/// so the caller's cursor moves forward on every answer and cannot loop on the
/// same batch.
fn last_sequence(events: &[ServerEventEnvelope]) -> Option<EventSequence> {
    events.last().map(|event| event.sequence)
}

/// The encoded size of one event envelope, in bytes.
fn encoded_event_bytes(event: &ServerEventEnvelope) -> Result<usize> {
    loom_protocol::encode_event(event)
        .map(|bytes| bytes.len())
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!(
                    "could not measure session event {} for the events batch bound: {error}",
                    event.sequence
                ),
                false,
            )
        })
}

/// The diagnostic for an event that cannot fit an events response at all.
///
/// It names the event's sequence and size, because the caller otherwise only
/// sees a sequence gap: the frame would have been sent and dropped by the
/// caller's own frame rule, with nothing saying which event went missing.
fn event_too_large_error(sequence: EventSequence, size: usize, limit: usize) -> LoomError {
    LoomError::payload_too_large(format!(
        "session event {sequence} of {size} bytes cannot be delivered: it is above the configured \
         {limit}-byte response frame limit, so the events batch stops before it; raise the frame \
         size limit to fetch it"
    ))
}

impl InProcessConnection {
    /// Bounds one batch of journaled events to a response this connection may
    /// send; see [`bound_event_batch`].
    pub(crate) fn bound_event_batch(
        &self,
        events: Vec<ServerEventEnvelope>,
    ) -> Result<BoundedEventBatch> {
        bound_event_batch(events, self.max_response_bytes)
    }

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
