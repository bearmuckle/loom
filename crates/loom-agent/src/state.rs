use super::*;

impl AgentRuntime {
    pub fn export_state(&self) -> AgentRuntimeState {
        AgentRuntimeState {
            session_id: self.session_id,
            task: self.task.clone(),
            run: self.run.clone(),
            plan: self.plan.clone(),
            messages: self.messages.clone(),
            message_timeline_ordinals: self.message_timeline_ordinals.clone(),
            last_project_message_sequence: self.last_project_message_sequence,
            last_queued_direction_sequence: self.last_queued_direction_sequence,
            attempts: self.attempts.clone(),
            pending_approval: self
                .pending_approval
                .as_ref()
                .map(|pending| pending.call.clone()),
            pending_tool_execution: self.pending_tool_execution.clone(),
            pending_project_join: self.pending_project_join.clone(),
            pending_input: self.pending_input.clone(),
            last_failed_call: self.last_failed_call.clone(),
            next_message_id: self.next_message_id,
            active_message_id: self.active_message_id,
            approval_policy: self.approval_policy.clone(),
            options: self.options.clone(),
            usage: self.usage.clone(),
            context_inspection: self.context_inspection.clone(),
            context_checkpoint: self.context_checkpoint.clone(),
            provider_cursor: self.provider_cursor,
            step_id: self.step_id,
            step_index: self.step_index,
            activities: self.activities.clone(),
            interactions: self.interactions.clone(),
        }
    }

    pub fn from_state(
        mut state: AgentRuntimeState,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
    ) -> Result<Self> {
        if state.session_id != state.run.session_id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "agent runtime state session ids do not match",
                false,
            ));
        }
        if state.pending_approval.is_some()
            && !matches!(
                state.run.state,
                AgentRunState::AwaitingApproval | AgentRunState::Paused
            )
        {
            state.pending_approval = None;
        }
        if state.pending_input.is_some()
            && !matches!(
                state.run.state,
                AgentRunState::NeedsInput | AgentRunState::Paused
            )
        {
            state.pending_input = None;
        }
        if state.pending_tool_execution.is_some() && state.pending_project_join.is_some() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "agent runtime state has both a pending tool and a project join continuation",
                false,
            ));
        }
        let resolved_at = Timestamp::now();
        for interaction in &mut state.interactions {
            if interaction.status != loom_protocol::AgentInteractionStatus::Pending {
                continue;
            }
            let active = interaction.attempt_id == state.run.attempt_id
                && interaction.control_revision == state.run.control_revision
                && match interaction.kind {
                    loom_protocol::AgentInteractionKind::ToolApproval => state
                        .pending_approval
                        .as_ref()
                        .is_some_and(|call| interaction.tool_call_id == Some(call.id)),
                    loom_protocol::AgentInteractionKind::UserInput => state.pending_input.is_some(),
                };
            if !active {
                interaction.status = loom_protocol::AgentInteractionStatus::Abandoned;
                interaction.resolved_at = Some(resolved_at);
            }
        }
        if state.attempts.is_empty() {
            state.attempts.push(loom_protocol::AgentRunAttemptRecord {
                run_id: state.run.id,
                session_id: state.session_id,
                id: state.run.attempt_id,
                number: 1,
                state: state.run.state,
                checkpoint_id: state.options.checkpoint_id,
                started_at: state.run.started_at,
                completed_at: state.run.completed_at,
            });
        }
        if !state.attempts.iter().any(|attempt| {
            attempt.id == state.run.attempt_id
                && attempt.run_id == state.run.id
                && attempt.session_id == state.session_id
                && attempt.state == state.run.state
        }) {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "agent runtime state does not contain its current attempt",
                false,
            ));
        }
        if provider.descriptor().id != state.task.model {
            return Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!(
                    "provider model '{}' does not match persisted model '{}'",
                    provider.descriptor().id.as_str(),
                    state.task.model.as_str()
                ),
                false,
            ));
        }
        let mut activities = state.activities;
        let mut message_timeline_ordinals = state.message_timeline_ordinals;
        if message_timeline_ordinals.len() > state.messages.len() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "agent runtime state has more timeline ordinals than transcript messages",
                false,
            ));
        }
        let has_timeline_metadata = !message_timeline_ordinals.is_empty()
            || activities
                .iter()
                .any(|activity| activity.timeline_ordinal != 0);
        let next_timeline_ordinal = if !has_timeline_metadata
            && (!state.messages.is_empty() || !activities.is_empty())
        {
            let mut next_timeline_ordinal = 0;
            for _ in &state.messages {
                message_timeline_ordinals.push(next_timeline_ordinal);
                next_timeline_ordinal = next_timeline_ordinal.checked_add(1).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "legacy run timeline ordinal space is exhausted",
                        false,
                    )
                })?;
            }
            for activity in &mut activities {
                activity.timeline_ordinal = next_timeline_ordinal;
                next_timeline_ordinal = next_timeline_ordinal.checked_add(1).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "legacy run timeline ordinal space is exhausted",
                        false,
                    )
                })?;
            }
            next_timeline_ordinal
        } else {
            let maximum = message_timeline_ordinals
                .iter()
                .copied()
                .chain(activities.iter().map(|activity| activity.timeline_ordinal))
                .max();
            let mut next = match maximum {
                Some(maximum) => maximum.checked_add(1).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "run timeline ordinal space is exhausted",
                        false,
                    )
                })?,
                None => 0,
            };
            while message_timeline_ordinals.len() < state.messages.len() {
                message_timeline_ordinals.push(next);
                next = next.checked_add(1).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "run timeline ordinal space is exhausted",
                        false,
                    )
                })?;
            }
            next
        };
        let mut seen_timeline_ordinals = BTreeSet::new();
        if message_timeline_ordinals
            .iter()
            .copied()
            .chain(activities.iter().map(|activity| activity.timeline_ordinal))
            .any(|ordinal| !seen_timeline_ordinals.insert(ordinal))
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "agent runtime state contains duplicate timeline ordinals",
                false,
            ));
        }
        Ok(Self {
            session_id: state.session_id,
            task: state.task,
            run: state.run,
            attempts: state.attempts,
            plan: state.plan,
            provider: Some(provider),
            tools,
            messages: state.messages,
            message_timeline_ordinals,
            next_timeline_ordinal,
            last_project_message_sequence: state.last_project_message_sequence,
            last_queued_direction_sequence: state.last_queued_direction_sequence,
            pending_approval: state.pending_approval.map(|call| PendingApproval { call }),
            pending_tool_execution: state.pending_tool_execution,
            pending_project_join: state.pending_project_join,
            pending_input: state.pending_input,
            last_failed_call: state.last_failed_call,
            next_message_id: state.next_message_id,
            active_message_id: state.active_message_id,
            approval_policy: state.approval_policy,
            options: state.options,
            usage: state.usage,
            context_inspection: state.context_inspection,
            context_checkpoint: state.context_checkpoint,
            provider_cursor: state.provider_cursor,
            step_id: state.step_id,
            step_index: state.step_index,
            activities,
            interactions: state.interactions,
            denied_tool_calls: BTreeSet::new(),
            output_truncation_retries: 0,
            control: RunControl::new(),
            observer: None,
            flush_offset: 0,
        })
    }
}
