use super::*;

impl AgentRuntime {
    pub(crate) fn exceeded_limits(&mut self) -> Option<LimitStatus> {
        self.usage.elapsed_ms = Timestamp::now()
            .as_unix_millis()
            .saturating_sub(self.run.started_at.as_unix_millis());
        let status = LimitStatus::new(self.options.limits.clone(), self.usage.clone());
        status.is_exceeded().then_some(status)
    }

    pub(crate) fn append_assistant_text(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut()
            && last.role == MessageRole::Assistant
        {
            last.content.push_str(text);
            return;
        }
        self.push_message(ModelMessage::new(MessageRole::Assistant, text));
    }

    /// Keeps provider reasoning on the assistant turn so providers that require
    /// it to be echoed back (DeepSeek thinking mode) accept the next request.
    pub(crate) fn append_assistant_reasoning(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut()
            && last.role == MessageRole::Assistant
        {
            last.reasoning_content
                .get_or_insert_with(String::new)
                .push_str(text);
            return;
        }
        let mut message = ModelMessage::new(MessageRole::Assistant, "");
        message.reasoning_content = Some(text.to_owned());
        self.push_message(message);
    }

    pub(crate) fn append_assistant_tool_call(&mut self, call: ToolCall) {
        if let Some(last) = self.messages.last_mut()
            && last.role == MessageRole::Assistant
        {
            last.tool_calls.push(call);
            return;
        }
        self.push_message(ModelMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            name: None,
            tool_call_id: None,
            tool_calls: vec![call],
            reasoning_content: None,
        });
    }

    pub(crate) fn assistant_message_id(&mut self) -> u64 {
        if let Some(message_id) = self.active_message_id {
            return message_id;
        }
        let message_id = self.next_message_id;
        self.next_message_id = self.next_message_id.saturating_add(1);
        self.active_message_id = Some(message_id);
        message_id
    }

    pub(crate) fn set_state(&mut self, state: AgentRunState) -> Vec<AgentEvent> {
        if matches!(
            state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            self.abandon_pending_interactions(self.run.attempt_id);
            self.pending_approval = None;
            self.pending_tool_execution = None;
            self.pending_input = None;
        }
        if self.run.state == state {
            return Vec::new();
        }
        self.run.state = state;
        let updated_at = Timestamp::now();
        self.run.updated_at = updated_at;
        let attempt = self
            .attempts
            .iter_mut()
            .rfind(|attempt| attempt.id == self.run.attempt_id)
            .expect("the current run attempt must be recorded");
        attempt.state = state;
        attempt.completed_at = if matches!(
            state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            Some(updated_at)
        } else {
            None
        };
        vec![AgentEvent::RunStateChanged {
            run_id: self.run.id,
            state,
        }]
    }

    pub(crate) fn finish_completed(&mut self) -> Vec<AgentEvent> {
        let mut events = self.set_state(AgentRunState::Completed);
        self.run.completed_at = Some(Timestamp::now());
        self.run.summary = Some(format!("Completed task: {}", self.task.task));
        events.push(AgentEvent::RunCompleted {
            snapshot: self.run.clone(),
        });
        events
    }

    pub(crate) fn finish_cancelled(&mut self) -> Vec<AgentEvent> {
        let mut events = self.set_state(AgentRunState::Cancelled);
        self.run.completed_at = Some(Timestamp::now());
        self.run.summary = Some("Model cancelled the run".to_owned());
        events.push(AgentEvent::RunCompleted {
            snapshot: self.run.clone(),
        });
        events
    }

    pub(crate) fn finish_failed(&mut self, reason: impl Into<String>) -> Vec<AgentEvent> {
        let reason = reason.into();
        let mut events = self.set_state(AgentRunState::Failed);
        self.run.completed_at = Some(Timestamp::now());
        self.run.summary = Some(format!("Agent failed: {reason}"));
        events.push(AgentEvent::RunCompleted {
            snapshot: self.run.clone(),
        });
        events
    }
}
