use super::*;

impl LoomView {
    /// Loads the session list synchronously for the startup
    /// bootstrap. Interactive refreshes use [`Self::reload_sessions`].
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn refresh_sessions(&mut self) -> Result<(), LoomError> {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
                    workspace_id: self.workspace_id,
                    include_archived: false,
                }));
        match response.result? {
            ServerResponse::AgentSessions { sessions } => {
                for session in &sessions {
                    self.session_node_ids
                        .insert(session.id, self.default_backend_node_id.clone());
                }
                self.sessions = sessions;
                if let Some(active) = self
                    .sessions
                    .iter()
                    .find(|session| session.id == self.active_session.id)
                {
                    self.active_session = active.clone();
                    self.session_state = active.state;
                }
            }
            response => return Err(unexpected_response("session list", response)),
        }

        Ok(())
    }

    /// Reloads sessions from every connected node while keeping their owners.
    pub(crate) fn reload_sessions(&mut self, cx: &mut Context<Self>) {
        let workspace_id = self.workspace_id;
        let mut node_requests = BTreeMap::new();
        for node in self
            .worker_nodes
            .iter()
            .filter(|node| node.connection.is_some() && node.status.online)
        {
            if let Some(backend) = self.node_backends.get(&node.status.node_id) {
                node_requests
                    .entry(node.status.node_id.clone())
                    .or_insert_with(|| {
                        backend.submit(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
                            workspace_id,
                            include_archived: false,
                        }))
                    });
            }
        }
        cx.spawn(async move |view, cx| {
            let node_responses = cx
                .background_spawn(async move {
                    let mut node_responses = Vec::with_capacity(node_requests.len());
                    for (node_id, pending) in node_requests {
                        node_responses.push((node_id, pending.wait().await));
                    }
                    node_responses
                })
                .await;
            view.update(cx, |view, cx| {
                let previous_active_node_id =
                    view.session_node_ids.get(&view.active_session.id).cloned();
                let node_results = node_responses
                    .into_iter()
                    .filter_map(|(node_id, response)| match response.result {
                        Ok(ServerResponse::AgentSessions { sessions }) => Some((node_id, sessions)),
                        Err(error) => {
                            view.record_backend_error("session list refresh", error);
                            None
                        }
                        Ok(response) => {
                            view.record_backend_error(
                                "session list refresh",
                                unexpected_response("session list", response),
                            );
                            None
                        }
                    })
                    .collect();
                (view.sessions, view.session_node_ids) =
                    merge_node_sessions(&view.sessions, &view.session_node_ids, node_results);
                let active_node_id = view.session_node_ids.get(&view.active_session.id).cloned();
                if active_node_id != previous_active_node_id
                    && let Some(node_id) = active_node_id
                {
                    view.refresh_models_for_node_async(node_id, cx);
                }
                if let Some(active) = view
                    .sessions
                    .iter()
                    .find(|session| session.id == view.active_session.id)
                {
                    view.active_session = active.clone();
                    view.session_state = active.state;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn reset_projection(&mut self) {
        self.timeline.clear();
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
        self.transcript_before_ordinal = None;
        self.transcript_loaded_ordinals.clear();
        self.transcript_messages.clear();
        self.transcript_has_older = false;
        self.transcript_loading = false;
        self.activity_records.clear();
        self.expanded_tools.clear();
        self.expanded_reasoning.clear();
        self.approval_request_in_flight = false;
        self.pending_approval = None;
        self.pending_input = None;
        self.active_run = None;
        self.active_run_id = None;
        self.context_inspection = None;
        self.run_state = None;
        self.after_sequence = None;
        self.rebuild_project_message_timeline();
    }

    pub(crate) fn update_session_list(&mut self) {
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == self.active_session.id)
        {
            *session = self.active_session.clone();
        }
    }
}
