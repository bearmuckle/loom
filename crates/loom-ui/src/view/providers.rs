use super::*;
use loom_core::{GitHubSignInStatus, ParticipantIdentity};
use loom_protocol::{AccessRequest, AccessResponse};

impl LoomView {
    pub(crate) fn toggle_github_login(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_github_login(GitHubLoginKind::Copilot, cx);
    }

    /// Starts the device flow that grants repository access (clone, push, and
    /// pull requests) through the GitHub CLI OAuth app.
    pub(crate) fn authorize_github_repository(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_github_login(GitHubLoginKind::Repository, cx);
    }

    /// Shared entry point for the Copilot and repository device-login flows.
    pub(crate) fn begin_github_login(&mut self, kind: GitHubLoginKind, cx: &mut Context<Self>) {
        let node_id = self
            .providers_node_id
            .clone()
            .unwrap_or_else(|| self.default_backend_node_id.clone());
        self.begin_github_login_for_node(kind, node_id, cx);
    }

    /// Starts the GitHub sign-in that binds a person to `node_id`.
    ///
    /// This is how a connection accepted without an operator token stops being
    /// pre-login: the worker derives the person from the GitHub account and
    /// binds it to this connection.
    pub(crate) fn begin_identity_sign_in(&mut self, node_id: String, cx: &mut Context<Self>) {
        self.begin_github_login_for_node(GitHubLoginKind::Identity, node_id, cx);
    }

    /// Starts a device-login flow of `kind` against `node_id`.
    pub(crate) fn begin_github_login_for_node(
        &mut self,
        kind: GitHubLoginKind,
        node_id: String,
        cx: &mut Context<Self>,
    ) {
        // A finished login pane toggles off. Any other leftover state belongs to
        // a flow that is not on screen, so a new one starts instead of the tap
        // being swallowed by state the user cannot see.
        if self.github_login_open()
            && self.github_login.as_ref().is_some_and(|state| {
                matches!(
                    state,
                    GitHubLoginState::Success | GitHubLoginState::Error(_)
                )
            })
        {
            self.close_covering_surface();
            cx.notify();
            return;
        }
        if !cfg!(target_family = "wasm")
            && !self
                .node_backends
                .get(&node_id)
                .is_some_and(BackendWorker::secure_for_secrets)
        {
            self.show_github_login(GitHubLoginState::Error(
                kind.secure_connection_message().to_owned(),
            ));
            cx.notify();
            return;
        }
        self.github_login_kind = kind;
        self.github_login_node_id = Some(node_id);
        self.start_github_login(cx);
    }

    /// Shows the GitHub login pane with `state`. This is how the pane starts; a
    /// later step of a running flow writes `github_login` directly, because it
    /// belongs to the pane the flow already put on screen.
    pub(crate) fn show_github_login(&mut self, state: GitHubLoginState) {
        self.show_surface(CoveringSurface::GitHubLogin);
        self.github_login = Some(state);
    }

    /// Closes the GitHub login pane, which is the surface its own close control
    /// belongs to. A control that fires after the pane was replaced only drops
    /// the state it held.
    pub(crate) fn close_github_login(&mut self, cx: &mut Context<Self>) {
        if !self.github_login_open() {
            self.github_login = None;
            cx.notify();
            return;
        }
        self.close_covering_surface();
        cx.notify();
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
        self.show_github_login(GitHubLoginState::Starting);
        cx.notify();
        self.start_github_login_flow(cx);
    }

    /// The worker performs the OAuth exchange and stores its credential;
    /// clients receive only the public device code and login status. Native
    /// and browser clients share this path, so the client never needs the
    /// provider implementation.
    pub(crate) fn start_github_login_flow(&mut self, cx: &mut Context<Self>) {
        let node_id = self
            .github_login_node_id
            .clone()
            .or_else(|| self.providers_node_id.clone())
            .unwrap_or_else(|| self.default_backend_node_id.clone());
        let Some(backend) = self.node_backends.get(&node_id).cloned() else {
            self.github_login = Some(GitHubLoginState::Error(
                "The selected worker is not connected.".to_owned(),
            ));
            cx.notify();
            return;
        };
        let kind = self.github_login_kind;
        let start_request = github_login_start_request(kind);
        let pending = backend.submit(RequestEnvelope::new(start_request));
        cx.spawn(async move |view, cx| {
            let response = pending.wait().await;
            let started = match response.result {
                Ok(response) => github_login_started(kind, response),
                Err(error) => Err(error),
            };
            let (login_id, user_code, verification_uri, expires_in, interval) = match started {
                Ok(started) => started,
                Err(error) => {
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
                if let Err(error) = github_login_delay(cx, interval).await {
                    view.update(cx, |view, cx| {
                        view.github_login = Some(GitHubLoginState::Error(error.message));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                let status_request = github_login_status_request(kind, login_id.clone());
                let response = backend
                    .submit(RequestEnvelope::new(status_request))
                    .wait()
                    .await;
                let status = match response.result {
                    Ok(response) => github_login_status(kind, response),
                    Err(error) => Err(error),
                };
                let status = match status {
                    Ok(status) => status,
                    Err(error) => {
                        view.update(cx, |view, cx| {
                            view.github_login = Some(GitHubLoginState::Error(error.message));
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                };
                match status {
                    DeviceLoginStatus::Pending => {}
                    DeviceLoginStatus::Configured => {
                        view.update(cx, |view, cx| match kind {
                            GitHubLoginKind::Copilot => {
                                view.handle_github_provider_configured(node_id.clone(), cx);
                            }
                            GitHubLoginKind::Repository => {
                                view.handle_github_repository_configured(node_id.clone(), cx);
                            }
                            // Only an identity flow reports a person instead.
                            GitHubLoginKind::Identity => {}
                        })
                        .ok();
                        return;
                    }
                    DeviceLoginStatus::SignedIn(participant) => {
                        view.update(cx, |view, cx| {
                            view.apply_github_sign_in_status(
                                &node_id,
                                GitHubSignInStatus::SignedIn { participant },
                                cx,
                            );
                        })
                        .ok();
                        return;
                    }
                    DeviceLoginStatus::Failed(message) => {
                        view.update(cx, |view, cx| {
                            view.github_login = Some(GitHubLoginState::Error(message));
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

    /// Marks repository access as configured and reflects it in settings.
    pub(crate) fn handle_github_repository_configured(
        &mut self,
        node_id: String,
        cx: &mut Context<Self>,
    ) {
        self.github_login = Some(GitHubLoginState::Success);
        self.github_repository_connected = true;
        self.record_status(format!(
            "GitHub repository access configured on {}",
            self.node_names
                .get(&node_id)
                .map_or(node_id.as_str(), String::as_str)
        ));
        self.refresh_identity_after_provider_login(&node_id, cx);
        cx.notify();
    }

    /// Reads the identity a successful provider login may have bound.
    ///
    /// A worker binds the account behind an operator's provider login, so the
    /// existing GitHub sign-in in Providers also joins the connection to the
    /// roster. The status response carries no participant, so the client asks.
    fn refresh_identity_after_provider_login(&mut self, node_id: &str, cx: &mut Context<Self>) {
        if self.active_node_id() != node_id {
            return;
        }
        self.collaboration.identity_loaded = false;
        self.refresh_collaboration(cx);
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
        self.refresh_identity_after_provider_login(&node_id, cx);
        self.dispatch_to_node(
            cx,
            node_id.clone(),
            ClientRequest::Provider(ProviderRequest::ListProviders),
            move |view, response, _| match response.result {
                Ok(ServerResponse::Provider(ProviderResponse::Providers { providers })) => {
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

    pub(crate) fn refresh_github_write_access(&self, node_id: String, cx: &mut Context<Self>) {
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::Provider(ProviderRequest::GetGitHubWriteAccess),
            |view, response, _| match response.result {
                Ok(ServerResponse::Provider(ProviderResponse::GitHubWriteAccess { enabled })) => {
                    view.github_write_access = enabled;
                }
                Err(error) => view.record_backend_error("read GitHub write access", error),
                Ok(response) => view.record_backend_error(
                    "read GitHub write access",
                    unexpected_response("GitHub write access", response),
                ),
            },
        );
    }

    pub(crate) fn refresh_github_repository_access(&self, node_id: String, cx: &mut Context<Self>) {
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::Provider(ProviderRequest::GetGitHubRepositoryAccess),
            |view, response, _| match response.result {
                Ok(ServerResponse::Provider(ProviderResponse::GitHubRepositoryAccess {
                    connected,
                })) => {
                    view.github_repository_connected = connected;
                }
                Err(error) => view.record_backend_error("read GitHub repository access", error),
                Ok(response) => view.record_backend_error(
                    "read GitHub repository access",
                    unexpected_response("GitHub repository access", response),
                ),
            },
        );
    }

    pub(crate) fn toggle_github_write_access(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.github_write_access = enabled;
        let node_id = self.default_backend_node_id.clone();
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::Provider(ProviderRequest::ConfigureGitHubWriteAccess { enabled }),
            move |view, response, _| match response.result {
                Ok(ServerResponse::Provider(ProviderResponse::GitHubWriteAccess {
                    enabled: configured,
                })) => {
                    view.github_write_access = configured;
                    view.record_status(if configured {
                        "GitHub write access enabled"
                    } else {
                        "GitHub write access disabled"
                    });
                }
                Err(error) => {
                    view.github_write_access = !enabled;
                    view.record_backend_error("configure GitHub write access", error);
                }
                Ok(response) => {
                    view.github_write_access = !enabled;
                    view.record_backend_error(
                        "configure GitHub write access",
                        unexpected_response("GitHub write access", response),
                    );
                }
            },
        );
        cx.notify();
    }

    pub(crate) fn sync_model_select_states(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = resolve_model_choice(&self.models, &self.model) {
            self.model = model.clone();
            self.session_models.insert(self.active_session.id, model);
        }
        if let Some(model) = resolve_model_choice(&self.default_models, &self.default_model) {
            self.default_model = model;
        }
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
        let model_value = model_choice_label(&self.model, active_provider_names);
        let default_model_value = model_choice_label(&self.default_model, default_provider_names);
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
        let model = resolve_model_choice(&self.models, &model).unwrap_or(model);
        self.session_models
            .insert(self.active_session.id, model.clone());
        self.model = model;
        cx.notify();
    }

    pub(crate) fn toggle_auto_approve_actions(&mut self, cx: &mut Context<Self>) {
        if self.approval_settings_request_in_flight || !self.is_connected() {
            return;
        }
        let session_id = self.active_session.id;
        let auto_approve_actions = !self.auto_approve_actions;
        let policy =
            session_approval_policy(auto_approve_actions, self.ask_before_destructive_actions);
        self.approval_settings_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::Session(SessionRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                auto_approve_actions: Some(auto_approve_actions),
            }),
            move |view, response, _| {
                if view.active_session.id != session_id {
                    return;
                }
                view.approval_settings_request_in_flight = false;
                match response.result {
                    Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(update))) => {
                        view.auto_approve_actions = auto_approve_actions;
                        view.session_auto_approve_actions
                            .insert(session_id, auto_approve_actions);
                        let text = if auto_approve_actions {
                            "Automatic approvals enabled for this session".to_owned()
                        } else {
                            "Automatic approvals disabled for this session".to_owned()
                        };
                        // The scope comes from the response: a run that is
                        // already executing keeps the policy it pinned.
                        let run_is_active = view.run_is_active();
                        view.record_status(approval_policy_status_text(
                            &text,
                            update.applies_from,
                            run_is_active,
                        ));
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

    /// Toggles whether destructive actions pause for approval in this session.
    pub(crate) fn toggle_ask_before_destructive_actions(&mut self, cx: &mut Context<Self>) {
        if self.approval_settings_request_in_flight || !self.is_connected() {
            return;
        }
        let session_id = self.active_session.id;
        let auto_approve_actions = self.auto_approve_actions;
        let ask_before_destructive_actions = !self.ask_before_destructive_actions;
        let policy = session_approval_policy(auto_approve_actions, ask_before_destructive_actions);
        self.approval_settings_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::Session(SessionRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                // The policy replaces every kind decision, so automatic
                // approvals are resent alongside it unchanged rather than
                // silently reset by this control.
                auto_approve_actions: Some(auto_approve_actions),
            }),
            move |view, response, _| {
                if view.active_session.id != session_id {
                    return;
                }
                view.approval_settings_request_in_flight = false;
                match response.result {
                    Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(update))) => {
                        view.ask_before_destructive_actions =
                            destructive_actions_require_approval(update.policy.destructive);
                        view.session_ask_before_destructive_actions
                            .insert(session_id, view.ask_before_destructive_actions);
                        let text = if view.ask_before_destructive_actions {
                            "Destructive actions will ask for approval in this session".to_owned()
                        } else {
                            "Destructive actions will run without approval in this session"
                                .to_owned()
                        };
                        // The scope comes from the response: a run that is
                        // already executing keeps the policy it pinned.
                        let run_is_active = view.run_is_active();
                        view.record_status(approval_policy_status_text(
                            &text,
                            update.applies_from,
                            run_is_active,
                        ));
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
        let Some(resolved_model) = resolve_model_choice(&self.default_models, &model) else {
            self.record_backend_error(
                "select default model",
                LoomError::invalid_state(format!(
                    "model '{}' is not available in the current worker model list",
                    model.as_str()
                )),
            );
            cx.notify();
            return;
        };
        let model = resolved_model;
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

    /// Opens the settings dialog, on the section it already showed.
    pub(crate) fn open_settings_from_menu(&mut self, cx: &mut Context<Self>) {
        let section = self.settings_section().unwrap_or(SettingsSection::Agents);
        self.open_settings_section(section, cx);
    }

    /// Opens the settings dialog on `section` and loads what that section shows.
    ///
    /// Opening it closes whatever covered the conversation, including the
    /// archived-session surface and the phone's session drawer.
    pub(crate) fn open_settings_section(
        &mut self,
        section: SettingsSection,
        cx: &mut Context<Self>,
    ) {
        self.show_surface(CoveringSurface::Settings(section));
        let node_id = self.default_backend_node_id.clone();
        self.refresh_github_write_access(node_id.clone(), cx);
        self.refresh_github_repository_access(node_id.clone(), cx);
        self.load_provider_scope(node_id, cx);
        cx.notify();
    }

    /// Loads the provider list for `node_id` and scopes provider settings to it.
    fn load_provider_scope(&mut self, node_id: String, cx: &mut Context<Self>) {
        self.providers_node_id = Some(node_id.clone());
        self.providers.clear();
        self.github_connected = false;
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::Provider(ProviderRequest::ListProviders),
            |view, response, _| match response.result {
                Ok(ServerResponse::Provider(ProviderResponse::Providers { providers })) => {
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
    }

    /// Persists the workspace config to the home workspace and writes it to
    /// every peer's own workspace. A workspace belongs to one worker, so no
    /// workspace record is shared across the fleet.
    pub(crate) fn persist_and_distribute_workspace_config(
        &self,
        retiring: Option<(String, ClientConnection)>,
        cx: &mut Context<Self>,
    ) {
        let home_workspace_id = self.workspace_id;
        let config = self.workspace_config.clone();
        let source = self.connection.clone();
        let peers = self
            .worker_nodes
            .iter()
            .filter(|node| !node.is_local)
            // A pre-login connection is refused the workspace reads this
            // distribution needs, so it is skipped until someone signs in.
            .filter(|node| !self.awaiting_sign_in_nodes.contains(&node.status.node_id))
            .filter_map(|node| {
                node.connection.as_ref().map(|connection| {
                    let known = self
                        .node_workspaces
                        .get(&node.status.node_id)
                        .and_then(|workspaces| active_workspace(workspaces))
                        .map(|workspace| workspace.id);
                    (node.status.name.clone(), connection.clone(), known)
                })
            })
            .collect::<Vec<_>>();
        #[cfg(not(target_family = "wasm"))]
        cx.spawn(async move |view, cx| {
            let errors = cx
                .background_spawn(async move {
                    let mut errors = Vec::new();
                    if let Err(error) =
                        set_workspace_config(&source, home_workspace_id, config.clone())
                    {
                        errors.push(("save workspace config".to_owned(), error));
                    }
                    for (name, connection, known) in peers {
                        let workspace_id = match known {
                            Some(workspace_id) => workspace_id,
                            None => match resolve_workspace_id(&connection) {
                                Ok(workspace_id) => workspace_id,
                                Err(error) => {
                                    errors.push((format!("resolve workspace on {name}"), error));
                                    continue;
                                }
                            },
                        };
                        if let Err(error) =
                            set_workspace_config(&connection, workspace_id, config.clone())
                        {
                            errors.push((format!("distribute workspace config to {name}"), error));
                        }
                    }
                    if let Some((name, connection)) = retiring
                        && let Err(error) = connection.close()
                    {
                        errors.push((format!("close worker node {name} connection"), error));
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
                set_workspace_config_async(&source, home_workspace_id, config.clone()).await
            {
                errors.push(("save workspace config".to_owned(), error));
            } else {
                for (name, connection, known) in peers {
                    let workspace_id = match known {
                        Some(workspace_id) => workspace_id,
                        None => match resolve_workspace_id_async(&connection).await {
                            Ok(workspace_id) => workspace_id,
                            Err(error) => {
                                errors.push((format!("resolve workspace on {name}"), error));
                                continue;
                            }
                        },
                    };
                    if let Err(error) =
                        set_workspace_config_async(&connection, workspace_id, config.clone()).await
                    {
                        errors.push((format!("distribute workspace config to {name}"), error));
                    }
                }
            }
            if let Some((name, connection)) = retiring
                && let Err(error) = connection.close()
            {
                errors.push((format!("close worker node {name} connection"), error));
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

    pub(crate) fn open_providers_from_menu(&mut self, cx: &mut Context<Self>) {
        self.open_settings_section(SettingsSection::Providers, cx);
    }

    /// Opens the About settings pane from the command palette or `/about`.
    pub(crate) fn open_about_from_menu(&mut self, cx: &mut Context<Self>) {
        // About reads what the client already knows, so it loads no provider
        // scope, exactly as it did before it was one section of the dialog.
        self.show_surface(CoveringSurface::Settings(SettingsSection::About));
        cx.notify();
    }

    /// Opens the Providers settings pane scoped to a specific worker node.
    pub(crate) fn open_providers_for_node(&mut self, node_id: String, cx: &mut Context<Self>) {
        self.show_surface(CoveringSurface::Settings(SettingsSection::Providers));
        self.refresh_github_write_access(node_id.clone(), cx);
        self.refresh_github_repository_access(node_id.clone(), cx);
        self.load_provider_scope(node_id, cx);
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
            ClientRequest::Provider(ProviderRequest::ConfigureApiKeyProvider {
                provider_id: provider_id.clone(),
                api_key,
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Provider(ProviderResponse::ProviderConfigured)) => {
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
                    view.load_provider_scope(node_id.clone(), cx);
                    view.refresh_models_for_node_async(node_id, cx);
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

    /// Drops the API key configured for `provider_id` on the scoped worker.
    pub(crate) fn remove_api_key_provider(
        &mut self,
        provider_id: loom_model::ProviderId,
        cx: &mut Context<Self>,
    ) {
        let Some(node_id) = self.providers_node_id.clone() else {
            self.provider_setup_status.insert(
                provider_id,
                "Choose a worker before removing an API key".to_owned(),
            );
            cx.notify();
            return;
        };
        self.provider_setup_status.insert(
            provider_id.clone(),
            format!(
                "Removing API key from {}…",
                self.node_names
                    .get(&node_id)
                    .map_or("worker", String::as_str)
            ),
        );
        cx.notify();
        self.dispatch_to_node(
            cx,
            node_id.clone(),
            ClientRequest::Provider(ProviderRequest::RemoveApiKeyProvider {
                provider_id: provider_id.clone(),
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Provider(ProviderResponse::ProviderConfigured)) => {
                    view.provider_setup_status.insert(
                        provider_id.clone(),
                        format!(
                            "API key removed from {}",
                            view.node_names
                                .get(&node_id)
                                .map_or("worker", String::as_str)
                        ),
                    );
                    view.record_status(format!("Provider API key removed on {node_id}"));
                    view.load_provider_scope(node_id.clone(), cx);
                    view.refresh_models_for_node_async(node_id, cx);
                }
                Err(error) => {
                    view.provider_setup_status.insert(
                        provider_id.clone(),
                        format!("Could not remove API key: {}", error.message),
                    );
                    view.record_backend_error("remove provider API key", error);
                }
                Ok(response) => {
                    let error = unexpected_response("provider API key removal", response);
                    view.provider_setup_status.insert(
                        provider_id.clone(),
                        format!("Could not remove API key: {}", error.message),
                    );
                    view.record_backend_error("remove provider API key", error);
                }
            },
        );
    }

    /// Closes the settings dialog, which is the surface its own close control
    /// belongs to.
    pub(crate) fn close_settings(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_open() {
            self.close_covering_surface();
        }
        for input in self.provider_api_key_inputs.values() {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.provider_api_key_inputs.clear();
        self.provider_setup_status.clear();
        cx.notify();
    }

    pub(crate) fn select_theme(
        &mut self,
        theme: ThemeChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.theme_choice != theme;
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
        if changed {
            self.persist_ui_preferences();
        }
    }

    /// Toggles whether provider reasoning is shown in the transcript.
    ///
    /// Reasoning is content the backend only sends when the client asks for it,
    /// so showing it re-reads the active session when the level would render it;
    /// the backend never sees the setting itself, only the projection it implies.
    pub(crate) fn toggle_show_reasoning(&mut self, cx: &mut Context<Self>) {
        let projection_before = self.output_detail.activity_detail(self.show_reasoning);
        self.show_reasoning = !self.show_reasoning;
        self.persist_ui_preferences();
        if self.output_detail.activity_detail(self.show_reasoning) != projection_before {
            self.reread_active_session_for_projection(cx);
        }
        cx.notify();
    }

    /// Sets how much agent activity the transcript shows.
    ///
    /// The level decides the projection the client asks for, so a level that
    /// renders more than the last read fetched has to read the session again.
    /// `Medium` and `High` ask for the same projection and differ only in how
    /// much of it is open, so switching between them never re-reads.
    pub(crate) fn set_output_detail(&mut self, detail: OutputDetail, cx: &mut Context<Self>) {
        let changed = self.output_detail != detail;
        let projection_changed = self.output_detail.activity_detail(self.show_reasoning)
            != detail.activity_detail(self.show_reasoning);
        self.output_detail = detail;
        if changed {
            self.persist_ui_preferences();
        }
        if projection_changed {
            self.reread_active_session_for_projection(cx);
        }
        cx.notify();
    }

    /// Re-reads the active session after the activity projection changed.
    ///
    /// Content the client did not fetch cannot be shown, so a level that needs
    /// more than the last read starts the same load an interactive selection
    /// does. The reset before it keeps the partial projection from being shown
    /// as the conversation until the read answers.
    fn reread_active_session_for_projection(&mut self, cx: &mut Context<Self>) {
        if !self
            .sessions
            .iter()
            .any(|session| session.id == self.active_session.id)
        {
            return;
        }
        self.reset_projection();
        self.begin_session_data_load(self.active_session.id, cx);
    }

    /// Writes the client-local presentation settings after a change. The
    /// backend never sees them, and a failed write must not interrupt the UI.
    pub(crate) fn persist_ui_preferences(&self) {
        UiPreferences {
            show_reasoning: self.show_reasoning,
            output_detail: self.output_detail,
            theme_choice: self.theme_choice,
            font_scale_percent: self.font_scale_percent,
        }
        .save();
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

/// The workspace to write settings into on a peer: its most recently updated
/// one, or a newly created `Default` when it has none. A workspace is never
/// shared across workers, so this resolves per connection.
#[cfg(not(target_family = "wasm"))]
fn resolve_workspace_id(connection: &ClientConnection) -> Result<WorkspaceId, LoomError> {
    let workspaces = list_workspaces(connection)?;
    if let Some(workspace) = active_workspace(&workspaces) {
        return Ok(workspace.id);
    }
    Ok(create_workspace(connection, "Default")?.id)
}

#[cfg(target_family = "wasm")]
async fn resolve_workspace_id_async(
    connection: &ClientConnection,
) -> Result<WorkspaceId, LoomError> {
    let workspaces = list_workspaces_async(connection).await?;
    if let Some(workspace) = active_workspace(&workspaces) {
        return Ok(workspace.id);
    }
    Ok(create_workspace_async(connection, "Default").await?.id)
}

/// The status a device-login poll reports.
///
/// The Copilot and repository flows end in a configured credential; an
/// identity flow ends in a person, which is what `SignedIn` carries.
enum DeviceLoginStatus {
    Pending,
    Configured,
    SignedIn(ParticipantIdentity),
    Failed(String),
}

/// The request that starts a device flow of `kind`.
fn github_login_start_request(kind: GitHubLoginKind) -> ClientRequest {
    match kind {
        GitHubLoginKind::Copilot => {
            ClientRequest::Provider(ProviderRequest::StartGitHubCopilotLogin)
        }
        GitHubLoginKind::Repository => {
            ClientRequest::Provider(ProviderRequest::StartGitHubRepositoryLogin)
        }
        GitHubLoginKind::Identity => ClientRequest::Access(AccessRequest::StartGitHubSignIn),
    }
}

/// The poll request for a device flow of `kind`.
fn github_login_status_request(kind: GitHubLoginKind, login_id: String) -> ClientRequest {
    match kind {
        GitHubLoginKind::Copilot => {
            ClientRequest::Provider(ProviderRequest::GetGitHubCopilotLoginStatus { login_id })
        }
        GitHubLoginKind::Repository => {
            ClientRequest::Provider(ProviderRequest::GetGitHubRepositoryLoginStatus { login_id })
        }
        GitHubLoginKind::Identity => {
            ClientRequest::Access(AccessRequest::GetGitHubSignInStatus { login_id })
        }
    }
}

/// The device code a start response carries, for any flow kind.
fn github_login_started(
    kind: GitHubLoginKind,
    response: ServerResponse,
) -> Result<(String, String, String, u64, u64), LoomError> {
    match (kind, response) {
        (
            GitHubLoginKind::Copilot,
            ServerResponse::Provider(ProviderResponse::GitHubCopilotLoginStarted {
                login_id,
                user_code,
                verification_uri,
                expires_in,
                interval,
            }),
        )
        | (
            GitHubLoginKind::Repository,
            ServerResponse::Provider(ProviderResponse::GitHubRepositoryLoginStarted {
                login_id,
                user_code,
                verification_uri,
                expires_in,
                interval,
            }),
        )
        | (
            GitHubLoginKind::Identity,
            ServerResponse::Access(AccessResponse::GitHubSignInStarted {
                login_id,
                user_code,
                verification_uri,
                expires_in,
                interval,
            }),
        ) => Ok((login_id, user_code, verification_uri, expires_in, interval)),
        (_, response) => Err(unexpected_response("GitHub login start", response)),
    }
}

/// The state a poll response reports, for any flow kind.
fn github_login_status(
    kind: GitHubLoginKind,
    response: ServerResponse,
) -> Result<DeviceLoginStatus, LoomError> {
    match (kind, response) {
        (
            GitHubLoginKind::Copilot,
            ServerResponse::Provider(ProviderResponse::GitHubCopilotLoginStatus { status }),
        )
        | (
            GitHubLoginKind::Repository,
            ServerResponse::Provider(ProviderResponse::GitHubRepositoryLoginStatus { status }),
        ) => Ok(match status {
            GitHubCopilotLoginStatus::Pending => DeviceLoginStatus::Pending,
            GitHubCopilotLoginStatus::Configured => DeviceLoginStatus::Configured,
            GitHubCopilotLoginStatus::Failed { message } => DeviceLoginStatus::Failed(message),
        }),
        (
            GitHubLoginKind::Identity,
            ServerResponse::Access(AccessResponse::GitHubSignInStatus { status }),
        ) => Ok(match status {
            GitHubSignInStatus::Pending => DeviceLoginStatus::Pending,
            GitHubSignInStatus::SignedIn { participant } => {
                DeviceLoginStatus::SignedIn(participant)
            }
            GitHubSignInStatus::Failed { message } => DeviceLoginStatus::Failed(message),
        }),
        (_, response) => Err(unexpected_response("GitHub login status", response)),
    }
}

/// Waits between GitHub login status polls without blocking the UI thread.
#[cfg(target_family = "wasm")]
async fn github_login_delay(_: &mut AsyncApp, interval: Duration) -> Result<(), LoomError> {
    browser_delay(interval).await
}

#[cfg(not(target_family = "wasm"))]
async fn github_login_delay(cx: &mut AsyncApp, interval: Duration) -> Result<(), LoomError> {
    cx.background_executor().timer(interval).await;
    Ok(())
}
