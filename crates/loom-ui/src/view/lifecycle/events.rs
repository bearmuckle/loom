use super::*;

impl LoomView {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn collect_events_since(
        &mut self,
        mut after_sequence: Option<EventSequence>,
        mut fallback: Option<AgentRunSnapshotProjection>,
    ) -> Result<(), LoomError> {
        let session_id = self.active_session.id;
        for resync_attempt in 0..=1 {
            let response = self
                .connection
                .request(RequestEnvelope::new(ClientRequest::Events(
                    EventsRequest::GetSessionEvents {
                        session_id: Some(session_id),
                        workspace_id: None,
                        after_sequence,
                        stream_epoch: self.event_stream_epoch.clone(),
                    },
                )));
            match response.result? {
                ServerResponse::Events(EventsResponse::SessionEvents {
                    events,
                    stream_epoch,
                }) => {
                    self.reset_projection();
                    self.after_sequence = after_sequence;
                    self.event_stream_epoch = stream_epoch;
                    for event in events {
                        self.after_sequence = Some(event.sequence);
                        self.consume_event(&event.event);
                    }
                    if let Some(projection) = fallback.as_ref() {
                        self.seed_plan_from_projection(projection);
                    }
                    if self.timeline.is_empty()
                        && let Some(projection) = fallback
                    {
                        self.apply_run_projection(projection);
                    }
                    return Ok(());
                }
                ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                    session,
                    events,
                    latest_sequence,
                    stream_epoch,
                    ..
                }) if resync_attempt == 0 => {
                    self.event_stream_epoch = stream_epoch;
                    self.active_session = session;
                    let refreshed =
                        self.connection
                            .request(RequestEnvelope::new(ClientRequest::Session(
                                SessionRequest::GetAgentSessionInitialState { session_id },
                            )));
                    if let Ok(ServerResponse::Session(SessionResponse::AgentSessionInitialState(
                        initial,
                    ))) = refreshed.result
                    {
                        after_sequence = Some(initial.cursor);
                        fallback = initial.projection.active_run;
                        self.active_session = initial.projection.session;
                        self.session_state = self.active_session.state;
                        self.auto_approve_actions = initial.projection.auto_approve_actions;
                        self.session_auto_approve_actions
                            .insert(session_id, initial.projection.auto_approve_actions);
                        let _ = events;
                        continue;
                    }
                    self.apply_event_snapshot(events, latest_sequence, fallback);
                    return Ok(());
                }
                ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                    session,
                    events,
                    latest_sequence,
                    stream_epoch,
                    ..
                }) => {
                    self.active_session = session;
                    self.event_stream_epoch = stream_epoch;
                    self.apply_event_snapshot(events, latest_sequence, fallback);
                    return Ok(());
                }
                response => return Err(unexpected_response("session event stream", response)),
            }
        }
        Ok(())
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn apply_event_snapshot(
        &mut self,
        events: Vec<loom_protocol::ServerEventEnvelope>,
        latest_sequence: EventSequence,
        fallback: Option<AgentRunSnapshotProjection>,
    ) {
        self.reset_projection();
        self.after_sequence = Some(latest_sequence);
        for event in events {
            self.after_sequence = Some(event.sequence);
            self.consume_event(&event.event);
        }
        if let Some(projection) = fallback.as_ref() {
            self.seed_plan_from_projection(projection);
        }
        if self.timeline.is_empty()
            && let Some(projection) = fallback
        {
            self.apply_run_projection(projection);
        }
    }

    pub(crate) fn consume_event(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::ProjectTaskUpdated { .. }
            | ServerEvent::ProjectChildWorktreeUpdated { .. }
            | ServerEvent::ProjectAgentCreated { .. }
            | ServerEvent::ProjectAgentUpdated { .. } => {
                self.project_snapshot_stale = true;
            }
            ServerEvent::ProjectAgentMessageAccepted { .. } => {}
            ServerEvent::AgentSessionCreated { snapshot } => {
                self.active_session = snapshot.clone();
                self.session_state = snapshot.state;
                self.update_session_list();
            }
            ServerEvent::AgentSessionStateChanged { current, .. } => {
                self.session_state = *current;
                self.active_session.state = *current;
                self.update_session_list();
            }
            ServerEvent::AgentSessionForked { .. } => {}
            ServerEvent::AgentSessionRenamed { session_id, name } => {
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == *session_id)
                {
                    session.name = name.clone();
                }
                if self.active_session.id == *session_id {
                    self.active_session.name = name.clone();
                    self.update_session_list();
                }
            }
            ServerEvent::AgentSessionArchived { session_id } => {
                // The event can refer to a descendant archived as part of a
                // project cascade, so only apply it to the session it names.
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == *session_id)
                {
                    session.state = AgentSessionState::Archived;
                }
                if self.active_session.id == *session_id {
                    self.session_state = AgentSessionState::Archived;
                    self.active_session.state = AgentSessionState::Archived;
                    self.update_session_list();
                }
            }
            ServerEvent::Agent { event } => self.consume_agent_event(event),
            ServerEvent::SessionFilesystemChanged { change } => {
                self.record_status(format!("Workspace {:?}: {}", change.kind, change.path));
                // The Changes tab renders a fetched snapshot, so a change newer
                // than that snapshot is unviewed content even while the tab is
                // displayed. `consume_event` has no `Context`, so it cannot
                // refetch the snapshot; the user refreshes by re-selecting the
                // tab.
                if self.review.changes_newer_than_snapshot(change.sequence) {
                    self.review.mark_changes_unread_from_event();
                }
            }
            ServerEvent::Terminal { .. } | ServerEvent::Task { .. } => {}
            ServerEvent::ProviderHealthChanged {
                provider_id,
                health,
            } => self.record_status(format!(
                "Provider {} health: {:?}",
                provider_id.as_str(),
                health.state
            )),
        }
    }

    pub(crate) fn consume_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::RunStarted { snapshot } => {
                if self.active_run_id != Some(snapshot.id) {
                    self.transcript_generation = self.transcript_generation.wrapping_add(1);
                    self.transcript_before_ordinal = None;
                    self.transcript_loaded_ordinals.clear();
                    self.transcript_messages.clear();
                    self.transcript_has_older = false;
                    self.transcript_loading = false;
                    self.activity_records.clear();
                    // A plan belongs to one run, so a new run starts without one.
                    self.plan = None;
                    self.plan_collapsed = false;
                    self.review.clear_unread(InspectorTab::Plan);
                }
                self.context_inspection = None;
                self.active_run = Some(snapshot.clone());
                self.active_run_id = Some(snapshot.id);
                self.run_state = Some(snapshot.state);
            }
            AgentEvent::PlanProposed { plan, .. } => {
                let steps = plan
                    .steps
                    .iter()
                    .map(|step| step.description.clone())
                    .collect::<Vec<_>>();
                self.plan = Some(PlanState::new(steps));
                self.plan_collapsed = false;
                self.review.mark_plan_unread();
            }
            AgentEvent::UserMessage {
                run_id,
                attempt_id,
                control_revision,
                text,
                ..
            } => {
                self.update_active_run_control(*run_id, *attempt_id, *control_revision);
                if self
                    .optimistic_messages
                    .first()
                    .is_some_and(|pending| pending == text)
                {
                    self.optimistic_messages.remove(0);
                } else {
                    self.timeline.push(TimelineItem::User(text.clone()));
                }
            }
            AgentEvent::AssistantMessageDelta { text, .. } => {
                push_assistant_text(&mut self.timeline, text);
            }
            AgentEvent::ReasoningDelta { text, .. } => {
                push_assistant_reasoning(&mut self.timeline, text);
            }
            AgentEvent::StepStarted { index, .. } => {
                if let Some(plan) = self.plan.as_mut() {
                    plan.active = Some(*index);
                    self.review.mark_plan_unread();
                }
            }
            AgentEvent::StepCompleted { index, .. } => {
                if let Some(plan) = self.plan.as_mut() {
                    plan.completed.insert(*index);
                    if plan.active == Some(*index) {
                        plan.active = None;
                    }
                    self.review.mark_plan_unread();
                }
            }
            AgentEvent::ContextInspected { inspection, .. } => {
                if inspection.compacted {
                    self.record_status(format!("Context compacted into lossy excerpts (approximately {} tokens removed). Full history is retained.", inspection.omitted_tokens));
                }
                self.context_inspection = Some(inspection.clone());
            }
            AgentEvent::ProviderError { error, .. } | AgentEvent::ContextError { error, .. } => {
                self.timeline.push(TimelineItem::System(SystemNote {
                    tone: SystemTone::Error,
                    heading: Some(format!("agent · {}", error.code)),
                    text: error.message.clone(),
                    retryable: error.retryable,
                }));
            }
            AgentEvent::ToolCallRequested { call, .. } => {
                upsert_tool_part(
                    &mut self.timeline,
                    tool_part_from_call(call, ToolPartStatus::Queued),
                );
            }
            AgentEvent::ToolApprovalRequired {
                run_id,
                attempt_id,
                control_revision,
                call,
                ..
            } => {
                self.update_active_run_control(*run_id, *attempt_id, *control_revision);
                self.pending_approval = Some(call.clone());
                upsert_tool_part(
                    &mut self.timeline,
                    tool_part_from_call(call, ToolPartStatus::AwaitingApproval),
                );
            }
            AgentEvent::ToolPolicyEvaluated { .. } => {}
            AgentEvent::ToolApprovalDecided {
                run_id,
                attempt_id,
                control_revision,
                tool_call_id,
                decision,
                ..
            } => {
                self.update_active_run_control(*run_id, *attempt_id, *control_revision);
                self.pending_approval = None;
                self.approval_request_in_flight = false;
                let status = if *decision == loom_protocol::ApprovalDecision::Approved {
                    ToolPartStatus::Running
                } else {
                    ToolPartStatus::Failed
                };
                upsert_tool_part(
                    &mut self.timeline,
                    ToolPart {
                        id: *tool_call_id,
                        name: String::new(),
                        title: String::new(),
                        status,
                        detail: None,
                        output: None,
                        elapsed_ms: None,
                        approval_pending: false,
                    },
                );
            }
            AgentEvent::ToolCallStarted { call, .. } => {
                upsert_tool_part(
                    &mut self.timeline,
                    tool_part_from_call(call, ToolPartStatus::Running),
                );
            }
            AgentEvent::ToolOutputChunk {
                tool_call_id,
                chunk,
                ..
            } => {
                upsert_tool_part(
                    &mut self.timeline,
                    ToolPart {
                        id: *tool_call_id,
                        name: String::new(),
                        title: String::new(),
                        status: ToolPartStatus::Running,
                        detail: None,
                        output: Some(bounded(chunk)),
                        elapsed_ms: None,
                        approval_pending: false,
                    },
                );
            }
            AgentEvent::ToolCallCompleted { result, .. } => {
                let status = if result.success {
                    ToolPartStatus::Completed
                } else {
                    ToolPartStatus::Failed
                };
                let call = loom_model::ToolCall {
                    id: result.tool_call_id,
                    name: result.name.clone(),
                    arguments: serde_json::Value::Null,
                };
                let mut part = tool_part_from_call(&call, status);
                part.output = (!result.output.is_empty())
                    .then(|| bounded(&humanize_tool_output(&result.name, &result.output)));
                upsert_tool_part(&mut self.timeline, part);
            }
            AgentEvent::ActivityRecorded { activity, .. } => {
                self.activity_records.insert(activity.id, activity.clone());
                if let Some(part) = tool_part_from_activity(activity) {
                    upsert_tool_part(&mut self.timeline, part);
                }
            }
            AgentEvent::NeedsInput {
                run_id,
                attempt_id,
                control_revision,
                prompt,
                ..
            } => {
                self.update_active_run_control(*run_id, *attempt_id, *control_revision);
                self.pending_input = Some(prompt.clone());
                self.timeline.push(TimelineItem::System(SystemNote {
                    tone: SystemTone::Input,
                    heading: Some("Agent needs input".to_owned()),
                    text: prompt.clone(),
                    retryable: false,
                }));
            }
            AgentEvent::RunUsage { usage, .. } => {
                let run = self
                    .review
                    .usage
                    .run
                    .get_or_insert_with(UsageSnapshot::default);
                run.input_tokens = usage.input_tokens;
                run.output_tokens = usage.output_tokens;
                run.cached_input_tokens = usage.cached_input_tokens;
            }
            AgentEvent::RunUsageUpdated { usage, .. } => {
                self.review.usage.run = Some(usage.clone());
            }
            AgentEvent::RunLimitReached { status, .. } => {
                self.record_status(format!("Limit reached: {:?}", status.exceeded));
            }
            AgentEvent::RecoveryRequired { reason, .. } => {
                self.record_status(format!("Recovery required: {reason}"));
            }
            AgentEvent::RunStateChanged { state, .. } => {
                self.run_state = Some(*state);
                self.session_state = session_state_for_run(*state);
                self.active_session.state = self.session_state;
                self.update_session_list();
                if matches!(
                    state,
                    AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                ) {
                    finish_assistant_turn(&mut self.timeline);
                }
            }
            AgentEvent::RunCompleted { snapshot } => {
                self.active_run = Some(snapshot.clone());
                self.active_run_id = Some(snapshot.id);
                self.run_state = Some(snapshot.state);
                self.session_state = session_state_for_run(snapshot.state);
                self.active_session.state = self.session_state;
                self.update_session_list();
                // The run is over, so stop the streaming cursor even when no
                // completion summary is rendered.
                finish_assistant_turn(&mut self.timeline);
                if let Some(summary) = &snapshot.summary
                    && !is_redundant_completion_summary(summary)
                    && !summary.trim().is_empty()
                    && !assistant_turn_matches(
                        &self.timeline,
                        |part| matches!(part, AssistantPart::Text(text) if text == summary),
                    )
                {
                    push_assistant_text(&mut self.timeline, summary);
                    finish_assistant_turn(&mut self.timeline);
                }
                if !assistant_turn_matches(&self.timeline, |part| {
                    matches!(part, AssistantPart::Evidence(_))
                }) {
                    push_assistant_evidence(
                        &mut self.timeline,
                        snapshot
                            .evidence
                            .iter()
                            .map(|link| EvidenceText {
                                label: link.label.clone(),
                                uri: link.uri.clone(),
                            })
                            .collect(),
                    );
                }
            }
        }
    }

    pub(crate) fn update_active_run_control(
        &mut self,
        run_id: RunId,
        attempt_id: loom_core::RunAttemptId,
        control_revision: u64,
    ) {
        if let Some(run) = self.active_run.as_mut().filter(|run| run.id == run_id) {
            run.attempt_id = attempt_id;
            run.control_revision = control_revision;
        }
    }

    /// Seeds the active plan from a run snapshot when the event stream has not
    /// already established one. Progress comes from the backend so a reopened
    /// session keeps its completed and active markers.
    pub(crate) fn seed_plan_from_projection(&mut self, projection: &AgentRunSnapshotProjection) {
        if self.plan.is_some() {
            return;
        }
        self.plan = PlanState::from_projection(
            projection
                .plan
                .iter()
                .map(|step| step.description.clone())
                .collect(),
            &projection.plan_progress,
        );
        if self.plan.is_some() {
            self.review.mark_plan_unread();
        }
    }

    pub(crate) fn apply_run_projection(&mut self, projection: AgentRunSnapshotProjection) {
        self.seed_plan_from_projection(&projection);
        self.active_run_id = Some(projection.run.id);
        self.active_run = Some(projection.run.clone());
        self.run_state = Some(projection.run.state);
        self.pending_approval = projection.pending_approval;
        self.pending_input = projection.pending_input;
        let activity_records = projection.activities;
        for activity in &activity_records {
            self.activity_records.insert(activity.id, activity.clone());
        }
        let message_timeline_ordinals = projection.message_timeline_ordinals;
        self.transcript_messages.clear();
        self.transcript_loaded_ordinals.clear();
        let message_orders_match = message_timeline_ordinals.len() == projection.messages.len();
        if !message_orders_match && !projection.messages.is_empty() {
            log::warn!(
                "run snapshot has {} messages but {} timeline ordinals; waiting for the transcript page",
                projection.messages.len(),
                message_timeline_ordinals.len()
            );
        }
        let messages = projection
            .messages
            .into_iter()
            .enumerate()
            .filter_map(|(index, message)| {
                if !message_orders_match {
                    return None;
                }
                let ordinal = u64::try_from(index).ok()?;
                let timeline_ordinal = *message_timeline_ordinals.get(index)?;
                self.transcript_messages
                    .insert(ordinal, (timeline_ordinal, message.clone()));
                Some((ordinal, timeline_ordinal, message))
            })
            .collect::<Vec<_>>();
        if self.timeline.is_empty() {
            let timeline = timeline_items_from_messages(messages, activity_records.clone());
            self.timeline = timeline;
        } else {
            for activity in &activity_records {
                if let Some(part) = tool_part_from_activity(activity) {
                    upsert_tool_part(&mut self.timeline, part);
                }
            }
        }
        let has_summary = assistant_turn_matches(&self.timeline, |part| {
            matches!(part, AssistantPart::Evidence(_))
        }) || projection.run.summary.as_deref().is_some_and(|summary| {
            assistant_turn_matches(
                &self.timeline,
                |part| matches!(part, AssistantPart::Text(text) if text == summary),
            )
        });
        if !has_summary
            && let Some(summary) = &projection.run.summary
            && !is_redundant_completion_summary(summary)
            && !summary.trim().is_empty()
        {
            push_assistant_text(&mut self.timeline, summary);
            finish_assistant_turn(&mut self.timeline);
            push_assistant_evidence(
                &mut self.timeline,
                projection
                    .run
                    .evidence
                    .iter()
                    .map(|link| EvidenceText {
                        label: link.label.clone(),
                        uri: link.uri.clone(),
                    })
                    .collect(),
            );
        }
    }
}
