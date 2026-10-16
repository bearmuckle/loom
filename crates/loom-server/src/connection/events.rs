use super::*;

/// Bytes of one response reserved for everything that is not an event.
///
/// A batch travels inside the response envelope, which carries the protocol
/// version, the request id, the response tag, the stream epoch, and the
/// continuation cursor. The reservation is an upper bound on that overhead, so
/// the events it leaves room for always fit the configured frame limit.
const EVENT_BATCH_ENVELOPE_BYTES: usize = 512;

/// One item of a journaled batch: where it sits in the journal, and what it
/// costs in a response.
///
/// Two response families are bounded the same way — the session events batch and
/// the workspace feed — so the bound itself is written once against this trait.
trait FeedItem {
    /// What one item is called in a size diagnostic.
    const NOUN: &'static str;

    fn sequence(&self) -> EventSequence;

    fn encoded_bytes(&self) -> Result<usize>;
}

impl FeedItem for ServerEventEnvelope {
    const NOUN: &'static str = "session event";

    fn sequence(&self) -> EventSequence {
        self.sequence
    }

    fn encoded_bytes(&self) -> Result<usize> {
        loom_protocol::encode_event(self)
            .map(|bytes| bytes.len())
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!(
                        "could not measure session event {} for the events batch bound: {error}",
                        self.sequence
                    ),
                    false,
                )
            })
    }
}

impl FeedItem for loom_protocol::WorkspaceFeedEvent {
    const NOUN: &'static str = "workspace feed event";

    fn sequence(&self) -> EventSequence {
        match self {
            Self::Session(event) => event.sequence,
            Self::Workspace(event) => event.sequence,
        }
    }

    fn encoded_bytes(&self) -> Result<usize> {
        serde_json::to_vec(self)
            .map(|bytes| bytes.len())
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!(
                        "could not measure workspace feed event {} for the events batch bound: {error}",
                        self.sequence()
                    ),
                    false,
                )
            })
    }
}

/// Which side of an oversized batch a response keeps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BatchEdge {
    /// Keep the oldest items and name the cursor to continue from, so a caller
    /// follows the pages forward. This is the paged batch, which can afford
    /// nothing else.
    Oldest,
    /// Keep the newest items. A response that cannot page (a snapshot) has to
    /// carry the state the caller is resuming into, and the older part is
    /// recovered from durable state by the rebuild the snapshot triggers.
    Newest,
}

/// One bounded page of journaled events.
#[derive(Debug)]
pub(crate) struct BoundedBatch<T> {
    /// The events this page carries, in sequence order.
    pub(crate) events: Vec<T>,
    /// The sequence to continue from when [`Self::events`] does not reach the
    /// latest journaled event; `None` when the page did — and always `None` for
    /// [`BatchEdge::Newest`], which cannot drop the newest side.
    pub(crate) next_after_sequence: Option<EventSequence>,
    /// The first sequence the page carries, which for [`BatchEdge::Newest`] is
    /// the oldest sequence this response reached; `None` for an empty page.
    pub(crate) oldest_sequence: Option<EventSequence>,
}

/// Bounds one batch of journaled events to a single response frame.
///
/// Both response families that grow with the journal are bounded here: the events
/// batch, which is delivered as a sequence of pages a caller follows, and a
/// snapshot, which cannot page and therefore keeps its newest side instead. Each
/// page stops before the event count (`MAX_EVENT_BATCH_EVENTS`) or the configured
/// response byte budget would be crossed.
///
/// An event that alone exceeds the byte budget cannot be delivered in any frame
/// under the configured limit. When such an event is the one the response has to
/// carry — the first event of an oldest-first page, or the newest event of a
/// snapshot — it is reported as [`ErrorCode::PayloadTooLarge`] naming its
/// sequence and size, instead of the response being sent and the reader's own
/// frame rule dropping the event without telling anyone which one was lost.
/// Elsewhere it simply ends the page, and the caller reaches it on the next one
/// or recovers it durably.
///
/// `max_response_bytes` is the frame limit this connection's responses must fit
/// (`None` for an in-process caller, which has no frame): the batch is then
/// bounded by event count alone.
fn bound_batch<T: FeedItem>(
    items: Vec<T>,
    max_response_bytes: Option<usize>,
    edge: BatchEdge,
) -> Result<BoundedBatch<T>> {
    // The events themselves must fit what the response envelope leaves over.
    let content_budget =
        max_response_bytes.map(|limit| limit.saturating_sub(EVENT_BATCH_ENVELOPE_BYTES));
    // A budget only exists with a limit, so a diagnostic always has one to name.
    let configured_limit = max_response_bytes.unwrap_or_default();
    let fits = |size: usize, total: usize| {
        content_budget.is_none_or(|budget| total.saturating_add(size) <= budget)
    };
    let mut page: Vec<T> = Vec::new();
    let mut total = 0usize;
    let mut next_after_sequence = None;
    match edge {
        BatchEdge::Oldest => {
            for item in items {
                let size = item.encoded_bytes()?;
                let oversized = content_budget.is_some_and(|budget| size > budget);
                if oversized && page.is_empty() {
                    return Err(event_too_large_error(
                        T::NOUN,
                        item.sequence(),
                        size,
                        configured_limit,
                    ));
                }
                if oversized || page.len() >= MAX_EVENT_BATCH_EVENTS || !fits(size, total) {
                    // This event opens the next page, and the caller is told
                    // where to continue.
                    next_after_sequence = page.last().map(FeedItem::sequence);
                    break;
                }
                total += size;
                page.push(item);
            }
        }
        BatchEdge::Newest => {
            // Walked from the newest event backwards, because a snapshot has to
            // carry the state the caller resumes into: dropping the newest side
            // would hand it a window that cannot be completed, while the oldest
            // side is recovered from durable state by the rebuild the snapshot
            // triggers.
            let mut newest_first: Vec<T> = Vec::new();
            for item in items.into_iter().rev() {
                let size = item.encoded_bytes()?;
                let oversized = content_budget.is_some_and(|budget| size > budget);
                if oversized && newest_first.is_empty() {
                    // The newest event cannot fit any frame this connection may
                    // send, so no snapshot can carry it: name it instead of
                    // sending a response the reader discards.
                    return Err(event_too_large_error(
                        T::NOUN,
                        item.sequence(),
                        size,
                        configured_limit,
                    ));
                }
                if oversized || newest_first.len() >= MAX_EVENT_BATCH_EVENTS || !fits(size, total) {
                    // Older than the window this response can carry; the
                    // caller's durable rebuild covers it.
                    break;
                }
                total += size;
                newest_first.push(item);
            }
            newest_first.reverse();
            page = newest_first;
        }
    }
    let oldest_sequence = page.first().map(FeedItem::sequence);
    Ok(BoundedBatch {
        events: page,
        next_after_sequence,
        oldest_sequence,
    })
}

/// Bounds a batch that a caller follows page by page: the oldest events are
/// kept and the continuation cursor names the next page.
pub(crate) fn bound_event_batch(
    events: Vec<ServerEventEnvelope>,
    max_response_bytes: Option<usize>,
) -> Result<BoundedBatch<ServerEventEnvelope>> {
    bound_batch(events, max_response_bytes, BatchEdge::Oldest)
}

/// The diagnostic for an event that cannot fit an events response at all.
///
/// It names the event's sequence and size, because the caller otherwise only
/// sees a sequence gap: the frame would have been sent and dropped by the
/// caller's own frame rule, with nothing saying which event went missing. `noun`
/// says which response family the event belongs to, so a workspace feed event is
/// not reported as a session event.
fn event_too_large_error(
    noun: &str,
    sequence: EventSequence,
    size: usize,
    limit: usize,
) -> LoomError {
    LoomError::payload_too_large(format!(
        "{noun} {sequence} of {size} bytes cannot be delivered: it is above the configured \
         {limit}-byte response frame limit, so the events batch stops before it; raise the frame \
         size limit to fetch it"
    ))
}

impl InProcessConnection {
    /// Bounds one batch of journaled events to a response this connection may
    /// send; see [`bound_batch`].
    pub(crate) fn bound_event_batch(
        &self,
        events: Vec<ServerEventEnvelope>,
    ) -> Result<BoundedBatch<ServerEventEnvelope>> {
        bound_event_batch(events, self.max_response_bytes)
    }

    /// Bounds a session snapshot's window, keeping its newest events.
    ///
    /// A snapshot cannot page, so the state the caller resumes into is the part
    /// that has to fit; the caller recovers everything older from the durable
    /// projection the snapshot is paired with.
    pub(crate) fn bound_event_snapshot(
        &self,
        events: Vec<ServerEventEnvelope>,
    ) -> Result<BoundedBatch<ServerEventEnvelope>> {
        bound_batch(events, self.max_response_bytes, BatchEdge::Newest)
    }

    /// Bounds one page of the workspace feed, which a caller follows forward.
    pub(crate) fn bound_workspace_events(
        &self,
        events: Vec<loom_protocol::WorkspaceFeedEvent>,
    ) -> Result<BoundedBatch<loom_protocol::WorkspaceFeedEvent>> {
        bound_batch(events, self.max_response_bytes, BatchEdge::Oldest)
    }

    /// Bounds a workspace snapshot's feed window, keeping its newest events.
    pub(crate) fn bound_workspace_snapshot(
        &self,
        events: Vec<loom_protocol::WorkspaceFeedEvent>,
    ) -> Result<BoundedBatch<loom_protocol::WorkspaceFeedEvent>> {
        bound_batch(events, self.max_response_bytes, BatchEdge::Newest)
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
