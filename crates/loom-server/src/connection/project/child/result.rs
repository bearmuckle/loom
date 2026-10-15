use super::*;

/// Delivery state of a delegated child task's own result, as the manager can
/// see it through the durable task projection.
///
/// A child's result is the `result` message it addressed to its manager. That
/// message is durable the moment it is accepted, while delivery happens when a
/// manager run appends project messages to its transcript, so the two states
/// can be told apart after the fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectChildResultState {
    /// The child reported a result that a manager transcript already carries.
    Delivered,
    /// The child reported a result that no manager run has consumed yet.
    Undelivered,
    /// The finished child never reported a result.
    Missing,
    /// The child has not finished, so no result is expected yet.
    Pending,
}

impl ProjectChildResultState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Undelivered => "undelivered",
            Self::Missing => "missing",
            Self::Pending => "pending",
        }
    }

    /// Whether a finished child's work product still has to be read, because it
    /// never reached a manager transcript.
    pub(crate) const fn needs_retrieval(self) -> bool {
        matches!(self, Self::Undelivered | Self::Missing)
    }
}

/// One child task's stored result and the manager's delivery position.
pub(crate) struct ProjectChildResultDelivery {
    /// The latest result message the child reported for its task, when any.
    pub(crate) message: Option<loom_core::AgentMessageRecord>,
    /// Sequence of the last project message a manager transcript consumed.
    pub(crate) delivered_through: u64,
}

impl ProjectChildResultDelivery {
    /// Whether the stored result already sits in a manager transcript.
    pub(crate) fn delivered(&self) -> bool {
        self.message
            .as_ref()
            .is_some_and(|message| message.project_sequence <= self.delivered_through)
    }

    /// Classifies the result against the manager's delivery position and the
    /// child task's terminal state.
    pub(crate) fn state(&self, task_terminal: bool) -> ProjectChildResultState {
        match &self.message {
            Some(_) if self.delivered() => ProjectChildResultState::Delivered,
            Some(_) => ProjectChildResultState::Undelivered,
            None if task_terminal => ProjectChildResultState::Missing,
            None => ProjectChildResultState::Pending,
        }
    }
}

impl InProcessConnection {
    /// Reads a delegated task's stored result and the delivery position of the
    /// manager it reports to.
    ///
    /// Delivery is judged against the manager's durable inbox cursor, which
    /// advances with its transcript checkpoint, so a result that was accepted
    /// but never delivered is reported as undelivered instead of being
    /// indistinguishable from a delivered one.
    pub(crate) fn project_child_result_delivery(
        &self,
        persistence: &dyn Persistence,
        task: &loom_core::DelegatedTaskRecord,
    ) -> Result<ProjectChildResultDelivery> {
        Ok(ProjectChildResultDelivery {
            message: persistence
                .load_project_task_result(task.task_id, task.requester_session_id)?,
            delivered_through: self
                .project_message_delivery_cursor(persistence, task.requester_session_id)?,
        })
    }

    /// Builds the project notification a finished child task owes its manager.
    ///
    /// Returns `None` when the manager already consumed the child's own result.
    /// Otherwise the notification names the task, so a result that was accepted
    /// but never delivered, and a child that finished without reporting at all,
    /// both surface to the manager instead of silence.
    pub(crate) fn project_child_finished_notification(
        &self,
        persistence: &dyn Persistence,
        task: &loom_core::DelegatedTaskRecord,
    ) -> Result<Option<String>> {
        let state = self
            .project_child_result_delivery(persistence, task)?
            .state(true);
        if !state.needs_retrieval() {
            return Ok(None);
        }
        Ok(Some(match state {
            ProjectChildResultState::Undelivered => format!(
                "[Project notification] Direct child task '{}' finished with status {:?} and its stored result has not reached this session yet. Read it with list_project_children before reporting completion.",
                task.child_name, task.status
            ),
            _ => format!(
                "[Project notification] Direct child task '{}' finished with status {:?} without reporting a result. Inspect the task and retrieve or re-request the work.",
                task.child_name, task.status
            ),
        }))
    }

    /// Returns the project-message sequence that `session_id`'s latest run has
    /// appended to its transcript.
    ///
    /// Zero means no run has consumed project messages, which is the case for a
    /// manager that ended its turn before its child reported.
    pub(crate) fn project_message_delivery_cursor(
        &self,
        persistence: &dyn Persistence,
        session_id: AgentSessionId,
    ) -> Result<u64> {
        let Some(summary) = persistence.load_latest_run_summary_for_session(session_id)? else {
            return Ok(0);
        };
        Ok(persistence
            .load_run_execution_state(summary.snapshot.id)?
            .map(|state| state.last_project_message_sequence)
            .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::{ProjectChildResultDelivery, ProjectChildResultState};

    fn result_at(sequence: u64) -> Option<loom_core::AgentMessageRecord> {
        Some(loom_core::AgentMessageRecord {
            message_id: loom_core::AgentMessageId::new(),
            project_id: loom_core::ProjectId::new(),
            task_id: Some(loom_core::TaskId::new()),
            sender_session_id: loom_core::AgentSessionId::new(),
            target_session_id: loom_core::AgentSessionId::new(),
            kind: loom_core::AgentMessageKind::Result,
            project_sequence: sequence,
            accepted_at: loom_core::Timestamp::now(),
            body: "result".to_owned(),
        })
    }

    #[test]
    fn delivery_state_separates_delivered_undelivered_and_missing_results() {
        let delivered = ProjectChildResultDelivery {
            message: result_at(4),
            delivered_through: 4,
        };
        assert_eq!(delivered.state(true), ProjectChildResultState::Delivered);
        assert!(!delivered.state(true).needs_retrieval());

        let undelivered = ProjectChildResultDelivery {
            message: result_at(5),
            delivered_through: 4,
        };
        assert_eq!(
            undelivered.state(true),
            ProjectChildResultState::Undelivered
        );
        assert!(undelivered.state(true).needs_retrieval());

        let missing = ProjectChildResultDelivery {
            message: None,
            delivered_through: 4,
        };
        assert_eq!(missing.state(true), ProjectChildResultState::Missing);
        assert_eq!(missing.state(false), ProjectChildResultState::Pending);
        assert!(missing.state(true).needs_retrieval());
    }
}
