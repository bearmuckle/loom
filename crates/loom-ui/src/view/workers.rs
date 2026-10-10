use super::*;

impl LoomView {
    pub(crate) fn add_worker_node(
        &mut self,
        connection: ClientConnection,
        status: WorkerNodeStatus,
        url: String,
        connection_detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let node_id = status.node_id.clone();
        let backend = BackendWorker::spawn(connection.clone());
        if let Some(previous_node_id) = self
            .worker_nodes
            .iter()
            .find(|node| !node.is_local && node.url.as_deref() == Some(&url))
            .map(|node| node.status.node_id.clone())
            && previous_node_id != node_id
        {
            self.node_backends.remove(&previous_node_id);
        }
        if !self
            .workspace_config
            .worker_nodes
            .iter()
            .any(|node| node.url == url)
        {
            self.workspace_config
                .worker_nodes
                .push(WorkerNodeConfig { url: url.clone() });
            self.workspace_config.revision = self.workspace_config.revision.saturating_add(1);
        }
        if let Some(node) = self
            .worker_nodes
            .iter_mut()
            .find(|node| !node.is_local && node.url.as_deref() == Some(&url))
        {
            node.status = status;
            node.connection = Some(connection);
            node.connection_state = WorkerConnectionState::Connected;
            node.connection_detail = connection_detail;
        } else {
            let id = self.next_worker_node_id;
            self.next_worker_node_id += 1;
            self.worker_nodes.push(WorkerNodeEntry {
                id,
                status,
                is_local: false,
                url: Some(url),
                connection: Some(connection),
                connection_state: WorkerConnectionState::Connected,
                connection_detail,
                severe_load_streak: 0,
            });
        }
        self.node_backends.insert(node_id.clone(), backend);
        if let Some(node) = self
            .worker_nodes
            .iter()
            .find(|node| node.status.node_id == node_id)
        {
            self.node_names
                .insert(node_id, worker_node_display_name(node));
        }
        self.record_status("Connected to worker node");
        self.schedule_worker_node_poll(cx);
        self.persist_and_distribute_workspace_config(None, cx);
        self.reload_sessions(cx);
    }

    pub(crate) fn begin_worker_node_connection(&mut self, url: &str) -> Result<u64, String> {
        if let Some(node) = self
            .worker_nodes
            .iter_mut()
            .find(|node| !node.is_local && node.url.as_deref() == Some(url))
        {
            if let Err(message) = transition_worker_connection_to_connecting(
                &mut node.connection_state,
                node.connection.is_some(),
            ) {
                return Err(message.to_owned());
            }
            node.connection_detail = None;
            node.status.online = false;
            return Ok(node.id);
        }

        let id = self.next_worker_node_id;
        self.next_worker_node_id = self.next_worker_node_id.saturating_add(1);
        self.worker_nodes.push(connection_placeholder(
            id,
            url.to_owned(),
            WorkerConnectionState::Connecting,
            None,
        ));
        Ok(id)
    }

    pub(crate) fn fail_worker_node_connection(
        &mut self,
        id: u64,
        url: &str,
        stage: WorkerConnectionStage,
        error: &LoomError,
        secret: Option<&str>,
        cleanup_failed: bool,
    ) {
        let Some(node) = self
            .worker_nodes
            .iter_mut()
            .find(|node| node.id == id && !node.is_local && node.url.as_deref() == Some(url))
        else {
            return;
        };
        let mut detail = worker_connection_failure_detail(stage, error, secret);
        let node_cleanup_failed = mark_worker_connection_failed(node, String::new());
        if cleanup_failed || node_cleanup_failed {
            detail.push_str(
                " Closing the partial connection also failed; restart the worker and retry.",
            );
        }
        node.connection_detail = Some(detail);
    }

    pub(crate) fn set_worker_node_connection_failure(
        &mut self,
        id: u64,
        url: &str,
        detail: String,
    ) {
        if let Some(node) = self
            .worker_nodes
            .iter_mut()
            .find(|node| node.id == id && !node.is_local && node.url.as_deref() == Some(url))
        {
            let _ = mark_worker_connection_failed(node, detail);
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn attach_reconnected_worker_node(
        &mut self,
        id: u64,
        url: &str,
        connection: ClientConnection,
        status: WorkerNodeStatus,
        cx: &mut Context<Self>,
    ) -> bool {
        let node_id = status.node_id.clone();
        let backend = BackendWorker::spawn(connection.clone());
        let Some(node) = self.worker_nodes.iter_mut().find(|node| {
            node.id == id
                && !node.is_local
                && node.url.as_deref() == Some(url)
                && node.connection.is_none()
        }) else {
            return false;
        };
        let name = status.name.clone();
        node.status = status;
        node.connection = Some(connection);
        node.connection_state = WorkerConnectionState::Connected;
        node.connection_detail = None;
        let display_name = worker_node_display_name(node);
        self.node_backends.insert(node_id, backend);
        self.node_names
            .insert(node.status.node_id.clone(), display_name);
        self.record_status(format!("Reconnected to worker node {name}"));
        self.schedule_worker_node_poll(cx);
        self.reload_sessions(cx);
        cx.notify();
        true
    }

    pub(crate) fn remove_worker_node(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(node) = remove_worker_node_entry(&mut self.worker_nodes, id) else {
            return;
        };
        self.worker_node_polls_scheduled.remove(&id);
        if !self
            .worker_nodes
            .iter()
            .any(|remaining| remaining.status.node_id == node.status.node_id)
        {
            self.node_backends.remove(&node.status.node_id);
            self.node_workspaces.remove(&node.status.node_id);
            self.node_session_load_errors.remove(&node.status.node_id);
        }
        if let Some(url) = &node.url {
            let count = self.workspace_config.worker_nodes.len();
            self.workspace_config
                .worker_nodes
                .retain(|configured| configured.url != *url);
            if self.workspace_config.worker_nodes.len() != count {
                self.workspace_config.revision = self.workspace_config.revision.saturating_add(1);
            }
        }
        self.record_status(format!("Removed worker node {}", node.status.name));
        cx.notify();

        let retiring = node
            .connection
            .map(|connection| (node.status.name, connection));
        self.persist_and_distribute_workspace_config(retiring, cx);
        self.reload_sessions(cx);
        #[cfg(not(target_family = "wasm"))]
        if let Some(url) = node.url {
            let workspace_id = self.workspace_id;
            cx.spawn(async move |view, cx| {
                let result = cx
                    .background_spawn(async move {
                        PeerCredentialStore::new().delete(workspace_id, &url)
                    })
                    .await;
                view.update(cx, |view, cx| {
                    if let Err(error) = result {
                        view.record_backend_error("remove worker-node credential", error);
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    pub(crate) fn adjust_cpu_pulse_threshold(&mut self, delta: i8, cx: &mut Context<Self>) {
        let next =
            adjusted_cpu_pulse_threshold(self.workspace_config.cpu_pulse_threshold_percent, delta);
        if next == self.workspace_config.cpu_pulse_threshold_percent {
            return;
        }
        self.workspace_config.cpu_pulse_threshold_percent = next;
        self.workspace_config.revision = self.workspace_config.revision.saturating_add(1);
        self.persist_and_distribute_workspace_config(None, cx);
        cx.notify();
    }

    pub(crate) fn adjust_project_agent_concurrency(&mut self, delta: i8, cx: &mut Context<Self>) {
        let next = adjusted_project_agent_concurrency(
            self.workspace_config.project_agent_concurrency,
            delta,
        );
        if next == self.workspace_config.project_agent_concurrency {
            return;
        }
        self.workspace_config.project_agent_concurrency = next;
        self.workspace_config.revision = self.workspace_config.revision.saturating_add(1);
        self.persist_and_distribute_workspace_config(None, cx);
        cx.notify();
    }

    pub(crate) fn set_font_scale_percent(
        &mut self,
        font_scale_percent: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let font_scale_percent =
            font_scale_percent.clamp(MIN_FONT_SCALE_PERCENT, MAX_FONT_SCALE_PERCENT);
        if self.font_scale_percent == font_scale_percent {
            return;
        }
        self.font_scale_percent = font_scale_percent;
        window.set_rem_size(px(
            BASE_FONT_SIZE * font_scale_percent as f32 / DEFAULT_FONT_SCALE_PERCENT as f32
        ));
        cx.notify();
    }

    pub(crate) fn adjust_font_scale(
        &mut self,
        delta: i16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_font_scale_percent(
            (self.font_scale_percent as i16 + delta)
                .clamp(MIN_FONT_SCALE_PERCENT as i16, MAX_FONT_SCALE_PERCENT as i16)
                as u16,
            window,
            cx,
        );
    }
}
