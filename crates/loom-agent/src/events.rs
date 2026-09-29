use super::*;

impl AgentRuntime {
    /// Publishes the events of the current call that the observer has not seen.
    pub(crate) fn flush_prefix(&mut self, events: &[AgentEvent]) {
        publish_events(self.observer.as_deref(), events, &mut self.flush_offset);
    }

    pub(crate) fn publish_progress(&mut self, result: Result<RunProgress>) -> Result<RunProgress> {
        match result {
            Ok(progress) => {
                let events = self.publish(Ok(progress.events))?;
                Ok(RunProgress {
                    events,
                    continues: progress.continues,
                })
            }
            Err(error) => {
                self.flush_offset = 0;
                Err(error)
            }
        }
    }

    /// Publishes anything left over and ends the current call.
    pub(crate) fn publish(&mut self, result: Result<Vec<AgentEvent>>) -> Result<Vec<AgentEvent>> {
        match result {
            Ok(events) => {
                self.flush_prefix(&events);
                self.flush_offset = 0;
                Ok(events)
            }
            Err(error) => {
                self.flush_offset = 0;
                Err(error)
            }
        }
    }

    /// Applies a pause or interrupt that was requested while the run was busy.
    pub(crate) fn apply_control_request(&mut self) -> Option<Vec<AgentEvent>> {
        if matches!(
            self.run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            self.control.clear_request();
            return None;
        }
        if self.control.is_interrupt_requested() {
            self.control.clear_request();
            self.step_id = None;
            let mut events = self.set_state(AgentRunState::Cancelled);
            self.run.completed_at = Some(Timestamp::now());
            self.run.summary = Some("Agent run interrupted by the user".to_owned());
            events.push(AgentEvent::RunCompleted {
                snapshot: self.run.clone(),
            });
            return Some(events);
        }
        if self.control.is_pause_requested() {
            self.control.clear_request();
            self.step_id = None;
            if self.run.state == AgentRunState::Paused {
                return Some(Vec::new());
            }
            return Some(self.set_state(AgentRunState::Paused));
        }
        None
    }
}
