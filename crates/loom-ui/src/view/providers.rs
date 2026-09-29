use super::*;

impl LoomView {
    pub(crate) fn toggle_github_login(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &self.github_login {
            if matches!(
                state,
                GitHubLoginState::Success | GitHubLoginState::Error(_)
            ) {
                self.github_login = None;
                cx.notify();
            }
            return;
        }
        let node_id = self
            .providers_node_id
            .as_ref()
            .unwrap_or(&self.default_backend_node_id);
        if !cfg!(target_family = "wasm")
            && !self
                .node_backends
                .get(node_id)
                .is_some_and(BackendWorker::secure_for_secrets)
        {
            self.github_login = Some(GitHubLoginState::Error(
                "GitHub Copilot sign-in requires a secure worker connection (wss:// or loopback ws://)."
                    .to_owned(),
            ));
            cx.notify();
            return;
        }
        self.settings_open = false;
        self.review.open = false;
        self.start_github_login(cx);
    }

    pub(crate) fn copy_github_login_value(
        &mut self,
        value: String,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        self.record_status(format!("Copied GitHub {label} to clipboard"));
        cx.notify();
    }

    pub(crate) fn start_github_login(&mut self, cx: &mut Context<Self>) {
        self.github_login = Some(GitHubLoginState::Starting);
        cx.notify();
        self.start_github_login_flow(cx);
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn start_github_login_flow(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async { GitHubCopilotAuthenticator::default().begin() });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| view.handle_github_device_code(result, cx))
                .ok();
        })
        .detach();
    }

    /// The worker performs the OAuth exchange and stores its credential;
    /// browser clients receive only the public device code and login status.
    #[cfg(target_family = "wasm")]
    pub(crate) fn start_github_login_flow(&mut self, cx: &mut Context<Self>) {
        let node_id = self
            .providers_node_id
            .clone()
            .unwrap_or_else(|| self.default_backend_node_id.clone());
        let Some(backend) = self.node_backends.get(&node_id).cloned() else {
            self.github_login = Some(GitHubLoginState::Error(
                "The selected worker is not connected.".to_owned(),
            ));
            cx.notify();
            return;
        };
        let pending = backend.submit(RequestEnvelope::new(ClientRequest::StartGitHubCopilotLogin));
        cx.spawn(async move |view, cx| {
            let response = pending.wait().await;
            let (login_id, user_code, verification_uri, expires_in, interval) =
                match response.result {
                    Ok(ServerResponse::GitHubCopilotLoginStarted {
                        login_id,
                        user_code,
                        verification_uri,
                        expires_in,
                        interval,
                    }) => (login_id, user_code, verification_uri, expires_in, interval),
                    Err(error) => {
                        view.update(cx, |view, cx| {
                            view.github_login = Some(GitHubLoginState::Error(error.message));
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                    Ok(response) => {
                        let error = unexpected_response("GitHub Copilot login start", response);
                        view.update(cx, |view, cx| {
                            view.github_login = Some(GitHubLoginState::Error(error.message));
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                };
            if view
                .update(cx, |view, cx| {
                    view.github_login = Some(GitHubLoginState::Awaiting {
                        verification_uri,
                        user_code,
                        expires_in,
                    });
                    cx.notify();
                })
                .is_err()
            {
                return;
            }

            let interval = Duration::from_secs(interval.clamp(1, 10));
            loop {
                if let Err(error) = browser_delay(interval).await {
                    view.update(cx, |view, cx| {
                        view.github_login = Some(GitHubLoginState::Error(error.message));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                let response = backend
                    .submit(RequestEnvelope::new(
                        ClientRequest::GetGitHubCopilotLoginStatus {
                            login_id: login_id.clone(),
                        },
                    ))
                    .wait()
                    .await;
                match response.result {
                    Ok(ServerResponse::GitHubCopilotLoginStatus {
                        status: GitHubCopilotLoginStatus::Pending,
                    }) => {}
                    Ok(ServerResponse::GitHubCopilotLoginStatus {
                        status: GitHubCopilotLoginStatus::Configured,
                    }) => {
                        view.update(cx, |view, cx| {
                            view.handle_github_provider_configured(node_id, cx);
                        })
                        .ok();
                        return;
                    }
                    Ok(ServerResponse::GitHubCopilotLoginStatus {
                        status: GitHubCopilotLoginStatus::Failed { message },
                    }) => {
                        view.update(cx, |view, cx| {
                            view.github_login = Some(GitHubLoginState::Error(message));
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                    Err(error) => {
                        view.update(cx, |view, cx| {
                            view.github_login = Some(GitHubLoginState::Error(error.message));
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                    Ok(response) => {
                        let error = unexpected_response("GitHub Copilot login status", response);
                        view.update(cx, |view, cx| {
                            view.github_login = Some(GitHubLoginState::Error(error.message));
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                }
            }
        })
        .detach();
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn handle_github_device_code(
        &mut self,
        result: Result<GitHubDeviceCode, LoomError>,
        cx: &mut Context<Self>,
    ) {
        let device = match result {
            Ok(device) => device,
            Err(error) => {
                self.github_login = Some(GitHubLoginState::Error(error.message));
                cx.notify();
                return;
            }
        };
        self.github_login = Some(GitHubLoginState::Awaiting {
            verification_uri: device.verification_uri.clone(),
            user_code: device.user_code.clone(),
            expires_in: device.expires_in,
        });
        cx.notify();
        let task =
            cx.background_spawn(async move { GitHubCopilotAuthenticator::default().poll(&device) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| view.finish_github_login(result, cx))
                .ok();
        })
        .detach();
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn finish_github_login(
        &mut self,
        result: Result<String, LoomError>,
        cx: &mut Context<Self>,
    ) {
        self.github_login = Some(GitHubLoginState::Completing);
        let token = match result {
            Ok(token) => token,
            Err(error) => {
                self.github_login = Some(GitHubLoginState::Error(error.message));
                cx.notify();
                return;
            }
        };
        let node_id = self
            .providers_node_id
            .clone()
            .unwrap_or_else(|| self.default_backend_node_id.clone());
        self.dispatch_to_node(
            cx,
            node_id.clone(),
            ClientRequest::ConfigureGitHubCopilot {
                access_token: token,
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::ProviderConfigured) => {
                    view.handle_github_provider_configured(node_id, cx)
                }
                Err(error) => {
                    view.github_login = Some(GitHubLoginState::Error(error.message));
                }
                Ok(response) => view.record_backend_error(
                    "configure GitHub Copilot",
                    unexpected_response("provider configuration", response),
                ),
            },
        );
        cx.notify();
    }

    pub(crate) fn handle_github_provider_configured(
        &mut self,
        node_id: String,
        cx: &mut Context<Self>,
    ) {
        self.github_login = Some(GitHubLoginState::Success);
        self.github_connected = true;
        let active_node_id = self
            .session_node_ids
            .get(&self.active_session.id)
            .map(String::as_str);
        let should_select_copilot = active_node_id == Some(node_id.as_str())
            && matches!(self.model.as_str(), "default" | "deterministic/demo");
        #[cfg(not(target_family = "wasm"))]
        if should_select_copilot {
            self.model = ModelId::new(GITHUB_COPILOT_DEFAULT_MODEL);
        }
        self.record_status(format!(
            "GitHub Copilot configured on {}",
            self.node_names
                .get(&node_id)
                .map_or(node_id.as_str(), String::as_str)
        ));
        self.dispatch_to_node(
            cx,
            node_id.clone(),
            ClientRequest::ListProviders,
            move |view, response, _| match response.result {
                Ok(ServerResponse::Providers { providers }) => {
                    if should_select_copilot
                        && view.model.as_str() == "deterministic/demo"
                        && let Some(model) = providers
                            .iter()
                            .find(|provider| provider.kind == ProviderKind::GitHubCopilot)
                            .and_then(|provider| provider.models.first())
                    {
                        view.model = model.id.clone();
                    }
                    view.github_connected = providers
                        .iter()
                        .any(|provider| provider.kind == ProviderKind::GitHubCopilot);
                    view.providers = providers;
                }
                Err(error) => view.record_backend_error("list providers", error),
                Ok(response) => view.record_backend_error(
                    "list providers",
                    unexpected_response("provider list", response),
                ),
            },
        );
        self.refresh_models_for_node_async(node_id, cx);
        cx.notify();
    }

    pub(crate) fn sync_model_select_states(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_node = self
            .session_node_ids
            .get(&self.active_session.id)
            .map(String::as_str)
            .unwrap_or(&self.default_backend_node_id);
        let active_provider_names = self.node_model_provider_names.get(active_node);
        let default_provider_names = self
            .node_model_provider_names
            .get(&self.default_backend_node_id);
        let model_choices = model_choice_labels(&self.models, active_provider_names);
        let default_model_choices =
            model_choice_labels(&self.default_models, default_provider_names);
        let items = model_choices.keys().cloned().collect::<Vec<_>>();
        let default_items = default_model_choices.keys().cloned().collect::<Vec<_>>();
        let model_value = model_choices
            .iter()
            .find(|(_, model)| *model == &self.model)
            .map(|(label, _)| label.clone())
            .unwrap_or_else(|| self.model.as_str().to_owned());
        let default_model_value = default_model_choices
            .iter()
            .find(|(_, model)| *model == &self.default_model)
            .map(|(label, _)| label.clone())
            .unwrap_or_else(|| self.default_model.as_str().to_owned());
        self.model_select_choices = model_choices;
        self.default_model_select_choices = default_model_choices;
        let model_needs_sync = self.model_select_items != items
            || self.model_select_value.as_deref() != Some(model_value.as_str());
        let default_model_needs_sync = self.default_model_select_items != default_items
            || self.default_model_select_value.as_deref() != Some(default_model_value.as_str());

        if let Some(state) = &self.model_select {
            if model_needs_sync {
                state.update(cx, |state, cx| {
                    state.set_items(SearchableVec::new(items.clone()), window, cx);
                    state.set_selected_value(&model_value, window, cx);
                });
            }
        } else {
            let selected_index = items
                .iter()
                .position(|item| item == &model_value)
                .map(|row| IndexPath::default().row(row));
            let state = cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(items.clone()),
                    selected_index,
                    window,
                    cx,
                )
                .searchable(true)
            });
            self.model_select_subscription = Some(cx.subscribe(
                &state,
                |view, _, event: &SelectEvent<SearchableVec<String>>, cx| {
                    if let SelectEvent::Confirm(Some(model)) = event
                        && let Some(model) = view.model_select_choices.get(model).cloned()
                    {
                        view.select_model(model, cx);
                    }
                },
            ));
            self.model_select = Some(state);
        }
        if model_needs_sync {
            self.model_select_items = items.clone();
            self.model_select_value = Some(model_value);
        }

        if let Some(state) = &self.default_model_select {
            if default_model_needs_sync {
                state.update(cx, |state, cx| {
                    state.set_items(SearchableVec::new(default_items.clone()), window, cx);
                    state.set_selected_value(&default_model_value, window, cx);
                });
            }
        } else {
            let selected_index = default_items
                .iter()
                .position(|item| item == &default_model_value)
                .map(|row| IndexPath::default().row(row));
            let state = cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(default_items.clone()),
                    selected_index,
                    window,
                    cx,
                )
                .searchable(true)
            });
            self.default_model_select_subscription = Some(cx.subscribe(
                &state,
                |view, _, event: &SelectEvent<SearchableVec<String>>, cx| {
                    if let SelectEvent::Confirm(Some(model)) = event
                        && let Some(model) = view.default_model_select_choices.get(model).cloned()
                    {
                        view.select_default_model(model, cx);
                    }
                },
            ));
            self.default_model_select = Some(state);
        }
        if default_model_needs_sync {
            self.default_model_select_items = default_items;
            self.default_model_select_value = Some(default_model_value);
        }
    }

    pub(crate) fn sync_agent_mode_select_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.agent_mode_select.is_some() {
            return;
        }
        let items = AgentMode::ALL
            .into_iter()
            .map(|mode| mode.label().to_owned())
            .collect::<Vec<_>>();
        let selected_index = items
            .iter()
            .position(|item| item == self.agent_mode.label())
            .map(|row| IndexPath::default().row(row));
        let state =
            cx.new(|cx| SelectState::new(SearchableVec::new(items), selected_index, window, cx));
        self.agent_mode_select_subscription = Some(cx.subscribe(
            &state,
            |view, _, event: &SelectEvent<SearchableVec<String>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = event
                    && let Some(mode) = AgentMode::ALL
                        .into_iter()
                        .find(|mode| mode.label() == label)
                {
                    view.select_agent_mode(mode, cx);
                }
            },
        ));
        self.agent_mode_select = Some(state);
    }

    pub(crate) fn select_model(&mut self, model: ModelId, cx: &mut Context<Self>) {
        let node_id = self
            .session_node_ids
            .get(&self.active_session.id)
            .map(String::as_str)
            .unwrap_or_default();
        let validation = if self.model_catalog_node_id.as_deref() != Some(node_id) {
            Err("worker models are still refreshing".to_owned())
        } else {
            validate_model_for_node(&self.node_model_catalogs, node_id, &model)
        };
        if let Err(reason) = validation {
            let node_name = self
                .node_names
                .get(node_id)
                .map(String::as_str)
                .unwrap_or(node_id);
            self.record_backend_error(
                "select model",
                LoomError::invalid_state(format!(
                    "Cannot select model '{}' on {node_name}: {reason}",
                    model.as_str()
                )),
            );
            cx.notify();
            return;
        }
        self.session_models
            .insert(self.active_session.id, model.clone());
        self.model = model;
        cx.notify();
    }

    pub(crate) fn select_agent_mode(&mut self, mode: AgentMode, cx: &mut Context<Self>) {
        if self.approval_settings_request_in_flight {
            return;
        }
        let session_id = self.active_session.id;
        let policy = mode.approval_policy(self.auto_approve_actions);
        self.approval_settings_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                auto_approve_actions: Some(self.auto_approve_actions),
            },
            move |view, response, _| {
                if view.active_session.id != session_id {
                    return;
                }
                view.approval_settings_request_in_flight = false;
                match response.result {
                    Ok(ServerResponse::ApprovalPolicy(_)) => {
                        view.agent_mode = mode;
                        view.session_auto_approve_actions
                            .insert(session_id, view.auto_approve_actions);
                        view.record_status(format!("{} mode enabled", mode.label()));
                    }
                    Err(error) => view.record_backend_error("set approval mode", error),
                    Ok(response) => view.record_backend_error(
                        "set approval mode",
                        unexpected_response("approval policy", response),
                    ),
                }
            },
        );
        cx.notify();
    }

    pub(crate) fn toggle_auto_approve_actions(&mut self, cx: &mut Context<Self>) {
        if self.approval_settings_request_in_flight || !self.is_connected() {
            return;
        }
        let session_id = self.active_session.id;
        let auto_approve_actions = !self.auto_approve_actions;
        let policy = self.agent_mode.approval_policy(auto_approve_actions);
        self.approval_settings_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                auto_approve_actions: Some(auto_approve_actions),
            },
            move |view, response, _| {
                if view.active_session.id != session_id {
                    return;
                }
                view.approval_settings_request_in_flight = false;
                match response.result {
                    Ok(ServerResponse::ApprovalPolicy(_)) => {
                        view.auto_approve_actions = auto_approve_actions;
                        view.session_auto_approve_actions
                            .insert(session_id, auto_approve_actions);
                        view.record_status(if auto_approve_actions {
                            "Automatic approvals enabled for this session".to_owned()
                        } else {
                            "Automatic approvals disabled for this session".to_owned()
                        });
                    }
                    Err(error) => {
                        view.record_backend_error("update session approval settings", error)
                    }
                    Ok(response) => view.record_backend_error(
                        "update session approval settings",
                        unexpected_response("approval policy", response),
                    ),
                }
            },
        );
        cx.notify();
    }

    pub(crate) fn select_default_model(&mut self, model: ModelId, cx: &mut Context<Self>) {
        if !self.default_models.contains(&model) {
            self.record_backend_error(
                "select default model",
                LoomError::invalid_state(format!(
                    "model '{}' is not available in the current worker model list",
                    model.as_str()
                )),
            );
            cx.notify();
            return;
        }
        self.default_model = model.clone();
        #[cfg(target_family = "wasm")]
        {
            self.browser_model = Some(model.clone());
            if let Err(error) = BrowserOptions::persist_default_model(&model) {
                self.record_backend_error("save default model", error);
            }
        }
        cx.notify();
    }

    pub(crate) fn open_settings_from_menu(&mut self, cx: &mut Context<Self>) {
        self.github_login = None;
        self.session_drawer_open = false;
        self.review.open = false;
        self.providers_open = false;
        self.about_open = false;
        self.settings_open = true;
        let node_id = self.default_backend_node_id.clone();
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::ListProviders,
            |view, response, _| match response.result {
                Ok(ServerResponse::Providers { providers }) => {
                    view.github_connected = providers
                        .iter()
                        .any(|provider| provider.kind == ProviderKind::GitHubCopilot);
                    view.providers = providers;
                }
                Err(error) => view.record_backend_error("check GitHub connection", error),
                Ok(response) => view.record_backend_error(
                    "check GitHub connection",
                    unexpected_response("provider list", response),
                ),
            },
        );
        cx.notify();
    }

    pub(crate) fn persist_and_distribute_workspace_config(
        &self,
        retiring: Option<(String, ClientConnection)>,
        cx: &mut Context<Self>,
    ) {
        let workspace_id = self.workspace_id;
        let workspace = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .cloned();
        let config = self.workspace_config.clone();
        let source = self.connection.clone();
        let peers = self
            .worker_nodes
            .iter()
            .filter(|node| !node.is_local)
            .filter_map(|node| {
                node.connection
                    .as_ref()
                    .map(|connection| (node.status.name.clone(), connection.clone()))
            })
            .collect::<Vec<_>>();
        #[cfg(not(target_family = "wasm"))]
        cx.spawn(async move |view, cx| {
            let errors = cx
                .background_spawn(async move {
                    let mut errors = Vec::new();
                    if let Err(error) = set_workspace_config(&source, workspace_id, config.clone())
                    {
                        errors.push(("save workspace config".to_owned(), error));
                    }
                    for (name, connection) in peers {
                        let registration = workspace
                            .clone()
                            .ok_or_else(|| LoomError::not_found("workspace", workspace_id));
                        if let Err(error) = registration
                            .and_then(|workspace| register_workspace(&connection, workspace))
                        {
                            errors.push((format!("register workspace on {name}"), error));
                            continue;
                        }
                        if let Err(error) =
                            set_workspace_config(&connection, workspace_id, config.clone())
                        {
                            errors.push((format!("distribute workspace config to {name}"), error));
                        }
                    }
                    if let Some((name, connection)) = retiring {
                        if let Some(workspace) = workspace.clone()
                            && let Err(error) = register_workspace(&connection, workspace)
                        {
                            errors.push((format!("register workspace on {name}"), error));
                        }
                        if let Err(error) =
                            set_workspace_config(&connection, workspace_id, config.clone())
                        {
                            errors.push((
                                format!("remove worker node {name} from its config"),
                                error,
                            ));
                        }
                        if let Err(error) = connection.close() {
                            errors.push((format!("close worker node {name} connection"), error));
                        }
                    }
                    errors
                })
                .await;
            view.update(cx, |view, cx| {
                for (context, error) in errors {
                    view.record_backend_error(&context, error);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        #[cfg(target_family = "wasm")]
        cx.spawn(async move |view, cx| {
            let mut errors = Vec::new();
            if let Err(error) =
                set_workspace_config_async(&source, workspace_id, config.clone()).await
            {
                errors.push(("save workspace config".to_owned(), error));
            } else {
                for (name, connection) in peers {
                    let registration = match workspace.clone() {
                        Some(workspace) => register_workspace_async(&connection, workspace).await,
                        None => Err(LoomError::not_found("workspace", workspace_id)),
                    };
                    if let Err(error) = registration {
                        errors.push((format!("register workspace on {name}"), error));
                        continue;
                    }
                    if let Err(error) =
                        set_workspace_config_async(&connection, workspace_id, config.clone()).await
                    {
                        errors.push((format!("distribute workspace config to {name}"), error));
                    }
                }
            }
            if let Some((name, connection)) = retiring {
                if let Some(workspace) = workspace
                    && let Err(error) = register_workspace_async(&connection, workspace).await
                {
                    errors.push((format!("register workspace on {name}"), error));
                }
                if let Err(error) =
                    set_workspace_config_async(&connection, workspace_id, config.clone()).await
                {
                    errors.push((format!("remove worker node {name} from its config"), error));
                }
                if let Err(error) = connection.close() {
                    errors.push((format!("close worker node {name} connection"), error));
                }
            }
            view.update(cx, |view, cx| {
                for (context, error) in errors {
                    view.record_backend_error(&context, error);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn open_about_from_menu(&mut self, cx: &mut Context<Self>) {
        self.github_login = None;
        self.session_drawer_open = false;
        self.review.open = false;
        self.settings_open = false;
        self.providers_open = false;
        self.about_open = true;
        cx.notify();
    }

    pub(crate) fn open_providers_from_menu(&mut self, cx: &mut Context<Self>) {
        self.open_providers_for_node(self.default_backend_node_id.clone(), cx);
    }

    pub(crate) fn open_providers_for_node(&mut self, node_id: String, cx: &mut Context<Self>) {
        self.github_login = None;
        self.session_drawer_open = false;
        self.review.open = false;
        self.settings_open = false;
        self.about_open = false;
        self.providers_open = true;
        self.providers_node_id = Some(node_id.clone());
        self.providers.clear();
        self.github_connected = false;
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::ListProviders,
            |view, response, _| match response.result {
                Ok(ServerResponse::Providers { providers }) => {
                    view.github_connected = providers
                        .iter()
                        .any(|provider| provider.kind == ProviderKind::GitHubCopilot);
                    view.providers = providers;
                }
                Err(error) => view.record_backend_error("list providers", error),
                Ok(response) => view.record_backend_error(
                    "list providers",
                    unexpected_response("provider list", response),
                ),
            },
        );
        cx.notify();
    }

    pub(crate) fn configure_api_key_provider(
        &mut self,
        provider_id: loom_model::ProviderId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(node_id) = self.providers_node_id.clone() else {
            self.provider_setup_status.insert(
                provider_id,
                "Choose a worker before saving an API key".to_owned(),
            );
            cx.notify();
            return;
        };
        let api_key = self
            .provider_api_key_inputs
            .get(&provider_id)
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        if api_key.trim().is_empty() {
            self.provider_setup_status
                .insert(provider_id, "Enter an API key before saving".to_owned());
            cx.notify();
            return;
        }
        if !self
            .node_backends
            .get(&node_id)
            .is_some_and(BackendWorker::secure_for_secrets)
        {
            self.provider_setup_status.insert(
                provider_id,
                "This worker needs a secure connection to save API keys (wss:// or loopback ws://)"
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        if let Some(input) = self.provider_api_key_inputs.get(&provider_id) {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.provider_setup_status.insert(
            provider_id.clone(),
            format!(
                "Saving API key on {}…",
                self.node_names
                    .get(&node_id)
                    .map(String::as_str)
                    .unwrap_or("worker")
            ),
        );
        cx.notify();
        self.dispatch_to_node(
            cx,
            node_id.clone(),
            ClientRequest::ConfigureApiKeyProvider {
                provider_id: provider_id.clone(),
                api_key,
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::ProviderConfigured) => {
                    view.provider_setup_status.insert(
                        provider_id.clone(),
                        format!(
                            "API key saved on {}",
                            view.node_names
                                .get(&node_id)
                                .map(String::as_str)
                                .unwrap_or("worker")
                        ),
                    );
                    view.record_status(format!("Provider configured on {node_id}"));
                    view.open_providers_for_node(node_id, cx);
                    view.refresh_models_for_node_async(
                        view.providers_node_id.clone().unwrap_or_default(),
                        cx,
                    );
                }
                Err(error) => {
                    view.provider_setup_status.insert(
                        provider_id.clone(),
                        format!("Could not save API key: {}", error.message),
                    );
                    view.record_backend_error("configure provider", error);
                }
                Ok(response) => {
                    let error = unexpected_response("provider configuration", response);
                    view.provider_setup_status.insert(
                        provider_id.clone(),
                        format!("Could not save API key: {}", error.message),
                    );
                    view.record_backend_error("configure provider", error);
                }
            },
        );
    }

    pub(crate) fn close_providers(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.providers_open = false;
        for input in self.provider_api_key_inputs.values() {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.provider_api_key_inputs.clear();
        self.provider_setup_status.clear();
        cx.notify();
    }

    pub(crate) fn close_settings(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_open = false;
        cx.notify();
    }

    pub(crate) fn close_about(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.about_open = false;
        cx.notify();
    }

    pub(crate) fn select_theme(
        &mut self,
        theme: ThemeChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.theme_choice = theme;
        let appearance = match theme {
            ThemeChoice::System => {
                cx.set_window_appearance(None);
                window.appearance()
            }
            ThemeChoice::Light => {
                cx.set_window_appearance(Some(WindowAppearance::Light));
                WindowAppearance::Light
            }
            ThemeChoice::Dark => {
                cx.set_window_appearance(Some(WindowAppearance::Dark));
                WindowAppearance::Dark
            }
        };
        self.apply_appearance(appearance, window, cx);
    }

    pub(crate) fn observe_system_appearance(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.appearance_subscription.is_none() {
            self.appearance_subscription =
                Some(cx.observe_window_appearance(window, |view, window, cx| {
                    if view.theme_choice == ThemeChoice::System {
                        view.apply_appearance(window.appearance(), window, cx);
                    }
                }));
        }
    }

    pub(crate) fn apply_appearance(
        &mut self,
        appearance: WindowAppearance,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::theme::apply_theme(appearance, cx);
        cx.notify();
    }
}
