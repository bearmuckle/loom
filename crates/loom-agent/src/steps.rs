use super::*;

/// User-supplied system instructions with the server-owned built-in tool
/// guidance appended. The guidance is derived from the tools actually
/// advertised this step, so it stays consistent with the request without being
/// persisted or duplicated into the stored transcript prefix.
pub(crate) fn system_instructions_with_tool_guidance(
    system_instructions: Option<&str>,
    tools: &[loom_model::ToolDefinition],
) -> Option<String> {
    let system_instructions = system_instructions.filter(|text| !text.trim().is_empty());
    match (system_instructions, loom_tools::tool_guidance(tools)) {
        (Some(system_instructions), Some(guidance)) => {
            Some(format!("{system_instructions}\n\n{guidance}"))
        }
        (Some(system_instructions), None) => Some(system_instructions.to_owned()),
        (None, guidance) => guidance,
    }
}

impl AgentRuntime {
    /// Drives the run until it finishes or needs a human.
    pub(crate) fn advance(&mut self) -> Result<Vec<AgentEvent>> {
        let mut events = Vec::new();
        loop {
            if self.advance_step(&mut events)? == StepOutcome::Blocked {
                return Ok(events);
            }
        }
    }

    /// Runs one model step. An owner that drives the run itself calls this so
    /// the run does not hold its lock across the whole loop.
    pub fn run_step(&mut self) -> Result<RunProgress> {
        let mut events = Vec::new();
        let outcome = self.advance_step(&mut events);
        match outcome {
            Ok(outcome) => {
                let events = self.publish(Ok(events))?;
                Ok(RunProgress {
                    events,
                    continues: outcome == StepOutcome::Continue,
                })
            }
            Err(error) => {
                let _ = self.publish(Ok(events));
                Err(error)
            }
        }
    }

    pub(crate) fn advance_step(&mut self, events: &mut Vec<AgentEvent>) -> Result<StepOutcome> {
        if let Some(control_events) = self.apply_control_request() {
            events.extend(control_events);
            return Ok(StepOutcome::Blocked);
        }
        if self.pending_project_join.is_some() {
            return Ok(StepOutcome::Blocked);
        }
        if let Some(call) = self.pending_tool_execution.clone() {
            if let Some(wait_id) = self.tools.prepare_deferred(&call) {
                events.extend(self.park_pending_project_join_inner(wait_id, call)?);
                return Ok(StepOutcome::Blocked);
            }
            self.pending_tool_execution = None;
            events.extend(self.set_state(AgentRunState::Executing));
            self.execute_tool(&call, events);
            self.last_failed_call = None;
            return Ok(StepOutcome::Continue);
        }
        if self.pending_approval.is_some()
            || matches!(
                self.run.state,
                AgentRunState::Completed
                    | AgentRunState::Failed
                    | AgentRunState::Cancelled
                    | AgentRunState::Paused
                    | AgentRunState::NeedsInput
            )
        {
            return Ok(StepOutcome::Blocked);
        }
        if let Some(status) = self.exceeded_limits() {
            events.push(AgentEvent::RunLimitReached {
                run_id: self.run.id,
                status,
            });
            events.extend(self.finish_failed("agent session limit reached"));
            return Ok(StepOutcome::Blocked);
        }
        if self
            .messages
            .last()
            .is_some_and(|message| message.role == MessageRole::Tool)
        {
            events.extend(self.set_state(AgentRunState::Evaluating));
        } else if self.run.state != AgentRunState::Executing {
            events.extend(self.set_state(AgentRunState::Executing));
        }
        let (request, inspection) = match self.model_request() {
            Ok(request) => request,
            Err(error) => {
                events.push(AgentEvent::ContextError {
                    run_id: self.run.id,
                    error: error.clone(),
                });
                if error.code == ErrorCode::ContextLimitExceeded {
                    let mut status = self.limit_status();
                    status.exceeded.push(LimitKind::ContextTokens);
                    events.push(AgentEvent::RunLimitReached {
                        run_id: self.run.id,
                        status,
                    });
                }
                events.extend(self.finish_failed(error.message));
                return Ok(StepOutcome::Blocked);
            }
        };
        self.context_checkpoint = inspection.summary.clone();
        self.context_inspection = Some(inspection.clone());
        events.push(AgentEvent::ContextInspected {
            run_id: self.run.id,
            inspection,
        });
        let step_id = StepId::new();
        self.step_id = Some(step_id);
        let step_index = self.step_index;
        events.push(AgentEvent::StepStarted {
            run_id: self.run.id,
            step_id,
            index: step_index,
        });
        let model_activity_id = ActivityId::new();
        let model_started_at = Timestamp::now();
        events.push(self.start_activity(AgentActivityRecord {
            id: model_activity_id,
            run_id: self.run.id,
            timeline_ordinal: 0,
            parent_id: None,
            step_id: Some(step_id),
            kind: AgentActivityKind::ModelTurn,
            status: AgentActivityStatus::Started,
            started_at: model_started_at,
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::ModelTurn {
                model: self.task.model.clone(),
            },
        }));
        self.provider_cursor = self.provider_cursor.saturating_add(1);
        let mut ctx = StepContext::new(step_id, self.step_index, model_activity_id);
        self.flush_prefix(events);
        let base = self.flush_offset;
        let token = self.control.stream_token();
        let mut provider = self.take_provider()?;
        let stream_result = provider.stream(&request, &token, &mut |event| {
            self.handle_stream_event(event, &mut ctx)
        });
        self.provider = Some(provider);
        let StepContext {
            events: step_events,
            published,
            saw_tool_call,
            completed,
            completion_guarded,
            continue_after_truncation,
            finished,
            buffered_tools,
            ..
        } = ctx;
        events.extend(step_events);
        self.flush_offset = base.saturating_add(published);
        if !buffered_tools.is_empty() {
            // Tool execution is its own phase: report it as executing instead
            // of leaving the run in the evaluating state set for the model turn.
            events.extend(self.set_state(AgentRunState::Executing));
            self.flush_tool_calls(buffered_tools, events);
        }
        let model_status = match &stream_result {
            Err(_) if self.control.is_stopping() => AgentActivityStatus::Cancelled,
            Err(_) => AgentActivityStatus::Failed,
            Ok(()) if !saw_tool_call && !completed => AgentActivityStatus::Failed,
            Ok(()) => AgentActivityStatus::Completed,
        };
        events.push(self.complete_activity(model_activity_id, model_status, model_started_at));
        match stream_result {
            Ok(()) => {}
            Err(error) if self.control.is_stopping() => {
                // A pause or interrupt cancelled the in-flight completion.
                debug_assert_eq!(error.code, ErrorCode::RequestCancelled);
            }
            Err(error) => {
                self.step_id = None;
                events.push(AgentEvent::ProviderError {
                    run_id: self.run.id,
                    error: error.clone(),
                });
                events.extend(self.finish_failed(error.message));
                return Ok(StepOutcome::Blocked);
            }
        }
        if let Some(events_from_control) = self.apply_control_request() {
            events.extend(events_from_control);
            return Ok(StepOutcome::Blocked);
        }
        if finished {
            return Ok(StepOutcome::Blocked);
        }
        if !saw_tool_call && !completed {
            self.step_id = None;
            events.extend(self.finish_failed("model returned an empty stream"));
            return Ok(StepOutcome::Blocked);
        }
        if completed && !saw_tool_call {
            if completion_guarded || continue_after_truncation {
                return Ok(StepOutcome::Continue);
            }
            return Ok(StepOutcome::Blocked);
        }
        Ok(StepOutcome::Continue)
    }

    /// Executes the tool calls buffered during one model turn. Consecutive
    /// read-only calls run concurrently; writes, commands, and other calls run
    /// sequentially in model order. Results are always recorded in model order.
    fn flush_tool_calls(&mut self, calls: Vec<ToolCall>, events: &mut Vec<AgentEvent>) {
        let mut index = 0;
        while index < calls.len() {
            let is_read_only = self
                .tools
                .action_kind(&calls[index])
                .is_some_and(|kind| kind == loom_core::ActionKind::Read);
            if is_read_only {
                let mut end = index;
                while end < calls.len()
                    && self
                        .tools
                        .action_kind(&calls[end])
                        .is_some_and(|kind| kind == loom_core::ActionKind::Read)
                {
                    end += 1;
                }
                let batch = calls[index..end].to_vec();
                self.execute_read_only_batch(&batch, events);
                index = end;
            } else {
                let _ = self.execute_tool(&calls[index], events);
                index += 1;
            }
        }
        if !calls.is_empty() {
            self.last_failed_call = None;
        }
    }

    fn execute_read_only_batch(&mut self, calls: &[ToolCall], events: &mut Vec<AgentEvent>) {
        for call in calls {
            events.push(AgentEvent::ToolCallStarted {
                run_id: self.run.id,
                call: call.clone(),
            });
        }
        // Publish the starts before executing so blocked reads report as running.
        self.flush_prefix(events);
        let tools = self.tools.clone();
        let cancel = self.control.stream_token();
        // Read-only calls overlap on a bounded, shared pool instead of a fresh
        // thread per call, so a large batch cannot exhaust OS threads.
        let jobs = calls
            .iter()
            .cloned()
            .map(|call| {
                let tools = tools.clone();
                let cancel = cancel.clone();
                move || tools.execute_with_cancel(&call, &cancel)
            })
            .collect::<Vec<_>>();
        let results = self
            .read_pool
            .execute_batch(jobs)
            .into_iter()
            .enumerate()
            .map(|(index, outcome)| {
                outcome.unwrap_or_else(|_| ToolResult {
                    tool_call_id: calls[index].id,
                    name: calls[index].name.clone(),
                    success: false,
                    output: "tool execution failed".to_owned(),
                })
            })
            .collect::<Vec<_>>();
        for result in &results {
            self.record_tool_result(result, events);
            self.flush_prefix(events);
        }
    }

    fn record_tool_result(&mut self, result: &ToolResult, events: &mut Vec<AgentEvent>) {
        if !result.output.is_empty() {
            events.push(AgentEvent::ToolOutputChunk {
                run_id: self.run.id,
                tool_call_id: result.tool_call_id,
                chunk: result.output.clone(),
            });
        }
        events.push(AgentEvent::ToolCallCompleted {
            run_id: self.run.id,
            result: result.clone(),
        });
        let status = if result.success {
            AgentActivityStatus::Completed
        } else {
            AgentActivityStatus::Failed
        };
        events.push(self.complete_tool_activity(result, status));
        self.push_message(ModelMessage {
            role: MessageRole::Tool,
            content: result.output.clone(),
            name: Some(result.name.clone()),
            tool_call_id: Some(result.tool_call_id),
            tool_calls: Vec::new(),
            reasoning_content: None,
        });
    }

    /// Applies one streamed model event to the run.
    ///
    /// This runs inside the provider's stream callback, so an assistant delta is
    /// journaled while the completion is still arriving.
    pub(crate) fn handle_stream_event(
        &mut self,
        event: ModelStreamEvent,
        ctx: &mut StepContext,
    ) -> Result<StreamFlow> {
        let flow = self.handle_stream_event_inner(event, ctx);
        publish_events(self.observer.as_deref(), &ctx.events, &mut ctx.published);
        flow
    }

    pub(crate) fn handle_stream_event_inner(
        &mut self,
        event: ModelStreamEvent,
        ctx: &mut StepContext,
    ) -> Result<StreamFlow> {
        match event {
            ModelStreamEvent::TextDelta { text } => {
                if !text.is_empty() {
                    self.append_assistant_text(&text);
                    let message_id = self.assistant_message_id();
                    ctx.events.push(AgentEvent::AssistantMessageDelta {
                        run_id: self.run.id,
                        message_id,
                        text,
                    });
                }
            }
            ModelStreamEvent::ReasoningDelta { text } => {
                // Record the field even when empty so a thinking-mode provider
                // sees it echoed back on the next request; only surface non-empty
                // reasoning to the transcript.
                self.append_assistant_reasoning(&text);
                if !text.is_empty() {
                    let message_id = self.assistant_message_id();
                    ctx.events.push(AgentEvent::ReasoningDelta {
                        run_id: self.run.id,
                        message_id,
                        text,
                    });
                }
            }
            ModelStreamEvent::ToolCallDelta { call } => {
                self.active_message_id = None;
                if self
                    .options
                    .limits
                    .max_tool_calls
                    .is_some_and(|limit| self.usage.tool_calls >= limit)
                {
                    let status = self.limit_status();
                    ctx.events.push(AgentEvent::RunLimitReached {
                        run_id: self.run.id,
                        status,
                    });
                    ctx.events
                        .extend(self.finish_failed("agent session limit reached"));
                    self.step_id = None;
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
                self.usage.add_tool_call();
                self.append_assistant_tool_call(call.clone());
                self.output_truncation_retries = 0;
                ctx.events.push(AgentEvent::RunUsageUpdated {
                    run_id: self.run.id,
                    usage: self.usage.clone(),
                });
                ctx.saw_tool_call = true;
                ctx.events
                    .push(self.start_tool_activity(&call, Some(ctx.activity_id)));
                ctx.events.push(AgentEvent::ToolCallRequested {
                    run_id: self.run.id,
                    call: call.clone(),
                });
                let kind = ToolKind::from_name(&call.name);
                let Some(action_kind) = self.tools.action_kind(&call) else {
                    let output = format!("unknown tool '{}'", call.name);
                    let result = ToolResult {
                        tool_call_id: call.id,
                        name: call.name.clone(),
                        success: false,
                        output: output.clone(),
                    };
                    ctx.events.push(AgentEvent::ToolCallCompleted {
                        run_id: self.run.id,
                        result: result.clone(),
                    });
                    ctx.events
                        .push(self.complete_tool_activity(&result, AgentActivityStatus::Failed));
                    self.push_message(ModelMessage {
                        role: MessageRole::Tool,
                        content: output.clone(),
                        name: Some(result.name.clone()),
                        tool_call_id: Some(result.tool_call_id),
                        tool_calls: Vec::new(),
                        reasoning_content: None,
                    });
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    return Ok(StreamFlow::Stop);
                };
                let evaluation = self
                    .tools
                    .policy_evaluation(&call, &self.approval_policy)
                    .unwrap_or_else(|| {
                        PolicyEvaluation::evaluate(&self.approval_policy, action_kind, &call.name)
                    });
                ctx.events.push(AgentEvent::ToolPolicyEvaluated {
                    run_id: self.run.id,
                    call: call.clone(),
                    evaluation: evaluation.clone(),
                });
                if matches!(evaluation.decision, loom_core::PolicyDecision::Deny) {
                    let result = ToolResult {
                        tool_call_id: call.id,
                        name: call.name.clone(),
                        success: false,
                        output: evaluation.reason,
                    };
                    ctx.events.push(AgentEvent::ToolCallCompleted {
                        run_id: self.run.id,
                        result: result.clone(),
                    });
                    ctx.events
                        .push(self.complete_tool_activity(&result, AgentActivityStatus::Failed));
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    self.push_message(ModelMessage {
                        role: MessageRole::Tool,
                        content: result.output,
                        name: Some(result.name),
                        tool_call_id: Some(result.tool_call_id),
                        tool_calls: Vec::new(),
                        reasoning_content: None,
                    });
                    return Ok(StreamFlow::Stop);
                }
                if matches!(
                    evaluation.decision,
                    loom_core::PolicyDecision::RequireApproval
                ) {
                    if self.denied_tool_calls.contains(&tool_call_signature(&call)) {
                        let result = ToolResult {
                            tool_call_id: call.id,
                            name: call.name.clone(),
                            success: false,
                            output: "this action was rejected earlier in the run; choose a different approach".to_owned(),
                        };
                        ctx.events.push(AgentEvent::ToolCallCompleted {
                            run_id: self.run.id,
                            result: result.clone(),
                        });
                        ctx.events.push(
                            self.complete_tool_activity(&result, AgentActivityStatus::Failed),
                        );
                        self.push_message(ModelMessage {
                            role: MessageRole::Tool,
                            content: result.output,
                            name: Some(result.name),
                            tool_call_id: Some(call.id),
                            tool_calls: Vec::new(),
                            reasoning_content: None,
                        });
                        return Ok(StreamFlow::Continue);
                    }
                    let control_revision = self.next_control_revision()?;
                    self.pending_approval = Some(PendingApproval { call: call.clone() });
                    self.run.control_revision = control_revision;
                    let interaction_id = self.open_interaction(
                        loom_protocol::AgentInteractionKind::ToolApproval,
                        "Tool approval requested".to_owned(),
                        Some(call.id),
                        control_revision,
                    );
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    ctx.events
                        .extend(self.set_state(AgentRunState::AwaitingApproval));
                    ctx.events.push(
                        self.update_activity_status(call.id, AgentActivityStatus::AwaitingApproval),
                    );
                    ctx.events.push(AgentEvent::ToolApprovalRequired {
                        run_id: self.run.id,
                        attempt_id: self.run.attempt_id,
                        control_revision,
                        interaction_id,
                        call,
                    });
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
                if kind == Some(ToolKind::ProposePlan) {
                    let steps = call
                        .arguments
                        .get("steps")
                        .and_then(serde_json::Value::as_array)
                        .map(|steps| {
                            steps
                                .iter()
                                .enumerate()
                                .filter_map(|(index, step)| {
                                    step.as_str().filter(|step| !step.trim().is_empty()).map(
                                        |description| AgentPlanStep {
                                            id: format!("step-{}", index + 1),
                                            description: description.to_owned(),
                                        },
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if steps.is_empty() {
                        let output = "propose_plan requires a non-empty steps array".to_owned();
                        let result = ToolResult {
                            tool_call_id: call.id,
                            name: call.name.clone(),
                            success: false,
                            output: output.clone(),
                        };
                        ctx.events.push(AgentEvent::ToolCallCompleted {
                            run_id: self.run.id,
                            result: result.clone(),
                        });
                        ctx.events.push(
                            self.complete_tool_activity(&result, AgentActivityStatus::Failed),
                        );
                        self.step_id = None;
                        self.step_index = self.step_index.saturating_add(1);
                        ctx.events.push(AgentEvent::StepCompleted {
                            run_id: self.run.id,
                            step_id: ctx.step_id,
                            index: ctx.step_index,
                        });
                        self.push_message(ModelMessage {
                            role: MessageRole::Tool,
                            content: output,
                            name: Some(call.name.clone()),
                            tool_call_id: Some(call.id),
                            tool_calls: Vec::new(),
                            reasoning_content: None,
                        });
                        return Ok(StreamFlow::Stop);
                    }
                    self.plan = AgentPlan {
                        steps: steps.clone(),
                    };
                    ctx.events.push(AgentEvent::PlanProposed {
                        run_id: self.run.id,
                        plan: self.plan.clone(),
                    });
                    let result = ToolResult::success(&call, "plan proposed".to_owned());
                    ctx.events.push(AgentEvent::ToolCallCompleted {
                        run_id: self.run.id,
                        result: result.clone(),
                    });
                    ctx.events
                        .push(self.complete_tool_activity(&result, AgentActivityStatus::Completed));
                    self.push_message(ModelMessage {
                        role: MessageRole::Tool,
                        content: result.output.clone(),
                        name: Some(result.name.clone()),
                        tool_call_id: Some(result.tool_call_id),
                        tool_calls: Vec::new(),
                        reasoning_content: None,
                    });
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    return Ok(StreamFlow::Stop);
                }
                if kind == Some(ToolKind::AskUser) {
                    let Some(prompt) = call
                        .arguments
                        .get("prompt")
                        .and_then(serde_json::Value::as_str)
                        .filter(|prompt| {
                            !prompt.trim().is_empty()
                                && prompt.len() <= MAX_INTERACTION_PROMPT_BYTES
                        })
                    else {
                        let output = format!(
                            "ask_user requires a non-empty prompt of at most {MAX_INTERACTION_PROMPT_BYTES} bytes"
                        );
                        let result = ToolResult {
                            tool_call_id: call.id,
                            name: call.name.clone(),
                            success: false,
                            output: output.clone(),
                        };
                        ctx.events.push(AgentEvent::ToolCallCompleted {
                            run_id: self.run.id,
                            result: result.clone(),
                        });
                        ctx.events.push(
                            self.complete_tool_activity(&result, AgentActivityStatus::Failed),
                        );
                        self.step_id = None;
                        self.step_index = self.step_index.saturating_add(1);
                        ctx.events.push(AgentEvent::StepCompleted {
                            run_id: self.run.id,
                            step_id: ctx.step_id,
                            index: ctx.step_index,
                        });
                        self.push_message(ModelMessage {
                            role: MessageRole::Tool,
                            content: output,
                            name: Some(call.name.clone()),
                            tool_call_id: Some(call.id),
                            tool_calls: Vec::new(),
                            reasoning_content: None,
                        });
                        return Ok(StreamFlow::Stop);
                    };
                    let control_revision = self.next_control_revision()?;
                    let interaction_id = self.open_interaction(
                        loom_protocol::AgentInteractionKind::UserInput,
                        prompt.to_owned(),
                        None,
                        control_revision,
                    );
                    self.pending_input = Some(prompt.to_owned());
                    self.run.control_revision = control_revision;
                    self.push_message(ModelMessage {
                        role: MessageRole::Tool,
                        content: format!("Waiting for user input: {prompt}"),
                        name: Some(call.name.clone()),
                        tool_call_id: Some(call.id),
                        tool_calls: Vec::new(),
                        reasoning_content: None,
                    });
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    ctx.events.extend(self.set_state(AgentRunState::NeedsInput));
                    ctx.events.push(
                        self.update_activity_status(call.id, AgentActivityStatus::AwaitingInput),
                    );
                    ctx.events.push(AgentEvent::NeedsInput {
                        run_id: self.run.id,
                        attempt_id: self.run.attempt_id,
                        control_revision,
                        interaction_id,
                        prompt: prompt.to_owned(),
                    });
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
                // Deferred tools (project joins) need the step boundary so the
                // run can park durably; queue them for the run driver like the
                // previous single-tool path.
                if self.tools.prepare_deferred(&call).is_some() {
                    self.pending_tool_execution = Some(call.clone());
                    self.step_id = None;
                    self.step_index = self.step_index.saturating_add(1);
                    ctx.events.push(AgentEvent::StepCompleted {
                        run_id: self.run.id,
                        step_id: ctx.step_id,
                        index: ctx.step_index,
                    });
                    return Ok(StreamFlow::Stop);
                }
                // Every remaining policy decision allows execution. Buffer the
                // call so consecutive read-only calls in this model turn can run
                // concurrently once the completion is fully received.
                ctx.buffered_tools.push(call);
                return Ok(StreamFlow::Continue);
            }
            ModelStreamEvent::Usage { usage } => {
                self.usage.add_tokens(
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cached_input_tokens,
                );
                self.usage.add_cost_micros(
                    usage
                        .input_tokens
                        .saturating_mul(self.options.input_cost_micros_per_1k)
                        .saturating_add(
                            usage
                                .output_tokens
                                .saturating_mul(self.options.output_cost_micros_per_1k),
                        )
                        / 1_000,
                );
                ctx.events.push(AgentEvent::RunUsage {
                    run_id: self.run.id,
                    usage,
                });
                ctx.events.push(AgentEvent::RunUsageUpdated {
                    run_id: self.run.id,
                    usage: self.usage.clone(),
                });
                if let Some(status) = self.exceeded_limits() {
                    ctx.events.push(AgentEvent::RunLimitReached {
                        run_id: self.run.id,
                        status,
                    });
                    ctx.events
                        .extend(self.finish_failed("agent session limit reached"));
                    self.step_id = None;
                    ctx.finished = true;
                    return Ok(StreamFlow::Stop);
                }
            }
            ModelStreamEvent::Completed { reason } => {
                ctx.completed = true;
                self.step_id = None;
                self.step_index = self.step_index.saturating_add(1);
                ctx.events.push(AgentEvent::StepCompleted {
                    run_id: self.run.id,
                    step_id: ctx.step_id,
                    index: ctx.step_index,
                });
                if !ctx.saw_tool_call {
                    if matches!(reason, loom_model::FinishReason::Stop) {
                        self.output_truncation_retries = 0;
                        if let Some(blocker) = self.tools.completion_blocker() {
                            self.active_message_id = None;
                            self.push_message(ModelMessage {
                                role: MessageRole::User,
                                content: format!("Project completion is blocked: {blocker}"),
                                name: Some("loom_project_completion_guard".to_owned()),
                                tool_call_id: None,
                                tool_calls: Vec::new(),
                                reasoning_content: None,
                            });
                            ctx.completion_guarded = true;
                        } else {
                            ctx.events.extend(self.finish_completed());
                        }
                    } else if matches!(reason, loom_model::FinishReason::Cancelled) {
                        ctx.events.extend(self.finish_cancelled());
                    } else if matches!(reason, loom_model::FinishReason::Length) {
                        if self.output_truncation_retries >= MAX_OUTPUT_TRUNCATION_RETRIES {
                            ctx.events.extend(self.finish_failed(
                                "the model kept reaching the output token limit without finishing",
                            ));
                        } else {
                            self.output_truncation_retries =
                                self.output_truncation_retries.saturating_add(1);
                            self.active_message_id = None;
                            self.push_message(ModelMessage {
                                role: MessageRole::User,
                                content:
                                    "Your previous response was cut off because it reached the \
                                     output token limit. Continue from where it stopped, keep the \
                                     answer concise, and prefer a tool call or a short summary."
                                        .to_owned(),
                                name: Some("loom_output_limit_continuation".to_owned()),
                                tool_call_id: None,
                                tool_calls: Vec::new(),
                                reasoning_content: None,
                            });
                            ctx.continue_after_truncation = true;
                            log::warn!(
                                "model output was truncated by the output token limit (run {}, \
                                 continuation {} of {})",
                                self.run.id,
                                self.output_truncation_retries,
                                MAX_OUTPUT_TRUNCATION_RETRIES,
                            );
                        }
                    } else if let loom_model::FinishReason::ErrorWithMessage { message } = reason {
                        ctx.events.extend(self.finish_failed(message));
                    } else if matches!(reason, loom_model::FinishReason::Error) {
                        log::error!(
                            "model stream ended with an error finish reason and no provider details (run {})",
                            self.run.id
                        );
                        ctx.events.extend(self.finish_failed(
                            "the model reported an error, but the provider supplied no details",
                        ));
                    } else {
                        ctx.events
                            .extend(self.finish_failed(format!("model finished with {reason:?}")));
                    }
                }
            }
        }
        if self.pending_approval.is_some()
            || matches!(
                self.run.state,
                AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
            )
        {
            ctx.finished = true;
            return Ok(StreamFlow::Stop);
        }
        if self.control.is_stopping() {
            return Ok(StreamFlow::Stop);
        }
        Ok(StreamFlow::Continue)
    }

    pub(crate) fn validate_control_revision(
        &self,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
    ) -> Result<()> {
        if self.run.attempt_id != attempt_id
            || self.run.control_revision != expected_control_revision
        {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                "agent interaction belongs to a different attempt or control revision",
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn next_control_revision(&self) -> Result<u64> {
        self.run.control_revision.checked_add(1).ok_or_else(|| {
            LoomError::new(
                ErrorCode::InvalidState,
                "agent control revision is exhausted",
                false,
            )
        })
    }

    pub(crate) fn model_request(&self) -> Result<(ModelRequest, ContextInspection)> {
        let provider = self.provider()?;
        let initial_count = initial_messages(&self.task).len();
        let source = self.messages.get(initial_count..).unwrap_or_default();
        // Repair legacy transcripts before budgeting, so repair cannot reinsert
        // an oversized output after the assembler has removed it.
        let conversation = repair_tool_transcript(source.to_vec(), source);
        let checkpoint = match self.context_checkpoint.as_ref() {
            Some(summary)
                if summary.projection_version == CONTEXT_PROJECTION_VERSION
                    && summary.source_message_count <= conversation.len()
                    && summary.source_digest
                        == context_projection_digest(
                            &conversation[..summary.source_message_count],
                        )? =>
            {
                Some(summary)
            }
            _ => None,
        };
        let boundary = checkpoint.map_or(0, |summary| summary.source_message_count);
        let mut options = self.options.context.clone();
        // Explicit overrides may lower, but never raise, a known model limit.
        // Unknown models use a conservative bounded fallback instead of an
        // unlimited conversation. Users can supply a known window explicitly.
        options.context_window =
            match (options.context_window, provider.descriptor().context_window) {
                (Some(requested), Some(model)) => Some(requested.min(u64::from(model))),
                (Some(requested), None) => Some(requested),
                (None, model) => Some(model.map_or(8_192, u64::from)),
            };
        for model_limit in [
            provider.descriptor().max_input_tokens.map(u64::from),
            self.options.limits.max_input_tokens,
        ]
        .into_iter()
        .flatten()
        {
            options.max_input_tokens = Some(
                options
                    .max_input_tokens
                    .map_or(model_limit, |requested| requested.min(model_limit)),
            );
        }
        let mut reserve = loom_context::output_reserve_for_model(
            &options,
            provider.descriptor().max_output_tokens.map(u64::from),
        );
        if let Some(session_output_limit) = self.options.limits.max_output_tokens {
            reserve = reserve.min(session_output_limit.saturating_sub(self.usage.output_tokens));
        }
        if reserve == 0 {
            return Err(LoomError::new(
                ErrorCode::ContextLimitExceeded,
                "no output tokens remain in the agent session budget",
                false,
            ));
        }
        reserve = reserve.min(u64::from(u32::MAX));
        options.reserved_output_tokens = Some(reserve);
        let budget = loom_protocol::ContextBudget::new(
            options.context_window,
            options.max_input_tokens,
            reserve,
        )?;
        let tools = if provider.descriptor().capabilities.tool_calling {
            self.tools.definitions()
        } else {
            Vec::new()
        };
        let system_instructions = system_instructions_with_tool_guidance(
            self.task.system_instructions.as_deref(),
            &tools,
        );
        let completion = CompletionOptions {
            max_output_tokens: Some(reserve as u32),
            ..Default::default()
        };
        let tool_tokens = provider.count_tokens(&ModelRequest {
            model: self.task.model.clone(),
            messages: Vec::new(),
            tools: tools.clone(),
            options: completion.clone(),
        });
        let message_limit = budget
            .effective_input_tokens
            .unwrap_or(u64::MAX)
            .saturating_sub(tool_tokens);
        let message_options = ContextAssemblyOptions {
            context_window: None,
            max_input_tokens: Some(message_limit),
            reserved_output_tokens: Some(reserve),
        };
        let assembly = ContextAssembler::assemble_with_counter(
            &ContextInput {
                system_instructions,
                repository_instructions: self.task.repository_instructions.clone(),
                task: self.task.task.clone(),
                conversation: conversation[boundary..].to_vec(),
                existing_summary: checkpoint.map(|summary| summary.text.clone()),
                latest_user_message: conversation
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, message)| message.role == MessageRole::User)
                    .filter(|(index, _)| *index < boundary)
                    .map(|(_, message)| message.content.clone()),
            },
            &message_options,
            |message| {
                provider.count_tokens(&ModelRequest {
                    model: self.task.model.clone(),
                    messages: vec![message.clone()],
                    tools: Vec::new(),
                    options: completion.clone(),
                })
            },
        )?;
        let mut messages = assembly.messages;
        echo_reasoning_on_assistant_turns(&mut messages, !tools.is_empty());
        let request = ModelRequest {
            model: self.task.model.clone(),
            messages,
            tools,
            options: completion,
        };
        let request_tokens = provider.count_tokens(&request);
        if budget
            .effective_input_tokens
            .is_some_and(|limit| request_tokens > limit)
        {
            return Err(LoomError::new(
                ErrorCode::ContextLimitExceeded,
                format!(
                    "assembled messages and tools use {request_tokens} tokens, above the context budget"
                ),
                false,
            ));
        }
        let mut inspection = assembly.inspection;
        inspection.budget = budget;
        inspection.included_tokens = request_tokens;
        inspection.total_tokens = inspection
            .total_tokens
            .saturating_add(tool_tokens)
            .max(request_tokens);
        if let Some(summary) = &mut inspection.summary {
            summary.source_message_count += boundary;
            summary.projection_version = CONTEXT_PROJECTION_VERSION;
            summary.source_digest =
                context_projection_digest(&conversation[..summary.source_message_count])?;
        }
        if provider.descriptor().context_window.is_none()
            && self.options.context.context_window.is_none()
        {
            inspection.items.push(loom_protocol::ContextItem {
                kind: loom_protocol::ContextItemKind::SystemInstructions,
                label: "Model context window unknown; using an 8192-token fallback".to_owned(),
                estimated_tokens: 0,
                included: true,
                omission_reason: None,
            });
        }
        Ok((request, inspection))
    }
}
