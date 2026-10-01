use super::*;

impl LoomView {
    pub(crate) fn send_message(&mut self, message: String, cx: &mut Context<Self>) {
        self.status_banner = None;
        #[cfg(target_family = "wasm")]
        if self.browser_demo_mode {
            self.timeline.push(TimelineItem::User(message));
            self.timeline
                .push(TimelineItem::Assistant(AssistantTurn::text(
                    "This is demo mode. The browser client needs to connect to a backend to work.",
                )));
            cx.notify();
            return;
        }
        let backend = match self.backend_for_session(self.active_session.id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("send message", error);
                cx.notify();
                return;
            }
        };
        if self.active_run_id.is_none()
            && !self.demo_workspace
            && self.model.as_str() != "deterministic/demo"
        {
            let node_id = self
                .session_node_ids
                .get(&self.active_session.id)
                .map(String::as_str)
                .unwrap_or_default();
            let validation = if self.model_catalog_node_id.as_deref() != Some(node_id) {
                Err("model availability has not been refreshed for this worker".to_owned())
            } else {
                validate_model_for_node(&self.node_model_catalogs, node_id, &self.model)
            };
            if let Err(reason) = validation {
                let node_name = self
                    .node_names
                    .get(node_id)
                    .map(String::as_str)
                    .unwrap_or(node_id);
                self.record_backend_error(
                    "start run",
                    LoomError::invalid_state(format!(
                        "Cannot start a run on {node_name}: {reason}. Choose a model configured on this worker before sending."
                    )),
                );
                cx.notify();
                return;
            }
        }
        self.sending_message = true;
        let session_title = if self.active_run_id.is_none() {
            self.session_task_cache
                .insert(self.active_session.id, message.clone());
            let title = session_title_from_task(&message);
            self.active_session.name = title.clone();
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| session.id == self.active_session.id)
            {
                session.name = title;
            }
            Some(self.active_session.name.clone())
        } else {
            None
        };
        let request = if let Some(run_id) = self.active_run_id {
            let Some(run) = self.active_run.as_ref().filter(|run| run.id == run_id) else {
                self.sending_message = false;
                self.record_backend_error(
                    "send message",
                    LoomError::invalid_state("active run control state is unavailable"),
                );
                return;
            };
            self.optimistic_messages.push(message.clone());
            ClientRequest::Run(RunRequest::SendAgentMessage {
                run_id,
                attempt_id: run.attempt_id,
                expected_control_revision: run.control_revision,
                message: message.clone(),
            })
        } else {
            if !self.demo_workspace && self.model.as_str() == "deterministic/demo" {
                self.sending_message = false;
                self.record_backend_error(
                    "start run",
                    LoomError::invalid_state(
                        "click Log in in the title bar to connect GitHub Copilot, or configure LOOM_OPENAI_ENDPOINT and LOOM_MODEL before starting a real run",
                    ),
                );
                return;
            }
            ClientRequest::Run(RunRequest::StartSessionAgentRun {
                session_id: self.active_session.id,
                task: message.clone(),
                model: self.model.clone(),
                system_instructions: Some(
                    "Work methodically, use the available tools, and report validation.".to_owned(),
                ),
                repository_instructions: Some(
                    "Keep the change focused and provide reviewable evidence.".to_owned(),
                ),
            })
        };
        self.timeline.push(TimelineItem::User(message));
        let session_id = self.active_session.id;
        let rename_request = session_title.map(|title| {
            backend.submit(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::RenameAgentSession {
                    session_id,
                    name: title,
                },
            )))
        });
        let run_request = backend.submit(RequestEnvelope::new(request));
        cx.spawn(async move |view, cx| {
            let response = cx
                .background_spawn(async move {
                    // The worker executes both requests in order; the rename is
                    // cosmetic, so its outcome does not gate the run.
                    if let Some(rename_request) = rename_request {
                        let _ = rename_request.wait().await;
                    }
                    run_request.wait().await
                })
                .await;
            view.update(cx, |view, cx| view.finish_send_response(response, cx))
                .ok();
        })
        .detach();
    }

    pub(crate) fn finish_send_response(
        &mut self,
        response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        self.sending_message = false;
        match response.result {
            Ok(ServerResponse::Run(RunResponse::AgentRunStarted(run)))
            | Ok(ServerResponse::Run(RunResponse::AgentRun(run))) => {
                self.session_task_cache
                    .insert(self.active_session.id, run.task.clone());
                self.active_run = Some(run);
                self.active_run_id = self.active_run.as_ref().map(|run| run.id);
                self.run_state = self.active_run.as_ref().map(|run| run.state);
                self.session_state = AgentSessionState::Executing;
                if let Some(run) = &self.active_run
                    && !self
                        .timeline
                        .iter()
                        .any(|item| matches!(item, TimelineItem::User(text) if text == &run.task))
                {
                    self.timeline
                        .insert(0, TimelineItem::User(run.task.clone()));
                }
                self.pending_input = None;
                self.start_run_polling(cx);
                if let Some(run) = &self.active_run
                    && !self
                        .timeline
                        .iter()
                        .any(|item| matches!(item, TimelineItem::User(text) if text == &run.task))
                {
                    self.timeline
                        .insert(0, TimelineItem::User(run.task.clone()));
                }
                self.refresh_review(cx);
            }
            Err(error) => self.record_backend_error("send message", error),
            Ok(response) => self.record_backend_error(
                "send message",
                unexpected_response("send message", response),
            ),
        }
        cx.notify();
    }

    pub(crate) fn approve_pending_action(&mut self, cx: &mut Context<Self>) {
        if self.approval_request_in_flight {
            return;
        }
        let (Some(run_id), Some(call)) = (self.active_run_id, self.pending_approval.clone()) else {
            return;
        };
        let Some(run) = self.active_run.as_ref().filter(|run| run.id == run_id) else {
            self.record_backend_error(
                "approve action",
                LoomError::invalid_state("active run control state is unavailable"),
            );
            return;
        };
        self.approval_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::Run(RunRequest::ApproveAgentAction {
                run_id,
                attempt_id: run.attempt_id,
                expected_control_revision: run.control_revision,
                tool_call_id: call.id,
            }),
            |view, response, cx| view.finish_approval_response(response, cx),
        );
    }

    pub(crate) fn reject_pending_action(&mut self, cx: &mut Context<Self>) {
        if self.approval_request_in_flight {
            return;
        }
        let (Some(run_id), Some(call)) = (self.active_run_id, self.pending_approval.clone()) else {
            return;
        };
        let Some(run) = self.active_run.as_ref().filter(|run| run.id == run_id) else {
            self.record_backend_error(
                "reject action",
                LoomError::invalid_state("active run control state is unavailable"),
            );
            return;
        };
        self.approval_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::Run(RunRequest::RejectAgentAction {
                run_id,
                attempt_id: run.attempt_id,
                expected_control_revision: run.control_revision,
                tool_call_id: call.id,
                reason: None,
            }),
            |view, response, cx| view.finish_approval_response(response, cx),
        );
    }

    pub(crate) fn finish_approval_response(
        &mut self,
        response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        match response.result {
            Ok(ServerResponse::Run(RunResponse::AgentRun(run)))
            | Ok(ServerResponse::Run(RunResponse::AgentRunStarted(run))) => {
                self.active_run = Some(run.clone());
                self.active_run_id = Some(run.id);
                self.run_state = Some(run.state);
                self.session_state = session_state_for_run(run.state);
                self.active_session.state = self.session_state;
                self.start_run_polling(cx);
            }
            Err(error) => {
                self.approval_request_in_flight = false;
                self.record_backend_error("approval", error);
            }
            Ok(response) => {
                self.approval_request_in_flight = false;
                self.record_backend_error("approval", unexpected_response("approval", response))
            }
        }
        cx.notify();
    }

    /// Whether the active run can still be interrupted.
    pub(crate) fn run_can_interrupt(&self) -> bool {
        self.active_run_id.is_some()
            && !matches!(
                self.run_state,
                Some(AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled)
            )
    }

    pub(crate) fn interrupt_active_run(&mut self, cx: &mut Context<Self>) {
        let Some(run_id) = self.active_run_id else {
            return;
        };
        self.dispatch(
            cx,
            ClientRequest::Run(RunRequest::InterruptAgentRun { run_id }),
            |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::Run(RunResponse::AgentRun(run)))
                    | Ok(ServerResponse::Run(RunResponse::AgentRunStarted(run))) => {
                        view.active_run_id = Some(run.id);
                        view.active_run = Some(run.clone());
                        view.run_state = Some(run.state);
                        view.session_state = session_state_for_run(run.state);
                        view.active_session.state = view.session_state;
                    }
                    Err(error) => view.record_backend_error("interrupt run", error),
                    Ok(response) => view.record_backend_error(
                        "interrupt run",
                        unexpected_response("interrupt run", response),
                    ),
                }
                cx.notify();
            },
        );
    }

    pub(crate) fn begin_transcript_page(
        &mut self,
        before_ordinal: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let Some(run_id) = self.active_run_id else {
            return;
        };
        if self.transcript_loading || (before_ordinal.is_some() && !self.transcript_has_older) {
            return;
        }
        let backend = match self.backend_for_session(self.active_session.id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("load conversation history", error);
                cx.notify();
                return;
            }
        };
        let transcript_generation = self.transcript_generation;
        self.transcript_loading = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = load_transcript_page(backend, run_id, before_ordinal).await;
            view.update(cx, |view, cx| {
                if view.active_run_id != Some(run_id)
                    || view.transcript_generation != transcript_generation
                {
                    return;
                }
                view.transcript_loading = false;
                match result {
                    Ok((messages, next_before, has_older)) => {
                        view.apply_transcript_page(
                            run_id,
                            before_ordinal,
                            messages,
                            next_before,
                            has_older,
                        );
                    }
                    Err(error) => view.record_backend_error("load conversation history", error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn apply_transcript_page(
        &mut self,
        run_id: RunId,
        before_ordinal: Option<u64>,
        messages: Vec<(u64, u64, ModelMessage)>,
        next_before: Option<u64>,
        has_older: bool,
    ) {
        if self.active_run_id != Some(run_id) {
            return;
        }
        if before_ordinal.is_none() {
            self.transcript_loaded_ordinals.clear();
            self.transcript_messages.clear();
        }
        let messages = unseen_transcript_messages(messages, &mut self.transcript_loaded_ordinals);
        for (ordinal, timeline_ordinal, message) in messages {
            self.transcript_messages
                .insert(ordinal, (timeline_ordinal, message));
        }
        self.timeline.retain(|item| {
            !matches!(
                item,
                TimelineItem::User(_)
                    | TimelineItem::Assistant(_)
                    | TimelineItem::ProjectMessageContext(_)
            )
        });
        let ordered_items = timeline_items_from_messages(
            self.transcript_messages
                .iter()
                .map(|(ordinal, (timeline_ordinal, message))| {
                    (*ordinal, *timeline_ordinal, message.clone())
                })
                .collect(),
            self.activity_records.values().cloned().collect(),
        );
        let insertion_index = self.transcript_insertion_index();
        self.timeline
            .splice(insertion_index..insertion_index, ordered_items);
        self.rebuild_project_message_timeline();
        self.transcript_before_ordinal = next_before;
        self.transcript_has_older = has_older;
        self.ensure_session_task_message(self.active_session.id);
    }

    pub(crate) fn transcript_insertion_index(&self) -> usize {
        let task = self.session_task_cache.get(&self.active_session.id);
        usize::from(
            matches!(self.timeline.first(), Some(TimelineItem::User(text)) if task == Some(text)),
        )
    }

    pub(crate) fn ensure_session_task_message(&mut self, session_id: AgentSessionId) {
        let Some(task) = self.session_task_cache.get(&session_id).cloned() else {
            return;
        };
        if !self
            .timeline
            .iter()
            .any(|item| matches!(item, TimelineItem::User(text) if text == &task))
        {
            self.timeline.insert(0, TimelineItem::User(task));
        }
    }
}
