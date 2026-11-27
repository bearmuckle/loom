//! Working through a lost worker connection.
//!
//! A lost connection keeps the workspace on screen. Prompts the user sends
//! while the worker is unreachable are held in a small queue, in order and per
//! session, and sent once the worker answers again. The queue lives only in
//! this client: it is not persisted, so closing the client drops it.

use std::collections::VecDeque;

use loom_core::AgentSessionId;

use super::*;

/// The most prompts the client holds while offline. A queue this size covers
/// a short outage; a longer one should not build up a backlog the agent then
/// receives all at once.
pub(crate) const OFFLINE_QUEUE_CAPACITY: usize = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueuedPrompt {
    session_id: AgentSessionId,
    text: String,
}

/// Prompts written while offline, oldest first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OfflineQueue {
    prompts: VecDeque<QueuedPrompt>,
}

impl OfflineQueue {
    /// Queues a prompt for a session. Returns `false`, keeping nothing, when
    /// the queue is full.
    pub(crate) fn push(&mut self, session_id: AgentSessionId, text: String) -> bool {
        if self.is_full() {
            return false;
        }
        self.prompts.push_back(QueuedPrompt { session_id, text });
        true
    }

    pub(crate) fn is_full(&self) -> bool {
        self.prompts.len() >= OFFLINE_QUEUE_CAPACITY
    }

    /// How many prompts are queued for a session.
    pub(crate) fn count_for(&self, session_id: AgentSessionId) -> usize {
        self.prompts
            .iter()
            .filter(|prompt| prompt.session_id == session_id)
            .count()
    }

    /// Removes and returns the oldest prompt queued for a session.
    pub(crate) fn take_next_for(&mut self, session_id: AgentSessionId) -> Option<String> {
        let index = self
            .prompts
            .iter()
            .position(|prompt| prompt.session_id == session_id)?;
        self.prompts.remove(index).map(|prompt| prompt.text)
    }

    /// Drops every prompt queued for a session and returns how many there were.
    pub(crate) fn discard_for(&mut self, session_id: AgentSessionId) -> usize {
        let before = self.prompts.len();
        self.prompts
            .retain(|prompt| prompt.session_id != session_id);
        before - self.prompts.len()
    }
}

/// The one line the composer shows about the connection and the queue, or
/// `None` when the client is online with nothing queued.
pub(crate) fn offline_strip_text(offline: bool, queued: usize, full: bool) -> Option<String> {
    let prompts = |count: usize| {
        if count == 1 {
            "1 prompt".to_owned()
        } else {
            format!("{count} prompts")
        }
    };
    match (offline, queued) {
        (false, 0) => None,
        (false, count) => Some(format!("Back online · sending {} queued", prompts(count))),
        (true, 0) => Some(
            "Offline · reconnecting. Prompts you send are queued and sent when the worker is back."
                .to_owned(),
        ),
        (true, count) if full => Some(format!(
            "Offline · {} queued, the queue is full. Wait for the worker before sending more.",
            prompts(count)
        )),
        (true, count) => Some(format!(
            "Offline · {} queued, sent when the worker is back",
            prompts(count)
        )),
    }
}

/// Whether the browser client covers the window with its connection card.
///
/// The card is for a client with nothing to show: a first connect, or a
/// startup that is still loading the landing project. Once a workspace is on
/// screen a lost connection keeps it there, with the offline strip and the
/// reconnecting notice saying what is going on.
#[cfg(any(target_family = "wasm", test))]
pub(crate) fn browser_shows_connection_card(
    starting: bool,
    connected: bool,
    has_workspace: bool,
) -> bool {
    starting || (!connected && !has_workspace)
}

impl LoomView {
    /// Whether a worker connection this client had is lost.
    ///
    /// A client that never connected is not offline: it has nothing to send
    /// queued prompts to, and says so through its own startup copy.
    pub(crate) fn is_offline(&self) -> bool {
        self.reconnecting.is_some() || self.browser_connection_lost.is_some()
    }

    /// Holds a prompt for the active session until the worker answers again.
    /// Returns `false` when the queue is full, so the caller keeps the draft.
    pub(crate) fn queue_offline_prompt(&mut self, text: String) -> bool {
        let session_id = self.active_session.id;
        if !self.offline_queue.push(session_id, text) {
            log::warn!("[loom-ui] the offline prompt queue is full; keeping the draft");
            return false;
        }
        log::info!(
            "[loom-ui] worker is offline; queued a prompt for session {session_id} ({} queued)",
            self.offline_queue.count_for(session_id)
        );
        true
    }

    /// The composer's connection line for the active session.
    pub(crate) fn offline_strip(&self) -> Option<String> {
        offline_strip_text(
            self.is_offline(),
            self.offline_queue.count_for(self.active_session.id),
            self.offline_queue.is_full(),
        )
    }

    /// Drops the prompts queued for the active session.
    pub(crate) fn discard_offline_prompts(&mut self, cx: &mut Context<Self>) {
        let dropped = self.offline_queue.discard_for(self.active_session.id);
        log::info!("[loom-ui] discarded {dropped} queued offline prompts");
        cx.notify();
    }

    /// Sends the oldest prompt queued for the active session, once the client
    /// can deliver it.
    ///
    /// One prompt goes at a time: each send's answer comes back through
    /// `finish_send_response`, which calls this again, so a second prompt is a
    /// follow-up to the run the first one started rather than a race with it.
    /// A prompt for another session waits until that session is opened.
    pub(crate) fn flush_offline_queue(&mut self, cx: &mut Context<Self>) {
        if !self.offline_queue_ready() {
            return;
        }
        let Some(text) = self.offline_queue.take_next_for(self.active_session.id) else {
            return;
        };
        log::info!("[loom-ui] the worker is back; sending a queued prompt");
        // A prompt the route refuses (for example a connection that became
        // read-only) is reported by the route and not retried.
        let _ = self.send_composer_text(text, cx);
        cx.notify();
    }

    /// Whether a queued prompt can be sent now without failing for a reason
    /// that would pass on its own.
    fn offline_queue_ready(&self) -> bool {
        if self.is_offline() || self.sending_message || self.browser_starting {
            return false;
        }
        if self.offline_queue.count_for(self.active_session.id) == 0 {
            return false;
        }
        match self.active_run_id {
            // A follow-up needs the run's control state, which arrives with
            // the run's first answer.
            Some(run_id) => self.active_run.as_ref().is_some_and(|run| run.id == run_id),
            // A new run needs the worker's models, which reload after a
            // reconnect.
            None => {
                self.demo_workspace
                    || self.model_catalog_node_id.as_ref()
                        == self.session_node_ids.get(&self.active_session.id)
            }
        }
    }
}
