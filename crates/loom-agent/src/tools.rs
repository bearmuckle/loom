use super::*;

impl AgentRuntime {
    /// Refreshes the GitHub token and write grant from the backend so a settings
    /// change applies to an already-registered run at the next step.
    pub fn set_github_access(&mut self, token: Option<String>, write_access: bool) {
        self.tools.set_github_access(token, write_access);
    }

    pub(crate) fn execute_tool(
        &mut self,
        call: &ToolCall,
        events: &mut Vec<AgentEvent>,
    ) -> ToolResult {
        events.push(AgentEvent::ToolCallStarted {
            run_id: self.run.id,
            call: call.clone(),
        });
        // Publish the start before running the tool so a long or blocked command
        // is reported as running instead of queued.
        self.flush_prefix(events);
        let result = self.tools.execute(call);
        if !result.output.is_empty() {
            events.push(AgentEvent::ToolOutputChunk {
                run_id: self.run.id,
                tool_call_id: call.id,
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
            content: result.output.clone(),
            name: Some(result.name.clone()),
            tool_call_id: Some(result.tool_call_id),
            tool_calls: Vec::new(),
            reasoning_content: None,
        });
        self.flush_prefix(events);
        result
    }

    pub(crate) fn start_activity(&mut self, mut activity: AgentActivityRecord) -> AgentEvent {
        activity.timeline_ordinal = self.allocate_timeline_ordinal();
        let run_id = activity.run_id;
        self.activities.push(activity.clone());
        AgentEvent::ActivityRecorded { run_id, activity }
    }

    pub(crate) fn allocate_timeline_ordinal(&mut self) -> u64 {
        let ordinal = self.next_timeline_ordinal;
        self.next_timeline_ordinal = ordinal
            .checked_add(1)
            .expect("run timeline ordinal space is exhausted");
        ordinal
    }

    pub(crate) fn push_message(&mut self, message: ModelMessage) {
        let ordinal = self.allocate_timeline_ordinal();
        self.message_timeline_ordinals.push(ordinal);
        self.messages.push(message);
    }

    /// The first tool call the model requested that has no matching tool result.
    /// An unanswered call means a step was interrupted between the model
    /// response and the tool result being recorded.
    pub(crate) fn next_unanswered_tool_call(&self) -> Option<ToolCall> {
        let answered = self
            .messages
            .iter()
            .filter_map(|message| message.tool_call_id)
            .collect::<BTreeSet<_>>();
        self.messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .flat_map(|message| message.tool_calls.iter())
            .find(|call| !answered.contains(&call.id))
            .cloned()
    }

    pub(crate) fn start_tool_activity(
        &mut self,
        call: &ToolCall,
        parent_id: Option<ActivityId>,
    ) -> AgentEvent {
        let (kind, data) = activity_data_for_call(call, None);
        self.start_activity(AgentActivityRecord {
            id: ActivityId::new(),
            run_id: self.run.id,
            timeline_ordinal: 0,
            parent_id,
            step_id: self.step_id,
            kind,
            status: AgentActivityStatus::Started,
            started_at: Timestamp::now(),
            completed_at: None,
            elapsed_ms: None,
            data,
        })
    }

    pub(crate) fn update_activity_status(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
        status: AgentActivityStatus,
    ) -> AgentEvent {
        let index = self
            .activities
            .iter()
            .rposition(|activity| activity_contains_call(activity, tool_call_id))
            .expect("tool activity must be started before its status changes");
        let mut activity = self.activities[index].clone();
        activity.status = status;
        self.activities[index] = activity.clone();
        AgentEvent::ActivityRecorded {
            run_id: self.run.id,
            activity,
        }
    }

    pub(crate) fn complete_tool_activity(
        &mut self,
        result: &ToolResult,
        status: AgentActivityStatus,
    ) -> AgentEvent {
        let index = self
            .activities
            .iter()
            .rposition(|activity| activity_contains_call(activity, result.tool_call_id));
        let Some(index) = index else {
            return self.start_activity(AgentActivityRecord {
                id: ActivityId::new(),
                run_id: self.run.id,
                timeline_ordinal: 0,
                parent_id: None,
                step_id: self.step_id,
                kind: AgentActivityKind::ToolCall,
                status,
                started_at: Timestamp::now(),
                completed_at: Some(Timestamp::now()),
                elapsed_ms: Some(0),
                data: AgentActivityData::ToolCall {
                    call: ToolCall {
                        id: result.tool_call_id,
                        name: result.name.clone(),
                        arguments: serde_json::Value::Null,
                    },
                    result: Some(result.clone()),
                },
            });
        };
        let mut activity = self.activities[index].clone();
        let completed_at = Timestamp::now();
        activity.status = status;
        activity.completed_at = Some(completed_at);
        activity.elapsed_ms = Some(
            completed_at
                .as_unix_millis()
                .saturating_sub(activity.started_at.as_unix_millis()),
        );
        activity.data = activity_data_with_result(activity.data, result.clone());
        self.activities[index] = activity.clone();
        AgentEvent::ActivityRecorded {
            run_id: self.run.id,
            activity,
        }
    }

    pub(crate) fn complete_activity(
        &mut self,
        activity_id: ActivityId,
        status: AgentActivityStatus,
        started_at: Timestamp,
    ) -> AgentEvent {
        let completed_at = Timestamp::now();
        let activity = self
            .activities
            .iter_mut()
            .find(|activity| activity.id == activity_id)
            .expect("started activity must be present");
        activity.status = status;
        activity.completed_at = Some(completed_at);
        activity.elapsed_ms = Some(
            completed_at
                .as_unix_millis()
                .saturating_sub(started_at.as_unix_millis()),
        );
        AgentEvent::ActivityRecorded {
            run_id: self.run.id,
            activity: activity.clone(),
        }
    }

    pub(crate) fn take_pending(
        &mut self,
        tool_call_id: loom_core::ToolCallId,
    ) -> Result<PendingApproval> {
        let pending = self.pending_approval.take().ok_or_else(|| {
            LoomError::new(
                ErrorCode::InvalidState,
                "agent run is not waiting for tool approval",
                false,
            )
        })?;
        if pending.call.id != tool_call_id {
            self.pending_approval = Some(pending);
            return Err(LoomError::invalid_request(format!(
                "tool approval is waiting for '{}'",
                tool_call_id
            )));
        }
        Ok(pending)
    }

    pub(crate) fn open_interaction(
        &mut self,
        kind: loom_protocol::AgentInteractionKind,
        prompt: String,
        tool_call_id: Option<loom_core::ToolCallId>,
        control_revision: u64,
    ) -> InteractionId {
        let id = InteractionId::new();
        self.interactions.push(AgentInteractionRecord {
            id,
            run_id: self.run.id,
            session_id: self.session_id,
            attempt_id: self.run.attempt_id,
            control_revision,
            kind,
            status: loom_protocol::AgentInteractionStatus::Pending,
            tool_call_id,
            prompt,
            decision: None,
            created_at: Timestamp::now(),
            resolved_at: None,
        });
        id
    }

    pub(crate) fn pending_interaction_id(
        &self,
        kind: loom_protocol::AgentInteractionKind,
        attempt_id: loom_core::RunAttemptId,
        control_revision: u64,
        tool_call_id: Option<loom_core::ToolCallId>,
    ) -> Result<InteractionId> {
        self.interactions
            .iter()
            .rev()
            .find(|interaction| {
                interaction.kind == kind
                    && interaction.status == loom_protocol::AgentInteractionStatus::Pending
                    && interaction.attempt_id == attempt_id
                    && interaction.control_revision == control_revision
                    && interaction.tool_call_id == tool_call_id
            })
            .map(|interaction| interaction.id)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::InvalidState,
                    "pending run interaction is missing from durable history",
                    false,
                )
            })
    }

    pub(crate) fn resolve_interaction(
        &mut self,
        interaction_id: InteractionId,
        status: loom_protocol::AgentInteractionStatus,
        decision: Option<ApprovalDecision>,
    ) -> Result<()> {
        let interaction = self
            .interactions
            .iter_mut()
            .find(|interaction| interaction.id == interaction_id)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::InvalidState,
                    "run interaction is missing from runtime history",
                    false,
                )
            })?;
        if interaction.status != loom_protocol::AgentInteractionStatus::Pending {
            return Err(LoomError::invalid_state(
                "run interaction has already been resolved",
            ));
        }
        interaction.status = status;
        interaction.decision = decision;
        interaction.resolved_at = Some(Timestamp::now());
        Ok(())
    }

    pub(crate) fn abandon_pending_interactions(&mut self, attempt_id: loom_core::RunAttemptId) {
        let resolved_at = Timestamp::now();
        for interaction in &mut self.interactions {
            if interaction.attempt_id == attempt_id
                && interaction.status == loom_protocol::AgentInteractionStatus::Pending
            {
                interaction.status = loom_protocol::AgentInteractionStatus::Abandoned;
                interaction.resolved_at = Some(resolved_at);
            }
        }
    }
}
