//! Attributed collaboration history and roster pushes.

use super::*;

impl InProcessBackend {
    /// Appends an attributed entry to its session's shared history.
    ///
    /// The entry is a session feed event, so it is persisted and retained by
    /// the same policy as the rest of the session history. It is never added
    /// to a run's model messages.
    pub(crate) fn record_collaboration(&self, entry: loom_core::CollaborationEntry) -> Result<()> {
        self.journal()?.append_server_event(
            CURRENT_PROTOCOL_VERSION,
            entry.session_id,
            ServerEvent::Collaboration { entry },
        );
        Ok(())
    }

    /// Pushes the current roster of `session_id` to everyone reading its feed.
    pub(crate) fn publish_roster(&self, session_id: AgentSessionId) {
        let entries = self.access.roster(session_id);
        match self.journal() {
            Ok(mut journal) => {
                journal.append_server_event(
                    CURRENT_PROTOCOL_VERSION,
                    session_id,
                    ServerEvent::SessionRosterChanged { entries },
                );
            }
            Err(error) => {
                log::warn!("[loom-server] could not publish roster of {session_id}: {error}");
            }
        }
    }
}
