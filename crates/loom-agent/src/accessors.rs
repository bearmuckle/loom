use super::*;

impl AgentRuntime {
    pub fn run_id(&self) -> RunId {
        self.run.id
    }

    /// Handle used to pause or interrupt this run while it is executing.
    pub fn control(&self) -> RunControl {
        self.control.clone()
    }

    /// Installs an observer that receives every event as it is produced,
    /// including assistant deltas that arrive mid-completion.
    ///
    /// When an observer is installed the returned event vectors are still
    /// complete; callers that journal through the observer must not journal the
    /// returned events again.
    pub fn set_event_observer(&mut self, observer: AgentEventObserver) {
        self.observer = Some(observer);
    }

    pub(crate) fn take_provider(&mut self) -> Result<Box<dyn ModelProvider>> {
        self.provider.take().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "agent run has no model provider attached",
                false,
            )
        })
    }

    pub(crate) fn provider(&self) -> Result<&dyn ModelProvider> {
        self.provider.as_deref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "agent run has no model provider attached",
                false,
            )
        })
    }

    pub fn session_id(&self) -> AgentSessionId {
        self.session_id
    }

    pub fn snapshot(&self) -> AgentRunSnapshot {
        self.run.clone()
    }

    pub fn usage(&self) -> UsageSnapshot {
        self.usage.clone()
    }

    pub fn limits(&self) -> &SessionLimits {
        &self.options.limits
    }

    pub fn limit_status(&self) -> LimitStatus {
        let mut usage = self.usage.clone();
        usage.elapsed_ms = Timestamp::now()
            .as_unix_millis()
            .saturating_sub(self.run.started_at.as_unix_millis());
        LimitStatus::new(self.options.limits.clone(), usage)
    }

    pub fn context_inspection(&self) -> Option<ContextInspection> {
        self.context_inspection.clone()
    }

    pub fn plan(&self) -> AgentPlan {
        self.plan.clone()
    }

    pub fn pending_approval(&self) -> Option<ToolCall> {
        self.pending_approval
            .as_ref()
            .map(|pending| pending.call.clone())
    }

    pub fn pending_input(&self) -> Option<String> {
        self.pending_input.clone()
    }

    pub fn pending_project_join(&self) -> Option<ProjectJoinContinuation> {
        self.pending_project_join.clone()
    }

    pub fn messages(&self) -> Vec<ModelMessage> {
        self.messages.clone()
    }

    pub fn last_project_message_sequence(&self) -> u64 {
        self.last_project_message_sequence
    }

    pub fn last_queued_direction_sequence(&self) -> u64 {
        self.last_queued_direction_sequence
    }

    /// Sets the durable inbox cursor before a new run begins in this session.
    pub fn set_project_message_cursor(&mut self, sequence: u64) {
        self.last_project_message_sequence = self.last_project_message_sequence.max(sequence);
    }

    /// Injects one durable project message at a safe model-turn boundary.
    /// Returns false when it was already delivered or the runtime is blocked.
    pub fn append_project_message(&mut self, message: &AgentMessageRecord) -> Result<bool> {
        if message.project_sequence <= self.last_project_message_sequence {
            return Ok(false);
        }
        if self.pending_tool_execution.is_some()
            || self.pending_project_join.is_some()
            || self.pending_approval.is_some()
            || self.pending_input.is_some()
            || !matches!(
                self.run.state,
                AgentRunState::Planning
                    | AgentRunState::Executing
                    | AgentRunState::Evaluating
                    | AgentRunState::Completed
                    | AgentRunState::Failed
                    | AgentRunState::Cancelled
            )
        {
            return Ok(false);
        }
        let kind = match message.kind {
            loom_core::AgentMessageKind::Progress => "progress",
            loom_core::AgentMessageKind::Result => "result",
            loom_core::AgentMessageKind::Question => "question",
            loom_core::AgentMessageKind::Blocker => "blocker",
            loom_core::AgentMessageKind::Direction => "direction",
            loom_core::AgentMessageKind::Answer => "answer",
        };
        let task = message
            .task_id
            .map(|task_id| format!("; task {task_id}"))
            .unwrap_or_default();
        let mut model_message = ModelMessage::new(
            MessageRole::User,
            format!(
                "[Project message {} from agent {} ({kind}{task})]\n{}",
                message.project_sequence, message.sender_session_id, message.body
            ),
        );
        model_message.name = Some("loom_project_message".to_owned());
        self.push_message(model_message);
        self.last_project_message_sequence = message.project_sequence;
        Ok(true)
    }

    /// Appends a system-generated project notification turn.
    ///
    /// Used to wake a manager that finished its turn when a child finished
    /// without sending its own result. The message is framed like a project
    /// message so it is treated as collaborator input, not a user instruction.
    pub fn notify_project_child_finished(&mut self, body: String) -> Result<()> {
        let mut message = ModelMessage::new(MessageRole::User, body);
        message.name = Some("loom_project_message".to_owned());
        self.push_message(message);
        Ok(())
    }

    /// Delivers one user direction that was queued while the run was executing.
    ///
    /// A direction is applied only at a safe model-turn boundary. It answers a
    /// pending input prompt when one is open, and otherwise is appended as a
    /// user turn. Returns `true` when the direction was delivered (or was
    /// already delivered) and the cursor advanced; `false` when the run is
    /// blocked on an interaction or continuation and must stay queued.
    pub fn deliver_queued_direction(&mut self, sequence: u64, message: &str) -> Result<bool> {
        if sequence <= self.last_queued_direction_sequence {
            return Ok(true);
        }
        if self.pending_tool_execution.is_some()
            || self.pending_project_join.is_some()
            || self.pending_approval.is_some()
            || matches!(
                self.run.state,
                AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
            )
        {
            return Ok(false);
        }
        let attempt_id = self.run.attempt_id;
        let control_revision = self.run.control_revision;
        let mut interaction_id = None;
        if self.pending_input.is_some() {
            let pending = self.pending_interaction_id(
                loom_protocol::AgentInteractionKind::UserInput,
                attempt_id,
                control_revision,
                None,
            )?;
            interaction_id = Some(pending);
            let revision = self.next_control_revision()?;
            self.resolve_interaction(
                pending,
                loom_protocol::AgentInteractionStatus::Answered,
                None,
            )?;
            self.run.control_revision = revision;
            self.pending_input = None;
        }
        self.push_message(ModelMessage::new(MessageRole::User, message.to_owned()));
        self.last_queued_direction_sequence = sequence;
        self.last_failed_call = None;
        self.run.completed_at = None;
        self.run.summary = None;
        self.active_message_id = None;
        let mut events = vec![AgentEvent::UserMessage {
            run_id: self.run.id,
            attempt_id: self.run.attempt_id,
            control_revision: self.run.control_revision,
            interaction_id,
            text: message.to_owned(),
        }];
        events.extend(self.set_state(AgentRunState::Executing));
        self.publish(Ok(events))?;
        Ok(true)
    }

    /// Parks the exact pending tool call as a durable project-join continuation.
    ///
    /// The operation is idempotent for the same wait id and call. A different
    /// continuation cannot replace an already parked call, and the call must
    /// still be the runtime's pending execution.
    pub fn park_pending_project_join(
        &mut self,
        wait_id: impl Into<String>,
        original_call: ToolCall,
    ) -> Result<Vec<AgentEvent>> {
        let result = self.park_pending_project_join_inner(wait_id.into(), original_call);
        self.publish(result)
    }

    pub(crate) fn park_pending_project_join_inner(
        &mut self,
        wait_id: String,
        original_call: ToolCall,
    ) -> Result<Vec<AgentEvent>> {
        if wait_id.trim().is_empty() || wait_id.len() > 256 {
            return Err(LoomError::invalid_request(
                "project join wait id must contain 1 to 256 bytes",
            ));
        }
        if let Some(existing) = &self.pending_project_join {
            if existing.wait_id == wait_id && existing.call == original_call {
                return Ok(Vec::new());
            }
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "a different project join continuation is already parked",
                false,
            ));
        }
        if self.pending_tool_execution.as_ref() != Some(&original_call) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "project join call is not the pending tool execution",
                false,
            ));
        }
        if self.pending_approval.is_some() || self.pending_input.is_some() {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent runtime has another pending interaction",
                false,
            ));
        }
        if !matches!(
            self.run.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not active while parking a project join",
                false,
            ));
        }

        self.pending_tool_execution = None;
        self.pending_project_join = Some(ProjectJoinContinuation {
            wait_id,
            call: original_call.clone(),
        });
        let mut events = vec![AgentEvent::ToolCallStarted {
            run_id: self.run.id,
            call: original_call,
        }];
        events.extend(self.set_state(AgentRunState::Paused));
        Ok(events)
    }

    /// Abandons a parked project join so a newer user direction can be handled.
    ///
    /// The original deferred tool call receives a bounded failure result and the
    /// continuation is cleared, so the owner can abandon the durable wait and
    /// resume the run without replaying an uncertain external tool effect.
    /// Returns no events when no join is parked.
    pub fn abandon_project_join(&mut self, reason: impl Into<String>) -> Result<Vec<AgentEvent>> {
        let result = self.abandon_project_join_inner(reason.into());
        self.publish(result)
    }

    pub(crate) fn abandon_project_join_inner(&mut self, reason: String) -> Result<Vec<AgentEvent>> {
        let Some(continuation) = self.pending_project_join.as_ref() else {
            return Ok(Vec::new());
        };
        let wait_id = continuation.wait_id.clone();
        let call = continuation.call.clone();
        let result = ToolResult {
            tool_call_id: call.id,
            name: call.name,
            success: true,
            output: format!("Project child wait was superseded: {reason}"),
        };
        let progress = self.complete_project_join_inner(&wait_id, result)?;
        Ok(progress.events)
    }

    /// Completes a previously parked project join exactly once.
    ///
    /// Both the durable wait id and the original model tool-call id/name must
    /// match the parked continuation. The resulting Tool message is appended
    /// to conversation history so the next model step can continue normally.
    pub fn complete_project_join(
        &mut self,
        wait_id: &str,
        result: ToolResult,
    ) -> Result<RunProgress> {
        let result = self.complete_project_join_inner(wait_id, result);
        self.publish_progress(result)
    }

    pub(crate) fn complete_project_join_inner(
        &mut self,
        wait_id: &str,
        result: ToolResult,
    ) -> Result<RunProgress> {
        let Some(continuation) = self.pending_project_join.as_ref() else {
            let already_completed = self.messages.iter().any(|message| {
                message.role == MessageRole::Tool
                    && message.tool_call_id == Some(result.tool_call_id)
                    && message.name.as_deref() == Some(result.name.as_str())
                    && message.content == result.output
            });
            if already_completed {
                return Ok(RunProgress::blocked(Vec::new()));
            }
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "there is no pending project join continuation",
                false,
            ));
        };
        if continuation.wait_id != wait_id {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "project join wait id does not match the pending continuation",
                false,
            ));
        }
        if result.tool_call_id != continuation.call.id || result.name != continuation.call.name {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "project join result does not match the original tool call",
                false,
            ));
        }

        let continuation = self
            .pending_project_join
            .take()
            .expect("the validated project join continuation must still be present");
        let mut events = Vec::new();
        if !result.output.is_empty() {
            events.push(AgentEvent::ToolOutputChunk {
                run_id: self.run.id,
                tool_call_id: continuation.call.id,
                chunk: result.output.clone(),
            });
        }
        events.push(AgentEvent::ToolCallCompleted {
            run_id: self.run.id,
            result: result.clone(),
        });
        events.push(self.complete_tool_activity(
            &result,
            if result.success {
                AgentActivityStatus::Completed
            } else {
                AgentActivityStatus::Failed
            },
        ));
        self.push_message(ModelMessage {
            role: MessageRole::Tool,
            content: result.output,
            name: Some(result.name),
            tool_call_id: Some(result.tool_call_id),
            tool_calls: Vec::new(),
            reasoning_content: None,
        });
        self.last_failed_call = None;
        let continues = matches!(
            self.run.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        );
        let progress = if continues {
            RunProgress::running(events)
        } else {
            RunProgress::blocked(events)
        };
        Ok(progress)
    }

    pub fn checkpoint_id(&self) -> Option<loom_core::CheckpointId> {
        self.options.checkpoint_id
    }

    pub fn add_evidence(&mut self, links: impl IntoIterator<Item = EvidenceLink>) {
        self.run.evidence.extend(links);
    }
}
