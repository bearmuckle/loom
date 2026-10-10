use super::*;

impl LoomView {
    /// Loads the session list synchronously for the startup
    /// bootstrap. Interactive refreshes use [`Self::reload_sessions`].
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn refresh_sessions(&mut self) -> Result<(), LoomError> {
        let response = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id: self.workspace_id,
                    include_archived: false,
                },
            )));
        match response.result? {
            ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => {
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

    /// Loads the project snapshot for every session not already covered by a
    /// known project, synchronously for the startup bootstrap.
    ///
    /// The active session's project is loaded by `load_session`; without this,
    /// every other project's delegated sub-tasks have no parent record at first
    /// paint and render as top-level sessions until their project is visited.
    /// One request is issued per project: a fetched snapshot covers all of its
    /// agent sessions.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn refresh_project_snapshots(&mut self) {
        let mut attempted = BTreeSet::new();
        while let Some(session_id) =
            uncovered_session_ids(&self.sessions, &self.project_tree_snapshots)
                .into_iter()
                .find(|session_id| !attempted.contains(session_id))
        {
            attempted.insert(session_id);
            let response = self
                .connection
                .request(RequestEnvelope::new(ClientRequest::Project(
                    ProjectRequest::GetProjectSnapshotForSession { session_id },
                )));
            let Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot))) =
                response.result
            else {
                continue;
            };
            self.project_tree_snapshots
                .retain(|known| known.project_id != snapshot.project_id);
            self.project_tree_snapshots.push(snapshot);
        }
    }

    /// Reloads sessions from every connected node while keeping their owners.
    ///
    /// Each worker is an independent backend with its own workspaces. Listing
    /// only the home workspace id would hide a worker's pre-existing projects,
    /// so every workspace on the node is listed and its sessions merged under
    /// that node's owner.
    pub(crate) fn reload_sessions(&mut self, cx: &mut Context<Self>) {
        let workspace_id = self.workspace_id;
        let node_backends = self
            .worker_nodes
            .iter()
            .filter(|node| node.connection.is_some() && node.status.online)
            .filter_map(|node| {
                self.node_backends
                    .get(&node.status.node_id)
                    .cloned()
                    .map(|backend| (node.status.node_id.clone(), backend))
            })
            .collect::<Vec<_>>();
        cx.spawn(async move |view, cx| {
            let mut node_responses = Vec::with_capacity(node_backends.len());
            for (node_id, backend) in node_backends {
                let sessions = list_node_sessions(backend, workspace_id).await;
                node_responses.push((node_id, sessions));
            }
            view.update(cx, |view, cx| {
                let previous_active_node_id =
                    view.session_node_ids.get(&view.active_session.id).cloned();
                let mut node_results = Vec::with_capacity(node_responses.len());
                for (node_id, result) in node_responses {
                    match result {
                        Ok(sessions) => {
                            view.node_session_load_errors.remove(&node_id);
                            node_results.push((node_id, sessions));
                        }
                        Err(error) => {
                            view.node_session_load_errors
                                .insert(node_id, error.message.clone());
                            view.record_backend_error("session list refresh", error);
                        }
                    }
                }
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
                view.refresh_missing_project_snapshots(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn reset_projection(&mut self) {
        self.timeline.clear();
        // The optimistic entries stood for timeline items that this reset just
        // removed, so keeping one would silently suppress the next matching
        // `UserMessage` event instead of showing that message.
        self.optimistic_messages.clear();
        self.plan = None;
        self.plan_collapsed = false;
        // A different session's review content must not count as already
        // viewed, so the Changes marker and its recorded snapshot both go.
        self.review.reset_changes_unread();
        self.review.clear_unread(InspectorTab::Plan);
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
        self.transcript_before_ordinal = None;
        self.transcript_loaded_ordinals.clear();
        self.transcript_messages.clear();
        self.transcript_has_older = false;
        self.transcript_loading = false;
        self.transcript_prepend_count = 0;
        self.activity_records.clear();
        self.expanded_tools.clear();
        self.expanded_tool_usage.clear();
        self.expanded_reasoning.clear();
        self.approval_request_in_flight = false;
        self.pending_approval = None;
        self.pending_input = None;
        self.active_run = None;
        self.active_run_id = None;
        self.context_inspection = None;
        self.run_state = None;
        self.after_sequence = None;
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

/// Lists every session a worker can see by walking each of its workspaces.
///
/// A worker that has been used standalone keeps its own workspace, so querying
/// the client's home workspace id would return nothing. If a worker reports no
/// workspaces yet (for example before the home workspace has been registered on
/// it), the fallback workspace id is queried so a shared workspace still loads.
async fn list_node_sessions(
    backend: BackendWorker,
    fallback_workspace: WorkspaceId,
) -> Result<Vec<AgentSessionSnapshot>, LoomError> {
    let workspace_list = backend
        .submit(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaces,
        )))
        .wait()
        .await;
    let workspaces = match workspace_list.result? {
        ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces }) => workspaces,
        response => return Err(unexpected_response("workspace list", response)),
    };
    let workspace_ids = if workspaces.is_empty() {
        vec![fallback_workspace]
    } else {
        workspaces
            .into_iter()
            .map(|workspace| workspace.id)
            .collect()
    };
    let mut sessions = Vec::new();
    for workspace_id in workspace_ids {
        let response = backend
            .submit(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id,
                    include_archived: false,
                },
            )))
            .wait()
            .await;
        match response.result? {
            ServerResponse::Session(SessionResponse::AgentSessions {
                sessions: mut found,
            }) => {
                sessions.append(&mut found);
            }
            response => return Err(unexpected_response("session list refresh", response)),
        }
    }
    Ok(sessions)
}
