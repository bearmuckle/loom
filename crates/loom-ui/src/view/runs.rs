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
        if self.active_run_id.is_none() {
            // Remember the task for transcript ordering and the Agent card, but
            // never rename the session from its first prompt. A project keeps
            // the name it was created with (its folder or repository), and a
            // session is renamed only through the explicit rename action.
            self.session_task_cache
                .insert(self.active_session.id, message.clone());
        }
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
        let run_request = backend.submit(RequestEnvelope::new(request));
        cx.spawn(async move |view, cx| {
            let response = cx
                .background_spawn(async move { run_request.wait().await })
                .await;
            view.update(cx, |view, cx| view.finish_send_response(response, cx))
                .ok();
        })
        .detach();
    }

    /// Answers the question the active run is waiting on with the text a choice
    /// stands for.
    ///
    /// The answer travels the same route as a typed one, so permissions, the
    /// run revision, and a conflicting direction are handled once: a choice is
    /// a shortcut for writing the answer, not a second way to send it. The
    /// guards keep a stale card, a double click, or a read-only connection from
    /// sending anything.
    pub(crate) fn answer_question(&mut self, answer: String, cx: &mut Context<Self>) {
        if self.pending_input.is_none() || self.sending_message {
            return;
        }
        if !self.session_permissions().can_act {
            return;
        }
        self.send_composer_text(answer, cx);
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
        if !self.session_permissions().can_act {
            return;
        }
        if self.approval_request_in_flight {
            return;
        }
        let (Some(run_id), Some(call)) = (self.active_run_id, self.pending_approval.clone()) else {
            return;
        };
        if self
            .active_run
            .as_ref()
            .filter(|run| run.id == run_id)
            .is_none()
        {
            self.record_backend_error(
                "approve action",
                LoomError::invalid_state("active run control state is unavailable"),
            );
            return;
        }
        self.approval_request_in_flight = true;
        self.control_run(
            loom_core::RunControlAction::Approve {
                tool_call_id: call.id,
            },
            cx,
        );
    }

    pub(crate) fn reject_pending_action(&mut self, cx: &mut Context<Self>) {
        if !self.session_permissions().can_act {
            return;
        }
        if self.approval_request_in_flight {
            return;
        }
        let (Some(run_id), Some(call)) = (self.active_run_id, self.pending_approval.clone()) else {
            return;
        };
        if self
            .active_run
            .as_ref()
            .filter(|run| run.id == run_id)
            .is_none()
        {
            self.record_backend_error(
                "reject action",
                LoomError::invalid_state("active run control state is unavailable"),
            );
            return;
        }
        self.approval_request_in_flight = true;
        self.control_run(
            loom_core::RunControlAction::Reject {
                tool_call_id: call.id,
                reason: None,
            },
            cx,
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

    /// Whether the active run can be paused: live and not already paused.
    pub(crate) fn run_can_pause(&self) -> bool {
        self.run_can_interrupt() && self.run_state != Some(AgentRunState::Paused)
    }

    /// Whether the active run is paused and can be resumed.
    pub(crate) fn run_can_resume(&self) -> bool {
        self.active_run_id.is_some() && self.run_state == Some(AgentRunState::Paused)
    }

    /// Whether the active run failed; the server retries a failed tool step.
    pub(crate) fn run_can_retry(&self) -> bool {
        self.active_run_id.is_some() && self.run_state == Some(AgentRunState::Failed)
    }

    pub(crate) fn interrupt_active_run(&mut self, cx: &mut Context<Self>) {
        if !self.session_permissions().can_act {
            return;
        }
        if self.active_run_id.is_none() {
            return;
        }
        self.control_run(loom_core::RunControlAction::Interrupt, cx);
    }

    /// Applies the response to a legacy interrupt, pause, resume, or retry.
    pub(crate) fn finish_legacy_control_response(
        &mut self,
        operation: &'static str,
        response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        match response.result {
            Ok(ServerResponse::Run(RunResponse::AgentRun(run)))
            | Ok(ServerResponse::Run(RunResponse::AgentRunStarted(run))) => {
                self.active_run_id = Some(run.id);
                self.active_run = Some(run.clone());
                self.run_state = Some(run.state);
                self.session_state = session_state_for_run(run.state);
                self.active_session.state = self.session_state;
            }
            Err(error) => self.record_backend_error(operation, error),
            Ok(response) => {
                self.record_backend_error(operation, unexpected_response(operation, response))
            }
        }
        cx.notify();
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
                self.finish_browser_startup();
                self.record_backend_error("load conversation history", error);
                cx.notify();
                return;
            }
        };
        let transcript_generation = self.transcript_generation;
        // A reopen's first page is the newest window, which a tool-heavy run
        // fills with tool traffic and no user turn, so that call keeps following
        // the older pages the backfill names. A scroll-triggered older page stays
        // a single request.
        let mut backfill = before_ordinal.is_none().then(ReopenTranscriptBackfill::new);
        // The event cursor as the backfill starts. A poll dispatched before it
        // can still land between two pages and apply timeline items the
        // rebuild would not know about.
        let cursor_before = self.after_sequence;
        let activity_detail = self.output_detail.activity_detail(self.show_reasoning);
        self.transcript_loading = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let mut before_ordinal = before_ordinal;
            loop {
                let result =
                    load_transcript_page(backend.clone(), run_id, before_ordinal, activity_detail)
                        .await;
                let next = view.update(cx, |view, cx| {
                    if view.active_run_id != Some(run_id)
                        || view.transcript_generation != transcript_generation
                    {
                        // A newer load owns transcript_loading.
                        return None;
                    }
                    match result {
                        Ok((messages, next_before, has_older)) => {
                            let next = backfill.as_mut().and_then(|backfill| {
                                backfill.record_page(
                                    before_ordinal,
                                    &messages,
                                    next_before,
                                    has_older,
                                )
                            });
                            view.apply_transcript_page(
                                run_id,
                                before_ordinal,
                                messages,
                                next_before,
                                has_older,
                            );
                            // The first page puts the conversation on screen, so
                            // a browser startup that waited for it can reveal
                            // the workspace.
                            view.finish_browser_startup();
                            if next.is_none() {
                                if backfill.is_some() {
                                    let cursor_after = view.after_sequence;
                                    if transcript_rebuild_after_backfill(
                                        cursor_before,
                                        cursor_after,
                                    ) {
                                        // The backfill is done, so rebuild once
                                        // from every loaded message and
                                        // activity row to put them in one
                                        // chronological order.
                                        view.rebuild_transcript_from_loaded_messages();
                                    }
                                    // An event landed mid-backfill, so keep the
                                    // per-page prepend result: the rebuild would
                                    // drop the item that event applied, which is
                                    // worse than the order the prepend leaves.
                                    // The next reopen reorders.
                                }
                                view.transcript_loading = false;
                            }
                            cx.notify();
                            next
                        }
                        Err(error) => {
                            view.finish_browser_startup();
                            view.record_backend_error("load conversation history", error);
                            view.transcript_loading = false;
                            cx.notify();
                            None
                        }
                    }
                });
                let Ok(Some(next)) = next else {
                    break;
                };
                before_ordinal = Some(next);
            }
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
            // The first page is authoritative for the messages it carries, but
            // not for newer durable messages that are already loaded: a run
            // projection read before the page can hold messages the durable page
            // read has not flushed yet, and the event cursor has already passed
            // them, so dropping them here would lose them for good. Keep the
            // ordinals the page does not cover and rebuild from the union.
            let page_newest = messages.iter().map(|(ordinal, _, _)| *ordinal).max();
            self.transcript_loaded_ordinals
                .retain(|ordinal| page_newest.is_none_or(|newest| *ordinal > newest));
            self.transcript_messages
                .retain(|ordinal, _| page_newest.is_none_or(|newest| *ordinal > newest));
        }
        let messages = unseen_transcript_messages(messages, &mut self.transcript_loaded_ordinals);
        for (ordinal, timeline_ordinal, message) in &messages {
            self.transcript_messages
                .insert(*ordinal, (*timeline_ordinal, message.clone()));
        }
        let before_len = self.timeline.len();
        if before_ordinal.is_none() {
            // The first page replaces the message portion of the transcript with
            // the messages loaded from it (and any newer durable message it does
            // not cover yet), so rebuild that portion.
            self.rebuild_transcript_from_loaded_messages();
        } else {
            // Older pages only add content ahead of what is loaded. Build just
            // those rows and prepend them so turns streamed since the request
            // (which are not in the page) are not discarded.
            let mut ordered_items = timeline_items_from_messages(messages, Vec::new());
            // The oldest page carries the run's original user message, whose
            // text is the run's task, and the task row is already pinned above
            // the transcript. Adding it again would show the question twice.
            let task = self.session_task_cache.get(&self.active_session.id);
            if let Some(task) = task
                && matches!(self.timeline.first(), Some(TimelineItem::User(text)) if text == task)
            {
                ordered_items
                    .retain(|item| !matches!(item, TimelineItem::User(text) if text == task));
            }
            let insertion_index = self.transcript_insertion_index();
            self.timeline
                .splice(insertion_index..insertion_index, ordered_items);
            self.transcript_prepend_count = self
                .transcript_prepend_count
                .saturating_add(self.timeline.len().saturating_sub(before_len));
        }
        self.transcript_before_ordinal = next_before;
        self.transcript_has_older = has_older;
        self.ensure_session_task_message(self.active_session.id);
    }

    /// Rebuilds the message portion of the timeline from every transcript
    /// message and activity loaded so far.
    ///
    /// A backfill applies its pages one at a time after the first page already
    /// rebuilt the timeline, so an activity-derived tool row would otherwise
    /// stay where that rebuild placed it rather than between the messages whose
    /// timeline ordinals surround it. Rebuilding from the whole loaded set once
    /// the backfill ends puts every row in one chronological order.
    pub(crate) fn rebuild_transcript_from_loaded_messages(&mut self) {
        // Dropping the message portion can empty the timeline, so compute the
        // insertion index after the retain: an index derived from the pre-retain
        // first row would be out of bounds for the splice below.
        self.timeline
            .retain(|item| !matches!(item, TimelineItem::User(_) | TimelineItem::Assistant(_)));
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
        self.ensure_session_task_message(self.active_session.id);
    }

    /// Automatically request the next older transcript page when the reader has
    /// scrolled up to the oldest loaded row.
    pub(crate) fn autoload_older_transcript(&mut self, cx: &mut Context<Self>) {
        if !self.transcript_has_older || self.transcript_loading {
            return;
        }
        self.begin_transcript_page(self.transcript_before_ordinal, cx);
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
