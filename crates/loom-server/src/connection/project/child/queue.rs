use super::*;

/// One workspace's delegated-task admission queue, as a manager reads it.
///
/// The queue is workspace-wide because the concurrency limit is a workspace
/// setting: every queued delegated task in the workspace competes for the same
/// slots, in the order the scheduler admits them — oldest first, ties broken by
/// task ID. Nothing extra is stored for this projection: a task joins the queue
/// when its status becomes `queued` and leaves it when any other status is
/// recorded, so a position always reflects the tasks that are still waiting.
pub(crate) struct ProjectChildAdmissionQueue {
    /// Queued task IDs in admission order, front of the queue first.
    ordered: Vec<loom_core::TaskId>,
    /// The workspace's effective project agent concurrency limit.
    pub(crate) concurrency_limit: u8,
}

impl ProjectChildAdmissionQueue {
    /// The 1-based place of a queued task in the waiting order.
    ///
    /// `None` means the task is not queued in this workspace, so it is not
    /// waiting for a slot and has no position to report. A task ahead of it that
    /// finishes or is cancelled leaves the queue, which lowers the position of
    /// everything behind it.
    pub(crate) fn position(&self, task_id: loom_core::TaskId) -> Option<u32> {
        self.ordered
            .iter()
            .position(|queued| *queued == task_id)
            .and_then(|index| u32::try_from(index + 1).ok())
    }
}

impl InProcessConnection {
    /// Reads the admission queue of one workspace.
    pub(crate) fn project_child_admission_queue(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<ProjectChildAdmissionQueue> {
        let mut queued = self
            .workspace_project_tasks(workspace_id)?
            .into_iter()
            .filter(|task| task.status == loom_core::DelegatedTaskStatus::Queued)
            .map(|task| (task.created_at, task.task_id.to_string(), task.task_id))
            .collect::<Vec<_>>();
        // The scheduler admits candidates in this same order, so a reported
        // position is the order the children actually start in.
        queued.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        Ok(ProjectChildAdmissionQueue {
            ordered: queued.into_iter().map(|(_, _, task_id)| task_id).collect(),
            concurrency_limit: self.workspace_project_agent_concurrency(workspace_id)?,
        })
    }

    /// The effective number of delegated project agents one workspace runs at
    /// once, read from its configuration with the default applied.
    pub(crate) fn workspace_project_agent_concurrency(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<u8> {
        Ok(self
            .backend
            .workspace_configs()?
            .get(&workspace_id)
            .map(|config| config.project_agent_concurrency)
            .unwrap_or_else(|| WorkspaceConfig::default().project_agent_concurrency))
    }
}

#[cfg(test)]
mod tests {
    use super::ProjectChildAdmissionQueue;
    use loom_core::TaskId;

    #[test]
    fn a_position_counts_the_queued_tasks_ahead_of_it() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let queue = ProjectChildAdmissionQueue {
            ordered: vec![first, second, third],
            concurrency_limit: 2,
        };

        assert_eq!(queue.position(first), Some(1));
        assert_eq!(queue.position(third), Some(3));
        // A task that has left the queue has no position to report.
        assert_eq!(queue.position(TaskId::new()), None);
        assert_eq!(queue.concurrency_limit, 2);
    }
}
