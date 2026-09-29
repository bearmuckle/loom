use super::*;

impl AgentRuntime {
    pub fn start(&mut self) -> Result<Vec<AgentEvent>> {
        let result = self.start_inner();
        self.publish(result)
    }

    /// Starts the run without driving it, so an owner can register the run
    /// before any model work happens.
    pub fn begin(&mut self) -> Result<RunProgress> {
        let result = self.begin_inner();
        self.publish_progress(result)
    }

    pub(crate) fn start_inner(&mut self) -> Result<Vec<AgentEvent>> {
        let progress = self.begin_inner()?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    pub(crate) fn begin_inner(&mut self) -> Result<RunProgress> {
        if self.run.state != AgentRunState::Planning {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run has already started",
                false,
            ));
        }
        let mut events = vec![AgentEvent::RunStarted {
            snapshot: self.run.clone(),
        }];
        if !self.plan.steps.is_empty() {
            events.push(AgentEvent::PlanProposed {
                run_id: self.run.id,
                plan: self.plan.clone(),
            });
        }
        events.extend(self.set_state(AgentRunState::Executing));
        Ok(RunProgress::running(events))
    }

    pub fn approve(&mut self, tool_call_id: loom_core::ToolCallId) -> Result<Vec<AgentEvent>> {
        let attempt_id = self.run.attempt_id;
        let control_revision = self.run.control_revision;
        let result = self.approve_inner(tool_call_id, attempt_id, control_revision);
        self.publish(result)
    }

    /// Applies an approval and queues the tool for the run driver.
    pub fn approve_entry(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        let result = self.approve_entry_inner(tool_call_id, attempt_id, expected_control_revision);
        self.publish_progress(result)
    }

    pub(crate) fn approve_inner(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<Vec<AgentEvent>> {
        let progress =
            self.approve_entry_inner(tool_call_id, attempt_id, expected_control_revision)?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    pub(crate) fn approve_entry_inner(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        self.validate_control_revision(attempt_id, expected_control_revision)?;
        if self.run.state != AgentRunState::AwaitingApproval {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for tool approval",
                false,
            ));
        }
        let interaction_id = self.pending_interaction_id(
            loom_protocol::AgentInteractionKind::ToolApproval,
            attempt_id,
            expected_control_revision,
            Some(tool_call_id),
        )?;
        let control_revision = self.next_control_revision()?;
        let pending = self.take_pending(tool_call_id)?;
        self.run.control_revision = control_revision;
        self.resolve_interaction(
            interaction_id,
            loom_protocol::AgentInteractionStatus::Approved,
            Some(ApprovalDecision::Approved),
        )?;
        self.pending_tool_execution = Some(pending.call.clone());
        let mut events = vec![AgentEvent::ToolApprovalDecided {
            run_id: self.run.id,
            attempt_id: self.run.attempt_id,
            control_revision,
            interaction_id,
            tool_call_id,
            decision: ApprovalDecision::Approved,
        }];
        events.extend(self.set_state(AgentRunState::Executing));
        events.push(self.update_activity_status(tool_call_id, AgentActivityStatus::Started));
        Ok(RunProgress::running(events))
    }

    pub fn reject(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        reason: Option<String>,
    ) -> Result<Vec<AgentEvent>> {
        let attempt_id = self.run.attempt_id;
        let control_revision = self.run.control_revision;
        let result = self.reject_entry_inner(tool_call_id, reason, attempt_id, control_revision);
        self.publish(result.map(|progress| progress.events))
    }

    /// Applies a rejection using the interaction revision observed by the client.
    pub fn reject_entry(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        reason: Option<String>,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        let result =
            self.reject_entry_inner(tool_call_id, reason, attempt_id, expected_control_revision);
        self.publish_progress(result)
    }

    pub(crate) fn reject_entry_inner(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        reason: Option<String>,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        self.validate_control_revision(attempt_id, expected_control_revision)?;
        if self.run.state != AgentRunState::AwaitingApproval {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for tool approval",
                false,
            ));
        }
        let interaction_id = self.pending_interaction_id(
            loom_protocol::AgentInteractionKind::ToolApproval,
            attempt_id,
            expected_control_revision,
            Some(tool_call_id),
        )?;
        let control_revision = self.next_control_revision()?;
        let pending = self.take_pending(tool_call_id)?;
        self.run.control_revision = control_revision;
        self.resolve_interaction(
            interaction_id,
            loom_protocol::AgentInteractionStatus::Rejected,
            Some(ApprovalDecision::Rejected),
        )?;
        let mut events = vec![AgentEvent::ToolApprovalDecided {
            run_id: self.run.id,
            attempt_id: self.run.attempt_id,
            control_revision,
            interaction_id,
            tool_call_id,
            decision: ApprovalDecision::Rejected,
        }];
        let output = reason.unwrap_or_else(|| "tool call rejected by the user".to_owned());
        let result = ToolResult {
            tool_call_id,
            name: pending.call.name.clone(),
            success: false,
            output: output.clone(),
        };
        events.push(AgentEvent::ToolCallCompleted {
            run_id: self.run.id,
            result: result.clone(),
        });
        events.push(self.complete_tool_activity(&result, AgentActivityStatus::Failed));
        // A rejection is a tool error the model can react to, not a fatal run
        // error. Remember the denied call so an identical retry is refused
        // without prompting the user again.
        self.denied_tool_calls
            .insert(tool_call_signature(&pending.call));
        self.push_message(ModelMessage {
            role: MessageRole::Tool,
            content: output,
            name: Some(result.name.clone()),
            tool_call_id: Some(tool_call_id),
            tool_calls: Vec::new(),
        });
        events.extend(self.set_state(AgentRunState::Executing));
        Ok(RunProgress::running(events))
    }

    pub fn interrupt(&mut self) -> Result<Vec<AgentEvent>> {
        if matches!(
            self.run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is already finished",
                false,
            ));
        }
        let mut events = self.set_state(AgentRunState::Cancelled);
        self.run.completed_at = Some(Timestamp::now());
        self.run.summary = Some("Agent run interrupted by the user".to_owned());
        events.push(AgentEvent::RunCompleted {
            snapshot: self.run.clone(),
        });
        self.publish(Ok(events))
    }

    pub fn pause(&mut self) -> Result<Vec<AgentEvent>> {
        if matches!(
            self.run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is already finished",
                false,
            ));
        }
        if self.run.state == AgentRunState::Paused {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is already paused",
                false,
            ));
        }
        let events = self.set_state(AgentRunState::Paused);
        self.publish(Ok(events))
    }

    pub fn resume(&mut self) -> Result<Vec<AgentEvent>> {
        let result = self.resume_inner();
        self.publish(result)
    }

    /// Leaves the paused state without driving the run.
    pub fn resume_entry(&mut self) -> Result<RunProgress> {
        let result = self.resume_entry_inner();
        self.publish_progress(result)
    }

    pub(crate) fn resume_inner(&mut self) -> Result<Vec<AgentEvent>> {
        let progress = self.resume_entry_inner()?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    pub(crate) fn resume_entry_inner(&mut self) -> Result<RunProgress> {
        if self.run.state != AgentRunState::Paused {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not paused",
                false,
            ));
        }
        if self.pending_project_join.is_some() {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "project join is still pending",
                false,
            ));
        }
        let events = if self.pending_approval.is_some() {
            self.set_state(AgentRunState::AwaitingApproval)
        } else if self.pending_input.is_some() {
            self.set_state(AgentRunState::NeedsInput)
        } else {
            self.set_state(AgentRunState::Executing)
        };
        if self.pending_approval.is_none() && self.pending_input.is_none() {
            return Ok(RunProgress::running(events));
        }
        Ok(RunProgress::blocked(events))
    }

    pub fn send_message(&mut self, message: impl Into<String>) -> Result<Vec<AgentEvent>> {
        let result = self.send_message_inner(message);
        self.publish(result)
    }

    /// Records a user message without driving the run.
    pub fn message_entry(&mut self, message: impl Into<String>) -> Result<RunProgress> {
        let attempt_id = self.run.attempt_id;
        let control_revision = self.run.control_revision;
        let result = self.message_entry_inner(message, attempt_id, control_revision);
        self.publish_progress(result)
    }

    /// Records a user message only if it targets the current attempt revision.
    pub fn message_entry_at_revision(
        &mut self,
        message: impl Into<String>,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        let result = self.message_entry_inner(message, attempt_id, expected_control_revision);
        self.publish_progress(result)
    }

    pub(crate) fn send_message_inner(
        &mut self,
        message: impl Into<String>,
    ) -> Result<Vec<AgentEvent>> {
        let attempt_id = self.run.attempt_id;
        let control_revision = self.run.control_revision;
        let progress = self.message_entry_inner(message, attempt_id, control_revision)?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    pub(crate) fn message_entry_inner(
        &mut self,
        message: impl Into<String>,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<RunProgress> {
        self.validate_control_revision(attempt_id, expected_control_revision)?;
        let message = message.into();
        if message.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent message must not be empty",
            ));
        }
        if self.pending_approval.is_some() {
            return Err(LoomError::invalid_state(
                "resolve the pending tool approval before sending a message",
            ));
        }
        if self.pending_project_join.is_some() {
            return Err(LoomError::invalid_state(
                "wait for the pending project join before sending a message",
            ));
        }
        if matches!(
            self.run.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            return Err(LoomError::invalid_state(
                "agent run is currently processing a message",
            ));
        }
        let interaction_id = if self.pending_input.is_some() {
            Some(self.pending_interaction_id(
                loom_protocol::AgentInteractionKind::UserInput,
                attempt_id,
                expected_control_revision,
                None,
            )?)
        } else {
            None
        };
        let control_revision = self.next_control_revision()?;
        if let Some(interaction_id) = interaction_id {
            self.resolve_interaction(
                interaction_id,
                loom_protocol::AgentInteractionStatus::Answered,
                None,
            )?;
        }
        self.push_message(ModelMessage::new(MessageRole::User, message.clone()));
        self.pending_input = None;
        self.run.control_revision = control_revision;
        self.active_message_id = None;
        self.last_failed_call = None;
        self.run.completed_at = None;
        self.run.summary = None;
        let mut events = vec![AgentEvent::UserMessage {
            run_id: self.run.id,
            attempt_id: self.run.attempt_id,
            control_revision,
            interaction_id,
            text: message,
        }];
        events.extend(self.set_state(AgentRunState::Executing));
        Ok(RunProgress::running(events))
    }

    pub fn request_input(&mut self, prompt: impl Into<String>) -> Result<Vec<AgentEvent>> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent input prompt must not be empty",
            ));
        }
        if prompt.len() > MAX_INTERACTION_PROMPT_BYTES {
            return Err(LoomError::invalid_request(
                "agent input prompt exceeds the supported size limit",
            ));
        }
        if self.pending_approval.is_some() {
            return Err(LoomError::invalid_state(
                "resolve the pending tool approval before requesting input",
            ));
        }
        if self.pending_project_join.is_some() {
            return Err(LoomError::invalid_state(
                "wait for the pending project join before requesting input",
            ));
        }
        if matches!(
            self.run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) {
            return Err(LoomError::invalid_state(
                "finished agent runs cannot request input",
            ));
        }
        let previous_interaction = if self.pending_input.is_some() {
            Some(self.pending_interaction_id(
                loom_protocol::AgentInteractionKind::UserInput,
                self.run.attempt_id,
                self.run.control_revision,
                None,
            )?)
        } else {
            None
        };
        let control_revision = self.next_control_revision()?;
        if let Some(interaction_id) = previous_interaction {
            self.resolve_interaction(
                interaction_id,
                loom_protocol::AgentInteractionStatus::Abandoned,
                None,
            )?;
        }
        let interaction_id = self.open_interaction(
            loom_protocol::AgentInteractionKind::UserInput,
            prompt.clone(),
            None,
            control_revision,
        );
        self.pending_input = Some(prompt.clone());
        self.run.control_revision = control_revision;
        let mut events = self.set_state(AgentRunState::NeedsInput);
        events.push(AgentEvent::NeedsInput {
            run_id: self.run.id,
            attempt_id: self.run.attempt_id,
            control_revision,
            interaction_id,
            prompt,
        });
        self.publish(Ok(events))
    }

    pub fn recover_after_restart(&mut self) -> Result<Vec<AgentEvent>> {
        let mut events = Vec::new();
        // A parked join has a durable wait record and must remain pending so
        // its eventual audit result can complete the original model call.
        if self.pending_project_join.is_some() {
            if matches!(
                self.run.state,
                AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
            ) {
                events.extend(self.set_state(AgentRunState::Paused));
            }
            return self.publish(Ok(events));
        }
        if let Some(call) = self.pending_tool_execution.take() {
            self.last_failed_call = Some(call);
            events.push(AgentEvent::RecoveryRequired {
                run_id: self.run.id,
                reason: "an approved tool execution was interrupted; its external outcome is unknown and it was not replayed".to_owned(),
            });
            events.extend(
                self.finish_failed(
                    "approved tool execution was interrupted with an unknown outcome",
                ),
            );
            return self.publish(Ok(events));
        }
        // A tool call with no recorded result means the step was interrupted
        // while the tool was running. Replaying it is unsafe, so surface it.
        if !matches!(
            self.run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        ) && self.pending_approval.is_none()
            && self.pending_input.is_none()
            && let Some(call) = self.next_unanswered_tool_call()
        {
            self.last_failed_call = Some(call);
            events.push(AgentEvent::RecoveryRequired {
                run_id: self.run.id,
                reason: "a tool execution was interrupted; its external outcome is unknown and it was not replayed".to_owned(),
            });
            events.extend(
                self.finish_failed("tool execution was interrupted with an unknown outcome"),
            );
            return self.publish(Ok(events));
        }
        if matches!(
            self.run.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            events.extend(self.set_state(AgentRunState::Paused));
        }
        self.publish(Ok(events))
    }

    pub fn retry(&mut self) -> Result<Vec<AgentEvent>> {
        let result = self.retry_inner();
        self.publish(result)
    }

    /// Queues the failed tool step for the run driver to retry.
    pub fn retry_entry(&mut self) -> Result<RunProgress> {
        let result = self.retry_entry_inner();
        self.publish_progress(result)
    }

    pub(crate) fn retry_inner(&mut self) -> Result<Vec<AgentEvent>> {
        let progress = self.retry_entry_inner()?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    pub(crate) fn retry_entry_inner(&mut self) -> Result<RunProgress> {
        let call = self.last_failed_call.clone().ok_or_else(|| {
            LoomError::new(
                ErrorCode::InvalidState,
                "there is no failed tool step to retry",
                false,
            )
        })?;
        if self.run.state != AgentRunState::Failed {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for a retry",
                false,
            ));
        }
        self.run.completed_at = None;
        self.run.summary = None;
        let mut events = self.set_state(AgentRunState::Executing);
        events.push(self.start_tool_activity(&call, None));
        self.pending_tool_execution = Some(call);
        Ok(RunProgress::running(events))
    }

    pub fn retry_from_checkpoint(&mut self) -> Result<Vec<AgentEvent>> {
        let result = self.retry_from_checkpoint_inner();
        self.publish(result)
    }

    /// Resets the run to its checkpoint without driving it.
    pub fn checkpoint_retry_entry(&mut self) -> Result<RunProgress> {
        let result = self.checkpoint_retry_entry_inner();
        self.publish_progress(result)
    }

    pub(crate) fn retry_from_checkpoint_inner(&mut self) -> Result<Vec<AgentEvent>> {
        let progress = self.checkpoint_retry_entry_inner()?;
        let mut events = progress.events;
        if progress.continues {
            events.extend(self.advance()?);
        }
        Ok(events)
    }

    pub(crate) fn checkpoint_retry_entry_inner(&mut self) -> Result<RunProgress> {
        if self.pending_project_join.is_some() {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "a pending project join must complete before retrying from a checkpoint",
                false,
            ));
        }
        if matches!(
            self.run.state,
            AgentRunState::Planning
                | AgentRunState::Executing
                | AgentRunState::AwaitingApproval
                | AgentRunState::Evaluating
                | AgentRunState::Paused
                | AgentRunState::NeedsInput
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run must be stopped before retrying from a checkpoint",
                false,
            ));
        }
        let previous_attempt_id = self.run.attempt_id;
        self.abandon_pending_interactions(previous_attempt_id);
        let attempt_number = self
            .attempts
            .iter()
            .map(|attempt| attempt.number)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::InvalidState,
                    "run attempt number is exhausted",
                    false,
                )
            })?;
        self.run.attempt_id = loom_core::RunAttemptId::new();
        self.run.control_revision = 0;
        self.run.completed_at = None;
        self.run.summary = None;
        let started_at = Timestamp::now();
        self.run.updated_at = started_at;
        self.run.state = AgentRunState::Planning;
        self.attempts.push(loom_protocol::AgentRunAttemptRecord {
            run_id: self.run.id,
            session_id: self.session_id,
            id: self.run.attempt_id,
            number: attempt_number,
            state: AgentRunState::Planning,
            checkpoint_id: self.options.checkpoint_id,
            started_at,
            completed_at: None,
        });
        self.messages = initial_messages(&self.task);
        self.message_timeline_ordinals.clear();
        for _ in 0..self.messages.len() {
            let ordinal = self.allocate_timeline_ordinal();
            self.message_timeline_ordinals.push(ordinal);
        }
        self.pending_approval = None;
        self.pending_tool_execution = None;
        self.pending_project_join = None;
        self.pending_input = None;
        self.last_failed_call = None;
        self.denied_tool_calls.clear();
        self.next_message_id = 0;
        self.active_message_id = None;
        self.usage = UsageSnapshot::default();
        self.context_inspection = None;
        self.context_checkpoint = None;
        if let Some(provider) = self.provider.as_mut() {
            provider.reset();
        }
        self.provider_cursor = 0;
        self.step_id = None;
        self.step_index = 0;
        let events = vec![AgentEvent::RunStateChanged {
            run_id: self.run.id,
            state: AgentRunState::Planning,
        }];
        Ok(RunProgress::running(events))
    }
}
