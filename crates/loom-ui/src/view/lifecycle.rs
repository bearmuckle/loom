use super::*;

impl LoomView {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn try_new(
        options: &UiOptions,
        focus_handle: FocusHandle,
    ) -> Result<Self, LoomError> {
        info!("bootstrapping backend connection");
        let mut remote_cleanup_guard = None;
        let (connection, workspace_root, demo_workspace, owned_backend) = if let Some(remote_url) =
            &options.remote
        {
            info!("connecting to remote backend");
            if worker_url_embeds_credential(remote_url) {
                return Err(LoomError::invalid_request(
                    "remote URL must not contain credentials; provide the access token separately",
                ));
            }
            let token = options.token.as_deref().ok_or_else(|| {
                LoomError::invalid_request("remote connections require LOOM_TOKEN to be set")
            })?;
            let connection = ClientConnection::remote(remote_url.clone(), token.to_owned())?;
            remote_cleanup_guard = Some(ConnectionCleanupGuard::new(connection.clone()));
            info!("remote transport connected; negotiating protocol");
            negotiate(&connection)?;
            let workspace_root = options
                .project
                .clone()
                .map(fs::canonicalize)
                .transpose()
                .map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not open repository source: {error}"),
                        false,
                    )
                })?
                .unwrap_or_default();
            (connection, workspace_root, false, None)
        } else {
            let (workspace_root, demo_workspace) = prepare_workspace(options)?;
            info!(
                "using workspace '{}'{}",
                workspace_root.display(),
                if demo_workspace { " (demo)" } else { "" }
            );
            let backend = if demo_workspace {
                info!("starting demo backend");
                InProcessBackend::demo_with_github_copilot()?
            } else if let Some(endpoint) = &options.endpoint {
                let persistence_path = backend_persistence_path();
                info!(
                    "starting local backend with OpenAI-compatible endpoint; state '{}'",
                    persistence_path.display()
                );
                InProcessBackend::with_openai_compatible_persistent_with_github_copilot(
                    endpoint,
                    options.api_key.as_deref().unwrap_or_default(),
                    options.model.clone(),
                    persistence_path,
                )?
            } else {
                let persistence_path = backend_persistence_path();
                info!(
                    "starting local backend with GitHub Copilot; state '{}'",
                    persistence_path.display()
                );
                InProcessBackend::new_persistent_with_github_copilot(persistence_path)?
            };
            (
                ClientConnection::InProcess(Box::new(backend.connect())),
                workspace_root,
                demo_workspace,
                Some(backend),
            )
        };
        if options.remote.is_none() {
            info!("negotiating protocol");
            negotiate(&connection)?;
        }
        let mut view = Self::initialize_from_connection(
            options,
            connection,
            workspace_root,
            demo_workspace,
            remote_cleanup_guard,
            focus_handle,
            true,
        )?;
        view.owned_backend = owned_backend;
        Ok(view)
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn initialize_from_connection(
        options: &UiOptions,
        connection: ClientConnection,
        workspace_root: PathBuf,
        demo_workspace: bool,
        mut remote_cleanup_guard: Option<ConnectionCleanupGuard>,
        focus_handle: FocusHandle,
        discover_models: bool,
    ) -> Result<Self, LoomError> {
        let node_status = worker_node_status(&connection)?;
        let default_backend_node_id = node_status.node_id.clone();
        let mut workspaces = list_workspaces(&connection)?;
        let workspace = match workspaces.first() {
            Some(workspace) => workspace.clone(),
            None => {
                let workspace = create_workspace(&connection, "Default")?;
                workspaces.push(workspace.clone());
                workspace
            }
        };
        let workspace_id = workspace.id;
        let workspace_config = workspace_config(&connection, workspace_id)?;
        let worker_nodes = initial_worker_nodes(
            node_status,
            connection.clone(),
            &workspace_config,
            options.remote.as_deref(),
        );
        let sessions = list_workspace_sessions(&connection, workspace_id)?;
        info!("loaded {} session(s)", sessions.len());
        let had_sessions = !sessions.is_empty();
        let (session, new_session) = match sessions.into_iter().next() {
            Some(session) => {
                info!("resuming session {}", session.id);
                (session, false)
            }
            None => {
                if workspace_root.as_os_str().is_empty() {
                    (empty_session_snapshot(workspace_id), false)
                } else {
                    info!("creating a session for the requested workspace");
                    (
                        create_session_in_workspace(
                            &connection,
                            workspace_id,
                            session_name_for_path(&workspace_root)
                                .as_deref()
                                .unwrap_or("New session"),
                        )?,
                        true,
                    )
                }
            }
        };
        let has_session = had_sessions || new_session;
        if new_session && !workspace_root.as_os_str().is_empty() {
            if options.remote.is_none() {
                let response = connection.request(RequestEnvelope::new(
                    ClientRequest::AttachSessionDirectory {
                        session_id: session.id,
                        source: workspace_root.display().to_string(),
                        path: "sources/local".to_owned(),
                    },
                ));
                match response.result? {
                    ServerResponse::SessionDirectoryAttached { .. } => {}
                    response => {
                        return Err(unexpected_response("directory attachment", response));
                    }
                }
            } else if workspace_root.join(".git").exists() {
                attach_session_repository(
                    &connection,
                    session.id,
                    &workspace_root.display().to_string(),
                    "repo",
                )?;
            }
        }
        let model_catalog = list_models(&connection)?;
        let models = model_catalog.models;
        let model_provider_names = model_catalog.provider_names;
        let model = if models.contains(&options.model) {
            options.model.clone()
        } else {
            models
                .iter()
                .find(|model| model.as_str() == GITHUB_COPILOT_DEFAULT_MODEL)
                .cloned()
                .or_else(|| models.first().cloned())
                .unwrap_or_else(|| options.model.clone())
        };
        let node_model_catalogs =
            BTreeMap::from([(default_backend_node_id.clone(), models.clone())]);
        info!(
            "loaded {} model(s); selected '{}'",
            models.len(),
            model.as_str()
        );
        let run = if demo_workspace {
            info!("starting demo agent run");
            Some(start_run(&connection, &session, &model, &options.task)?)
        } else {
            None
        };
        let backend = BackendWorker::spawn(connection.clone());
        let node_backends = BTreeMap::from([(default_backend_node_id.clone(), backend.clone())]);
        let session_node_ids = if has_session {
            BTreeMap::from([(session.id, default_backend_node_id.clone())])
        } else {
            BTreeMap::new()
        };
        let node_names = worker_nodes
            .iter()
            .map(|node| (node.status.node_id.clone(), worker_node_display_name(node)))
            .collect();
        let mut view = Self {
            backend,
            connection: connection.clone(),
            #[cfg(not(target_family = "wasm"))]
            owned_backend: None,
            default_backend_node_id: default_backend_node_id.clone(),
            node_backends,
            node_names,
            session_node_ids,
            workspace_id,
            workspaces,
            local_directory_sources_available: options.remote.is_none(),
            sessions: if has_session {
                vec![session.clone()]
            } else {
                Vec::new()
            },
            project_snapshot: None,
            project_tree_snapshots: Vec::new(),
            project_child_review: None,
            project_snapshot_stale: false,
            project_messages: Vec::new(),
            project_message_cursors: BTreeMap::new(),
            project_messages_stale: false,
            project_messages_loading: false,
            project_message_generation: 0,
            project_feed_after_sequence: None,
            project_feed_epoch: None,
            project_poll_scheduled: false,
            session_tree: None,
            session_tree_entries: Vec::new(),
            active_session: session.clone(),
            active_run: run.clone(),
            active_run_id: run.as_ref().map(|run| run.id),
            context_inspection: None,
            default_model: model.clone(),
            session_models: BTreeMap::new(),
            agent_mode: AgentMode::Agent,
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model,
            models: models.clone(),
            default_models: models,
            node_model_catalogs,
            node_model_provider_names: BTreeMap::from([(
                default_backend_node_id.clone(),
                model_provider_names,
            )]),
            model_catalog_node_id: Some(default_backend_node_id.clone()),
            model_refreshes_in_flight: BTreeSet::new(),
            model_select: None,
            default_model_select: None,
            model_select_subscription: None,
            default_model_select_subscription: None,
            model_select_items: Vec::new(),
            default_model_select_items: Vec::new(),
            model_select_value: None,
            default_model_select_value: None,
            model_select_choices: BTreeMap::new(),
            default_model_select_choices: BTreeMap::new(),
            agent_mode_select: None,
            agent_mode_select_subscription: None,
            settings_open: false,
            settings_section: SettingsSection::Agents,
            providers_open: false,
            about_open: false,
            providers: Vec::new(),
            providers_node_id: None,
            provider_api_key_inputs: BTreeMap::new(),
            provider_setup_status: BTreeMap::new(),
            theme_choice: ThemeChoice::System,
            font_scale_percent: DEFAULT_FONT_SCALE_PERCENT,
            appearance_subscription: None,
            after_sequence: None,
            event_stream_epoch: None,
            timeline: Vec::new(),
            transcript_before_ordinal: None,
            transcript_loaded_ordinals: BTreeSet::new(),
            transcript_messages: BTreeMap::new(),
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            timeline_view: None,
            activity_records: BTreeMap::new(),
            expanded_tools: BTreeSet::new(),
            expanded_tool_groups: BTreeSet::new(),
            expanded_reasoning: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer_input: None,
            composer_placeholder: None,
            composer_completion: None,
            pending_completion_accept: false,
            suppress_completion_once: false,
            command_palette_open: false,
            command_palette_input: None,
            command_palette_selection: 0,
            session_filter_input: None,
            input_subscriptions: Vec::new(),
            clear_composer_on_render: false,
            clear_node_on_render: false,
            rename_input_state: None,
            source_path_input: None,
            repository_filter_input: None,
            pending_source_path: None,
            composer_focus_handle: focus_handle,
            session_state: session.state,
            run_state: run.as_ref().map(|run| run.state),
            review: ReviewState::default(),
            session_repositories: Vec::new(),
            session_directories: Vec::new(),
            selected_repository_id: None,
            session_drawer_open: false,
            rename_dialog: None,
            source_dialog: None,
            #[cfg(target_family = "wasm")]
            welcome_dialog_dismissed: false,
            demo_workspace,
            login_enabled: true,
            github_connected: false,
            github_login: None,
            next_worker_node_id: worker_nodes.len() as u64,
            worker_nodes,
            worker_node_polls_scheduled: BTreeSet::new(),
            workspace_config,
            node_input_initial: String::new(),
            node_input_state: None,
            run_poll_scheduled: false,
            browser_startup_error: None,
        };
        if discover_models {
            view.refresh_models();
        }
        view.refresh_sessions()?;
        let active_session = view.active_session.clone();
        if has_session {
            view.load_session(active_session);
        }
        info!("initial session state loaded");
        if let Some(guard) = &mut remote_cleanup_guard {
            guard.disarm();
        }
        Ok(view)
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn new_browser_disconnected(
        options: &BrowserOptions,
        startup_error: Option<String>,
        focus_handle: FocusHandle,
    ) -> Self {
        let connection = ClientConnection::Disconnected;
        let backend = BackendWorker::spawn(connection.clone());
        let workspace_id = WorkspaceId::new();
        let timestamp = loom_core::Timestamp::from_unix_millis(0);
        let demo_mode = options.demo();
        let active_session = AgentSessionSnapshot {
            id: AgentSessionId::new(),
            workspace_id,
            name: if demo_mode {
                "Demo conversation"
            } else {
                "No worker connected"
            }
            .to_owned(),
            state: AgentSessionState::Idle,
            created_at: timestamp,
            updated_at: timestamp,
        };
        let node_input_initial = format!("{} {}", options.remote(), options.token())
            .trim()
            .to_owned();

        Self {
            connected: demo_mode,
            browser_demo_mode: demo_mode,
            backend,
            connection,
            default_backend_node_id: String::new(),
            node_backends: BTreeMap::new(),
            node_names: BTreeMap::new(),
            session_node_ids: BTreeMap::new(),
            workspace_id,
            workspaces: Vec::new(),
            local_directory_sources_available: false,
            sessions: if demo_mode {
                vec![active_session.clone()]
            } else {
                Vec::new()
            },
            project_snapshot: None,
            project_tree_snapshots: Vec::new(),
            project_child_review: None,
            project_snapshot_stale: false,
            project_messages: Vec::new(),
            project_message_cursors: BTreeMap::new(),
            project_messages_stale: false,
            project_messages_loading: false,
            project_message_generation: 0,
            project_feed_after_sequence: None,
            project_feed_epoch: None,
            project_poll_scheduled: false,
            session_tree: None,
            session_tree_entries: Vec::new(),
            active_session,
            active_run: None,
            active_run_id: None,
            context_inspection: None,
            default_model: if demo_mode {
                ModelId::new("deterministic/demo")
            } else {
                ModelId::new("default")
            },
            session_models: BTreeMap::new(),
            agent_mode: AgentMode::Agent,
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model: if demo_mode {
                ModelId::new("deterministic/demo")
            } else {
                ModelId::new("default")
            },
            models: if demo_mode {
                vec![ModelId::new("deterministic/demo")]
            } else {
                Vec::new()
            },
            default_models: if demo_mode {
                vec![ModelId::new("deterministic/demo")]
            } else {
                Vec::new()
            },
            node_model_catalogs: BTreeMap::new(),
            node_model_provider_names: BTreeMap::new(),
            model_catalog_node_id: None,
            model_refreshes_in_flight: BTreeSet::new(),
            model_select: None,
            default_model_select: None,
            model_select_subscription: None,
            default_model_select_subscription: None,
            model_select_items: Vec::new(),
            default_model_select_items: Vec::new(),
            model_select_value: None,
            default_model_select_value: None,
            model_select_choices: BTreeMap::new(),
            default_model_select_choices: BTreeMap::new(),
            agent_mode_select: None,
            agent_mode_select_subscription: None,
            settings_open: options.is_configured(),
            settings_section: SettingsSection::Agents,
            providers_open: false,
            about_open: false,
            providers: Vec::new(),
            providers_node_id: None,
            provider_api_key_inputs: BTreeMap::new(),
            provider_setup_status: BTreeMap::new(),
            theme_choice: ThemeChoice::System,
            font_scale_percent: DEFAULT_FONT_SCALE_PERCENT,
            appearance_subscription: None,
            after_sequence: None,
            event_stream_epoch: None,
            timeline: if demo_mode {
                vec![
                    TimelineItem::User("What can Loom do?".to_owned()),
                    TimelineItem::Assistant(AssistantTurn::text(
                        "Loom gives you a workspace for steering coding agents. Connect a backend to work with a real repository, run tools, and keep sessions available across clients. This browser demo is a static preview.",
                    )),
                ]
            } else {
                Vec::new()
            },
            transcript_before_ordinal: None,
            transcript_loaded_ordinals: BTreeSet::new(),
            transcript_messages: BTreeMap::new(),
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            timeline_view: None,
            activity_records: BTreeMap::new(),
            expanded_tools: BTreeSet::new(),
            expanded_tool_groups: BTreeSet::new(),
            expanded_reasoning: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer_input: None,
            composer_placeholder: None,
            composer_completion: None,
            pending_completion_accept: false,
            suppress_completion_once: false,
            command_palette_open: false,
            command_palette_input: None,
            command_palette_selection: 0,
            session_filter_input: None,
            input_subscriptions: Vec::new(),
            clear_composer_on_render: false,
            clear_node_on_render: false,
            rename_input_state: None,
            source_path_input: None,
            repository_filter_input: None,
            pending_source_path: None,
            composer_focus_handle: focus_handle,
            session_state: AgentSessionState::Idle,
            run_state: None,
            review: ReviewState::default(),
            session_repositories: Vec::new(),
            session_directories: Vec::new(),
            selected_repository_id: None,
            session_drawer_open: false,
            rename_dialog: None,
            source_dialog: None,
            #[cfg(target_family = "wasm")]
            welcome_dialog_dismissed: false,
            demo_workspace: demo_mode,
            login_enabled: false,
            github_connected: false,
            github_login: None,
            next_worker_node_id: 0,
            worker_nodes: Vec::new(),
            worker_node_polls_scheduled: BTreeSet::new(),
            workspace_config: WorkspaceConfig::default(),
            node_input_initial,
            node_input_state: None,
            run_poll_scheduled: false,
            browser_workspace: options.workspace().map(str::to_owned),
            browser_model: options.model().cloned(),
            browser_window_initialized: true,
            browser_startup_error: startup_error,
        }
    }

    /// Builds the view for the browser client: connects to a remote backend
    /// over the in-page WebSocket transport and resolves the same project /
    /// session / model state that native's remote-mode bootstrap resolves,
    /// using the `_async` request helpers since nothing may block the page's
    /// single JS thread. Unlike [`Self::try_new`], this does not load the
    /// active session's snapshot/events itself (that requires a `Context`,
    /// which does not exist yet); the caller finishes bootstrapping once the
    /// view is mounted, via [`Self::select_session`] and [`Self::reload_sessions`].
    #[cfg(target_family = "wasm")]
    pub(crate) async fn try_new_browser(
        options: &BrowserOptions,
        focus_handle: FocusHandle,
    ) -> Result<Self, LoomError> {
        if worker_url_embeds_credential(options.remote()) {
            return Err(LoomError::invalid_request(
                "remote URL must not contain credentials; provide the access token separately",
            ));
        }
        let connection = ClientConnection::browser(options.remote(), options.token())?;
        let mut cleanup_guard = ConnectionCleanupGuard::new(connection.clone());
        negotiate_async(&connection).await?;
        let node_status = worker_node_status_async(&connection).await?;
        let default_backend_node_id = node_status.node_id.clone();
        let mut workspaces = list_workspaces_async(&connection).await?;
        let workspace = match workspaces.first() {
            Some(workspace) => workspace.clone(),
            None => {
                let workspace = create_workspace_async(&connection, "Default").await?;
                workspaces.push(workspace.clone());
                workspace
            }
        };
        let workspace_id = workspace.id;
        let sessions = list_workspace_sessions_async(&connection, workspace_id).await?;
        let had_sessions = !sessions.is_empty();
        let workspace_config = workspace_config_async(&connection, workspace_id).await?;
        let mut worker_nodes = initial_worker_nodes(
            node_status,
            connection.clone(),
            &workspace_config,
            Some(options.remote()),
        );
        if let Err(error) = options.persist_connection() {
            worker_nodes[0].connection_detail = Some(worker_connection_failure_detail(
                WorkerConnectionStage::BootstrapSave,
                &error,
                Some(options.token()),
            ));
        }
        let (session, new_session) = match sessions.into_iter().next() {
            Some(session) => (session, false),
            None => {
                if options.workspace().is_some() {
                    (
                        create_session_in_workspace_async(
                            &connection,
                            workspace_id,
                            session_name_for_path(Path::new(options.workspace().unwrap()))
                                .as_deref()
                                .unwrap_or("New session"),
                        )
                        .await?,
                        true,
                    )
                } else {
                    (empty_session_snapshot(workspace_id), false)
                }
            }
        };
        let has_session = had_sessions || new_session;
        let workspace_root = options.workspace().map(PathBuf::from).unwrap_or_default();
        if new_session && !workspace_root.as_os_str().is_empty() {
            attach_session_repository_async(
                &connection,
                session.id,
                &workspace_root.display().to_string(),
                "repo",
            )
            .await?;
        }
        let model_catalog = list_models_async(&connection).await?;
        let models = model_catalog.models;
        let model_provider_names = model_catalog.provider_names;
        let model = options
            .model()
            .cloned()
            .filter(|model| models.contains(model))
            .or_else(|| models.first().cloned())
            .unwrap_or_else(|| ModelId::new("default"));
        let node_model_catalogs =
            BTreeMap::from([(default_backend_node_id.clone(), models.clone())]);
        let backend = BackendWorker::spawn(connection.clone());
        let node_backends = BTreeMap::from([(default_backend_node_id.clone(), backend.clone())]);
        let session_node_ids = if has_session {
            BTreeMap::from([(session.id, default_backend_node_id.clone())])
        } else {
            BTreeMap::new()
        };
        let node_names = worker_nodes
            .iter()
            .map(|node| (node.status.node_id.clone(), worker_node_display_name(node)))
            .collect();

        let view = Self {
            connected: true,
            browser_demo_mode: false,
            backend,
            connection: connection.clone(),
            default_backend_node_id: default_backend_node_id.clone(),
            node_backends,
            node_names,
            session_node_ids,
            workspace_id,
            workspaces,
            local_directory_sources_available: false,
            sessions: if has_session {
                vec![session.clone()]
            } else {
                Vec::new()
            },
            project_snapshot: None,
            project_tree_snapshots: Vec::new(),
            project_child_review: None,
            project_snapshot_stale: false,
            project_messages: Vec::new(),
            project_message_cursors: BTreeMap::new(),
            project_messages_stale: false,
            project_messages_loading: false,
            project_message_generation: 0,
            project_feed_after_sequence: None,
            project_feed_epoch: None,
            project_poll_scheduled: false,
            session_tree: None,
            session_tree_entries: Vec::new(),
            active_session: session.clone(),
            active_run: None,
            active_run_id: None,
            context_inspection: None,
            default_model: model.clone(),
            session_models: BTreeMap::new(),
            agent_mode: AgentMode::Agent,
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model,
            models: models.clone(),
            default_models: models,
            node_model_catalogs,
            node_model_provider_names: BTreeMap::from([(
                default_backend_node_id.clone(),
                model_provider_names,
            )]),
            model_catalog_node_id: None,
            model_refreshes_in_flight: BTreeSet::new(),
            model_select: None,
            default_model_select: None,
            model_select_subscription: None,
            default_model_select_subscription: None,
            model_select_items: Vec::new(),
            default_model_select_items: Vec::new(),
            model_select_value: None,
            default_model_select_value: None,
            model_select_choices: BTreeMap::new(),
            default_model_select_choices: BTreeMap::new(),
            agent_mode_select: None,
            agent_mode_select_subscription: None,
            settings_open: false,
            settings_section: SettingsSection::Agents,
            providers_open: false,
            about_open: false,
            providers: Vec::new(),
            providers_node_id: None,
            provider_api_key_inputs: BTreeMap::new(),
            provider_setup_status: BTreeMap::new(),
            theme_choice: ThemeChoice::System,
            font_scale_percent: DEFAULT_FONT_SCALE_PERCENT,
            appearance_subscription: None,
            after_sequence: None,
            event_stream_epoch: None,
            timeline: Vec::new(),
            transcript_before_ordinal: None,
            transcript_loaded_ordinals: BTreeSet::new(),
            transcript_messages: BTreeMap::new(),
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            timeline_view: None,
            activity_records: BTreeMap::new(),
            expanded_tools: BTreeSet::new(),
            expanded_tool_groups: BTreeSet::new(),
            expanded_reasoning: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer_input: None,
            composer_placeholder: None,
            composer_completion: None,
            pending_completion_accept: false,
            suppress_completion_once: false,
            command_palette_open: false,
            command_palette_input: None,
            command_palette_selection: 0,
            session_filter_input: None,
            input_subscriptions: Vec::new(),
            clear_composer_on_render: false,
            clear_node_on_render: false,
            rename_input_state: None,
            source_path_input: None,
            repository_filter_input: None,
            pending_source_path: None,
            composer_focus_handle: focus_handle,
            session_state: session.state,
            run_state: None,
            review: ReviewState::default(),
            session_repositories: Vec::new(),
            session_directories: Vec::new(),
            selected_repository_id: None,
            session_drawer_open: false,
            rename_dialog: None,
            source_dialog: None,
            #[cfg(target_family = "wasm")]
            welcome_dialog_dismissed: false,
            demo_workspace: false,
            login_enabled: true,
            github_connected: false,
            github_login: None,
            next_worker_node_id: worker_nodes.len() as u64,
            worker_nodes,
            worker_node_polls_scheduled: BTreeSet::new(),
            workspace_config,
            node_input_initial: String::new(),
            node_input_state: None,
            run_poll_scheduled: false,
            browser_workspace: options.workspace().map(str::to_owned),
            browser_model: options.model().cloned(),
            browser_window_initialized: false,
            browser_startup_error: None,
        };
        cleanup_guard.disarm();
        Ok(view)
    }

    /// Submits a backend request without blocking the UI thread and applies the
    /// answer on the UI thread once it arrives.
    pub(crate) fn dispatch(
        &self,
        cx: &mut Context<Self>,
        request: ClientRequest,
        apply: impl FnOnce(&mut Self, ResponseEnvelope, &mut Context<Self>) + 'static,
    ) {
        let request_envelope = RequestEnvelope::new(request);
        let request_id = request_envelope.request_id;
        let pending = self
            .backend_for_request(&request_envelope.request)
            .map(|backend| backend.submit(request_envelope));
        cx.spawn(async move |view, cx| {
            let response = match pending {
                Ok(pending) => {
                    cx.background_spawn(async move { pending.wait().await })
                        .await
                }
                Err(error) => ResponseEnvelope::failure(request_id, error),
            };
            view.update(cx, |view, cx| {
                apply(view, response, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn dispatch_to_node(
        &self,
        cx: &mut Context<Self>,
        node_id: String,
        request: ClientRequest,
        apply: impl FnOnce(&mut Self, ResponseEnvelope, &mut Context<Self>) + 'static,
    ) {
        let request_envelope = RequestEnvelope::new(request);
        let request_id = request_envelope.request_id;
        let pending = self
            .node_backends
            .get(&node_id)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::NotFound,
                    format!("worker node {node_id} is not connected"),
                    false,
                )
            })
            .map(|backend| backend.submit(request_envelope));
        cx.spawn(async move |view, cx| {
            let response = match pending {
                Ok(pending) => {
                    cx.background_spawn(async move { pending.wait().await })
                        .await
                }
                Err(error) => ResponseEnvelope::failure(request_id, error),
            };
            view.update(cx, |view, cx| {
                apply(view, response, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn backend_for_request(
        &self,
        request: &ClientRequest,
    ) -> Result<BackendWorker, LoomError> {
        let Some(session_id) = session_id_for_request(request, self.active_session.id) else {
            return Ok(self.backend.clone());
        };
        self.backend_for_session(session_id)
    }

    pub(crate) fn backend_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<BackendWorker, LoomError> {
        let node_id = assigned_node_id(&self.session_node_ids, session_id)?;
        self.node_backends.get(node_id).cloned().ok_or_else(|| {
            LoomError::new(
                ErrorCode::NotFound,
                format!("assigned worker node {node_id} for session {session_id} is unavailable"),
                false,
            )
        })
    }

    pub(crate) fn record_status(&mut self, status: impl Into<String>) {
        self.timeline
            .push(TimelineItem::System(SystemNote::status(status.into())));
    }

    pub(crate) fn record_backend_error(&mut self, operation: &str, error: LoomError) {
        self.timeline.push(TimelineItem::System(SystemNote {
            tone: SystemTone::Error,
            heading: Some(format!("{operation} · {}", error.code)),
            text: error.message.clone(),
            retryable: error.retryable,
        }));
    }

    /// Refreshes the model list. The synchronous variant is only used during
    /// the startup bootstrap, before the window exists.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn refresh_models(&mut self) {
        let provider_ids = match list_provider_ids(&self.connection) {
            Ok(provider_ids) => provider_ids,
            Err(error) => {
                self.record_status(format!(
                    "Could not list providers for model refresh: {error}"
                ));
                return;
            }
        };
        let catalog = match list_models(&self.connection) {
            Ok(models) => models,
            Err(error) => {
                self.record_status(format!("Could not load available models: {error}"));
                return;
            }
        };
        let mut provider_names = catalog.provider_names;
        let mut models = catalog.models;
        for provider_id in provider_ids {
            let response = self.connection.request(RequestEnvelope::new(
                ClientRequest::DiscoverProviderModels {
                    provider_id: provider_id.clone(),
                },
            ));
            match response.result {
                Ok(ServerResponse::Models { models: discovered }) => {
                    for model in discovered {
                        provider_names.insert(
                            model.id.clone(),
                            crate::connection::provider_name_for_id(provider_id.as_str()),
                        );
                        models.push(model.id);
                    }
                }
                Err(error) => self.record_status(format!(
                    "Model discovery unavailable for {}: {}",
                    provider_id.as_str(),
                    error.message
                )),
                Ok(response) => self.record_backend_error(
                    "model discovery",
                    unexpected_response("model list", response),
                ),
            }
        }
        models.sort();
        models.dedup();
        self.node_model_provider_names
            .insert(self.default_backend_node_id.clone(), provider_names);
        self.apply_models(models);
    }

    pub(crate) fn refresh_models_for_node_async(
        &mut self,
        node_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.node_backends.get(&node_id).cloned() else {
            self.record_backend_error(
                "model refresh",
                LoomError::new(
                    ErrorCode::NotFound,
                    format!("worker node {node_id} is not connected"),
                    false,
                ),
            );
            cx.notify();
            return;
        };
        if !self.model_refreshes_in_flight.insert(node_id.clone()) {
            return;
        }
        let active_node_id = self
            .session_node_ids
            .get(&self.active_session.id)
            .map(String::as_str);
        if active_node_id == Some(node_id.as_str()) {
            self.models.clear();
            self.model_catalog_node_id = None;
            cx.notify();
        }
        cx.spawn(async move |view, cx| {
            let result = list_models_from_backend(&backend).await;
            let provider_response = backend
                .submit(RequestEnvelope::new(ClientRequest::ListProviders))
                .wait()
                .await;
            view.update(cx, |view, cx| {
                view.model_refreshes_in_flight.remove(&node_id);
                if view.providers_node_id.as_deref() == Some(node_id.as_str()) {
                    match provider_response.result {
                        Ok(ServerResponse::Providers { providers }) => {
                            view.providers = providers;
                        }
                        Err(error) => view.record_backend_error("list providers", error),
                        Ok(response) => view.record_backend_error(
                            "list providers",
                            unexpected_response("provider list", response),
                        ),
                    }
                }
                match result {
                    Ok(catalog) => {
                        view.record_model_discovery_errors(catalog.discovery_errors);
                        view.node_model_provider_names
                            .insert(node_id.clone(), catalog.provider_names);
                        let models = catalog.models;
                        view.node_model_catalogs
                            .insert(node_id.clone(), models.clone());
                        if view.default_backend_node_id == node_id {
                            view.default_models = models.clone();
                            #[cfg(target_family = "wasm")]
                            let preferred_model = view
                                .browser_model
                                .as_ref()
                                .filter(|model| models.contains(model))
                                .cloned();
                            #[cfg(not(target_family = "wasm"))]
                            let preferred_model = None;
                            if let Some(model) = preferred_model.or_else(|| {
                                if models.contains(&view.default_model) {
                                    None
                                } else {
                                    models.first().cloned()
                                }
                            }) {
                                view.default_model = model;
                            }
                        }
                        let active_node_id = view
                            .session_node_ids
                            .get(&view.active_session.id)
                            .map(String::as_str);
                        if active_node_id == Some(node_id.as_str()) {
                            view.models = models;
                            view.model_catalog_node_id = Some(node_id.clone());
                            view.record_status(format!(
                                "Loaded available models for {}",
                                view.node_names
                                    .get(&node_id)
                                    .map(String::as_str)
                                    .unwrap_or(node_id.as_str())
                            ));
                        }
                    }
                    Err(error) => {
                        view.record_backend_error("model refresh", error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn record_model_discovery_errors(
        &mut self,
        errors: Vec<crate::connection::ModelDiscoveryError>,
    ) {
        for discovery_error in errors {
            self.record_status(format!(
                "Model discovery unavailable for {}: {}",
                discovery_error.provider_id, discovery_error.error.message
            ));
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn apply_models(&mut self, models: Vec<ModelId>) {
        let node_id = self.default_backend_node_id.clone();
        self.node_model_catalogs
            .insert(node_id.clone(), models.clone());
        self.default_models = models.clone();
        let active_node_id = self
            .session_node_ids
            .get(&self.active_session.id)
            .map(String::as_str);
        if active_node_id == Some(node_id.as_str()) {
            self.models = models;
            self.model_catalog_node_id = Some(node_id);
        }
        self.record_status(format!(
            "Loaded {} available model{}",
            self.default_models.len(),
            if self.default_models.len() == 1 {
                ""
            } else {
                "s"
            }
        ));
    }

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

    pub(crate) fn schedule_project_poll(&mut self, cx: &mut Context<Self>) {
        if self.project_poll_scheduled
            || !self.project_root_is_active()
            || !self.project_has_live_children()
        {
            return;
        }
        self.project_poll_scheduled = true;
        cx.spawn(async move |view, cx| {
            #[cfg(target_family = "wasm")]
            {
                let promise = js_sys::Promise::new(&mut |resolve, _reject| {
                    if let Some(window) = web_sys::window() {
                        let _ = window
                            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 1000);
                    }
                });
                let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
            }
            #[cfg(not(target_family = "wasm"))]
            cx.background_spawn(async {
                std::thread::sleep(Duration::from_secs(1));
            })
            .await;
            view.update(cx, |view, cx| {
                if view.project_root_is_active() {
                    view.poll_project_once(cx);
                } else {
                    view.project_poll_scheduled = false;
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn poll_project_once(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project_snapshot.as_ref() else {
            self.project_poll_scheduled = false;
            return;
        };
        let project_id = project.project_id;
        let root_session_id = project.root_session_id;
        let workspace_id = self.active_session.workspace_id;
        let member_ids = project
            .agents
            .iter()
            .map(|agent| agent.session_id)
            .collect::<BTreeSet<_>>();
        let after_sequence = self.project_feed_after_sequence;
        let stream_epoch = self.project_feed_epoch.clone();
        self.dispatch(
            cx,
            ClientRequest::GetSessionEvents {
                session_id: None,
                workspace_id: Some(workspace_id),
                after_sequence,
                stream_epoch,
            },
            move |view, response, cx| {
                if view.active_session.id != root_session_id
                    || view.active_session.workspace_id != workspace_id
                {
                    view.project_poll_scheduled = false;
                    return;
                }
                view.project_poll_scheduled = false;
                match response.result {
                    Ok(ServerResponse::WorkspaceEvents {
                        workspace_id: response_workspace,
                        events,
                        stream_epoch,
                    }) if response_workspace == workspace_id => {
                        view.project_feed_epoch = stream_epoch;
                        if let Some(latest) = events.iter().map(workspace_feed_event_sequence).max()
                        {
                            view.project_feed_after_sequence = Some(
                                view.project_feed_after_sequence
                                    .map_or(latest, |current| current.max(latest)),
                            );
                        }
                        if events
                            .iter()
                            .any(|event| is_project_workspace_event(event, project_id, &member_ids))
                        {
                            view.project_snapshot_stale = true;
                            view.project_messages_stale = true;
                        }
                    }
                    Ok(ServerResponse::WorkspaceEventsSnapshot {
                        workspace_id: response_workspace,
                        events,
                        latest_sequence,
                        stream_epoch,
                        ..
                    }) if response_workspace == workspace_id => {
                        view.project_feed_epoch = stream_epoch;
                        view.project_feed_after_sequence = Some(latest_sequence);
                        view.project_snapshot_stale = true;
                        view.project_messages_stale = true;
                        if events
                            .iter()
                            .any(|event| is_project_workspace_event(event, project_id, &member_ids))
                        {
                            view.project_snapshot_stale = true;
                        }
                    }
                    Err(error) => view.record_backend_error("project event stream", error),
                    Ok(response) => view.record_backend_error(
                        "project event stream",
                        unexpected_response("project event stream", response),
                    ),
                }
                if view.project_snapshot_stale {
                    view.refresh_active_project_snapshot(cx);
                } else if view.project_messages_stale {
                    view.refresh_project_messages(cx);
                }
                view.schedule_project_poll(cx);
            },
        );
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn collect_events_since(
        &mut self,
        mut after_sequence: Option<EventSequence>,
        mut fallback: Option<AgentRunSnapshotProjection>,
    ) -> Result<(), LoomError> {
        let session_id = self.active_session.id;
        for resync_attempt in 0..=1 {
            let response =
                self.connection
                    .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                        session_id: Some(session_id),
                        workspace_id: None,
                        after_sequence,
                        stream_epoch: self.event_stream_epoch.clone(),
                    }));
            match response.result? {
                ServerResponse::SessionEvents {
                    events,
                    stream_epoch,
                } => {
                    self.reset_projection();
                    self.after_sequence = after_sequence;
                    self.event_stream_epoch = stream_epoch;
                    for event in events {
                        self.after_sequence = Some(event.sequence);
                        self.consume_event(&event.event);
                    }
                    if self.timeline.is_empty()
                        && let Some(projection) = fallback
                    {
                        self.apply_run_projection(projection);
                    }
                    return Ok(());
                }
                ServerResponse::SessionEventsSnapshot {
                    session,
                    events,
                    latest_sequence,
                    stream_epoch,
                    ..
                } if resync_attempt == 0 => {
                    self.event_stream_epoch = stream_epoch;
                    self.active_session = session;
                    let refreshed = self.connection.request(RequestEnvelope::new(
                        ClientRequest::GetAgentSessionInitialState { session_id },
                    ));
                    if let Ok(ServerResponse::AgentSessionInitialState(initial)) = refreshed.result
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
                ServerResponse::SessionEventsSnapshot {
                    session,
                    events,
                    latest_sequence,
                    stream_epoch,
                    ..
                } => {
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
        if self.timeline.is_empty()
            && let Some(projection) = fallback
        {
            self.apply_run_projection(projection);
        }
    }

    /// Applies newly journaled session events through the connection worker.
    pub(crate) fn poll_run_once(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::GetSessionEvents {
                session_id: Some(self.active_session.id),
                workspace_id: None,
                after_sequence: self.after_sequence,
                stream_epoch: self.event_stream_epoch.clone(),
            },
            |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::SessionEvents {
                        events,
                        stream_epoch,
                    }) => {
                        view.event_stream_epoch = stream_epoch;
                        for event in events {
                            view.after_sequence = Some(event.sequence);
                            view.consume_event(&event.event);
                        }
                    }
                    Ok(ServerResponse::SessionEventsSnapshot {
                        session,
                        events,
                        latest_sequence,
                        stream_epoch,
                        ..
                    }) => {
                        view.event_stream_epoch = stream_epoch;
                        view.active_session = session;
                        view.reset_projection();
                        view.after_sequence = Some(latest_sequence);
                        for event in events {
                            view.after_sequence = Some(event.sequence);
                            view.consume_event(&event.event);
                        }
                    }
                    Err(error) => view.record_backend_error("session event stream", error),
                    Ok(response) => view.record_backend_error(
                        "session event stream",
                        unexpected_response("session event stream", response),
                    ),
                }
                if view.project_snapshot_stale {
                    view.refresh_active_project_snapshot(cx);
                }
                if view.project_messages_stale {
                    view.refresh_project_messages(cx);
                }
                view.run_poll_scheduled = false;
                view.schedule_run_poll(cx);
            },
        );
    }

    pub(crate) fn run_is_active(&self) -> bool {
        matches!(
            self.run_state,
            Some(AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating)
        )
    }

    /// Polls the journal while a run is active. The protocol keeps the event
    /// stream resumable, so polling is sufficient for both local and remote
    /// connections without making the UI depend on a transport-specific push
    /// implementation.
    /// Whether the session event stream should keep being polled. A run that is
    /// parked on a durable project join reports `Paused`, but the server
    /// resumes it when its children finish, so polling must continue past the
    /// active states or the wait never appears to complete.
    pub(crate) fn run_should_poll(&self) -> bool {
        self.run_is_active() || matches!(self.run_state, Some(AgentRunState::Paused))
    }

    pub(crate) fn schedule_run_poll(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled || !self.run_should_poll() {
            return;
        }
        self.run_poll_scheduled = true;
        let active = self.run_is_active();
        cx.spawn(async move |view, cx| {
            let delay = if active { 250 } else { 1_000 };
            #[cfg(target_family = "wasm")]
            {
                let promise = js_sys::Promise::new(&mut |resolve, _reject| {
                    if let Some(window) = web_sys::window() {
                        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                            &resolve,
                            delay as i32,
                        );
                    }
                });
                let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
            }
            #[cfg(not(target_family = "wasm"))]
            cx.background_spawn(async move {
                std::thread::sleep(Duration::from_millis(delay));
            })
            .await;
            view.update(cx, |view, cx| {
                if view.run_should_poll() {
                    view.poll_run_once(cx);
                } else {
                    view.run_poll_scheduled = false;
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn start_run_polling(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled || self.active_run_id.is_none() {
            return;
        }
        self.run_poll_scheduled = true;
        self.poll_run_once(cx);
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

    pub(crate) fn consume_event(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::ProjectTaskUpdated { .. }
            | ServerEvent::ProjectChildWorktreeUpdated { .. }
            | ServerEvent::ProjectAgentCreated { .. }
            | ServerEvent::ProjectAgentUpdated { .. } => {
                self.project_snapshot_stale = true;
            }
            ServerEvent::ProjectAgentMessageAccepted { .. } => {
                self.project_messages_stale = true;
            }
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
            ServerEvent::AgentSessionRenamed { name, .. } => {
                self.active_session.name = name.clone();
                self.update_session_list();
            }
            ServerEvent::AgentSessionArchived { .. } => {
                self.session_state = AgentSessionState::Archived;
                self.active_session.state = AgentSessionState::Archived;
                self.update_session_list();
            }
            ServerEvent::Agent { event } => self.consume_agent_event(event),
            ServerEvent::SessionFilesystemChanged { change } => {
                self.record_status(format!("Workspace {:?}: {}", change.kind, change.path));
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
                if let Some(TimelineItem::Plan {
                    steps: existing,
                    completed,
                    active,
                }) = self
                    .timeline
                    .iter_mut()
                    .rev()
                    .find(|item| matches!(item, TimelineItem::Plan { .. }))
                {
                    *existing = steps;
                    completed.clear();
                    *active = None;
                } else {
                    self.timeline.push(TimelineItem::Plan {
                        steps,
                        completed: BTreeSet::new(),
                        active: None,
                    });
                }
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
                if let Some(TimelineItem::Plan { active, .. }) = self
                    .timeline
                    .iter_mut()
                    .rev()
                    .find(|item| matches!(item, TimelineItem::Plan { .. }))
                {
                    *active = Some(*index);
                }
            }
            AgentEvent::StepCompleted { index, .. } => {
                if let Some(TimelineItem::Plan {
                    completed, active, ..
                }) = self
                    .timeline
                    .iter_mut()
                    .rev()
                    .find(|item| matches!(item, TimelineItem::Plan { .. }))
                {
                    completed.insert(*index);
                    if active == &Some(*index) {
                        *active = None;
                    }
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
            AgentEvent::RunUsage { .. } | AgentEvent::RunUsageUpdated { .. } => {}
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

    pub(crate) fn apply_run_projection(&mut self, projection: AgentRunSnapshotProjection) {
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
            let mut timeline = timeline_items_from_messages(messages, activity_records.clone());
            if !projection.plan.is_empty() {
                timeline.insert(
                    0,
                    TimelineItem::Plan {
                        steps: projection
                            .plan
                            .into_iter()
                            .map(|step| step.description)
                            .collect(),
                        completed: BTreeSet::new(),
                        active: None,
                    },
                );
            }
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

    pub(crate) fn schedule_worker_node_poll(&mut self, cx: &mut Context<Self>) {
        let node_ids = self
            .worker_nodes
            .iter()
            .filter(|node| {
                node.connection.is_some() && !self.worker_node_polls_scheduled.contains(&node.id)
            })
            .map(|node| node.id)
            .collect::<Vec<_>>();
        for id in node_ids {
            self.worker_node_polls_scheduled.insert(id);
            cx.spawn(async move |view, cx| {
                #[cfg(target_family = "wasm")]
                {
                    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
                        if let Some(window) = web_sys::window() {
                            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                                &resolve, 10_000,
                            );
                        }
                    });
                    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                }
                #[cfg(not(target_family = "wasm"))]
                cx.background_spawn(async {
                    std::thread::sleep(Duration::from_secs(10));
                })
                .await;
                view.update(cx, |view, cx| view.poll_worker_node_once(id, cx))
                    .ok();
            })
            .detach();
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn reconnect_configured_worker_nodes(&mut self, cx: &mut Context<Self>) {
        let candidates = self
            .worker_nodes
            .iter()
            .filter(|node| {
                !node.is_local
                    && node.connection.is_none()
                    && node.connection_state != WorkerConnectionState::Connecting
            })
            .filter_map(|node| node.url.as_ref().map(|url| (node.id, url.clone())))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return;
        }
        let workspace_id = self.workspace_id;
        for (id, url) in candidates {
            if worker_url_embeds_credential(&url) {
                self.set_worker_node_connection_failure(
                    id,
                    &url,
                    "This saved URL contains credentials. Remove them from the URL and reconnect with the token in the separate access-token field.".to_owned(),
                );
                continue;
            }
            if let Some(node) = self.worker_nodes.iter_mut().find(|node| node.id == id) {
                node.connection_state = WorkerConnectionState::Connecting;
                node.connection_detail = None;
            }
            let candidate_url = url.clone();
            cx.spawn(async move |view, cx| {
                let result = cx
                    .background_spawn(async move {
                        let credentials = PeerCredentialStore::new();
                        let token = match credentials.get(workspace_id, &candidate_url) {
                            Ok(Some(token)) => token,
                            Ok(None) => {
                                return Err((
                                    WorkerConnectionStage::CredentialRead,
                                    LoomError::new(
                                        ErrorCode::AuthenticationRequired,
                                        "no saved worker credential was found",
                                        false,
                                    ),
                                    false,
                                ));
                            }
                            Err(error) => {
                                return Err((WorkerConnectionStage::CredentialRead, error, false));
                            }
                        };
                        let connection = ClientConnection::remote(candidate_url.clone(), token)
                            .map_err(|error| (WorkerConnectionStage::Transport, error, false))?;
                        if let Err(error) = negotiate(&connection) {
                            let cleanup_failed = connection.close().is_err();
                            return Err((
                                WorkerConnectionStage::Negotiation,
                                error,
                                cleanup_failed,
                            ));
                        }
                        let status = match worker_node_status(&connection) {
                            Ok(status) => status,
                            Err(error) => {
                                let cleanup_failed = connection.close().is_err();
                                return Err((WorkerConnectionStage::Status, error, cleanup_failed));
                            }
                        };
                        Ok::<_, (WorkerConnectionStage, LoomError, bool)>((
                            connection,
                            status,
                            candidate_url,
                        ))
                    })
                    .await;
                view.update(cx, |view, cx| {
                    let is_pending = view.worker_nodes.iter().any(|node| {
                        node.id == id
                            && !node.is_local
                            && node.url.as_deref() == Some(url.as_str())
                            && node.connection.is_none()
                    });
                    match result {
                        Ok((connection, status, connected_url)) if is_pending => {
                            view.attach_reconnected_worker_node(
                                id,
                                &connected_url,
                                connection,
                                status,
                                cx,
                            );
                        }
                        Ok((connection, _, _)) => {
                            cx.spawn(async move |view, cx| {
                                let result =
                                    cx.background_spawn(async move { connection.close() })                                    .await;
                                        if result.is_err() {
                                            view.update(cx, |view, cx| {
                                                view.record_status(
                                                    "A stale worker connection could not be closed cleanly.",
                                                );
                                                cx.notify();
                                            })
                                            .ok();
                                        }
                            })
                            .detach();
                        }
                        Err((stage, error, cleanup_failed)) if is_pending => {
                            view.fail_worker_node_connection(
                                id,
                                &url,
                                stage,
                                &error,
                                None,
                                cleanup_failed,
                            );
                        }
                        Err(_) => {}
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    pub(crate) fn poll_worker_node_once(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(connection) = self
            .worker_nodes
            .iter()
            .find(|node| node.id == id)
            .and_then(|node| node.connection.clone())
        else {
            self.worker_node_polls_scheduled.remove(&id);
            return;
        };

        cx.spawn(async move |view, cx| {
            #[cfg(not(target_family = "wasm"))]
            let results = cx
                .background_spawn(async move { worker_node_status(&connection) })
                .await;
            #[cfg(target_family = "wasm")]
            let results = worker_node_status_async(&connection).await;

            view.update(cx, |view, cx| {
                let mut recovered = false;
                if let Some(node_index) = view.worker_nodes.iter().position(|node| node.id == id) {
                    let status_message = match results {
                        Ok(status) => {
                            recovered =
                                !view.worker_nodes[node_index].status.online && status.online;
                            view.worker_nodes[node_index].severe_load_streak =
                                next_severe_load_streak(
                                    view.worker_nodes[node_index].severe_load_streak,
                                    &status.resources,
                                );
                            update_worker_node_status(&mut view.worker_nodes, id, status)
                        }
                        Err(_) => {
                            let node = &mut view.worker_nodes[node_index];
                            node.severe_load_streak = 0;
                            if node.status.online {
                                node.status.online = false;
                                Some(format!("Worker node {} is unavailable", node.status.name))
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(message) = status_message {
                        view.record_status(message);
                    }
                    let node = &view.worker_nodes[node_index];
                    view.node_names
                        .insert(node.status.node_id.clone(), worker_node_display_name(node));
                }
                view.worker_node_polls_scheduled.remove(&id);
                view.schedule_worker_node_poll(cx);
                if recovered {
                    view.reload_sessions(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn connect_worker_node(&mut self, cx: &mut Context<Self>) {
        let value = self
            .node_input_state
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_else(|| self.node_input_initial.clone())
            .trim()
            .to_owned();
        let mut parts = value.split_whitespace();
        let Some(url) = parts.next() else {
            self.record_status("Enter a node URL followed by its access token");
            cx.notify();
            return;
        };
        let url = url.to_owned();
        let id = match self.begin_worker_node_connection(&url) {
            Ok(id) => id,
            Err(message) => {
                self.record_status(message);
                cx.notify();
                return;
            }
        };
        if worker_url_embeds_credential(&url) {
            self.set_worker_node_connection_failure(
                id,
                &url,
                "Do not include credentials in the URL. Enter the worker access token after the URL."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        let Some(token) = parts.next().map(str::to_owned) else {
            self.set_worker_node_connection_failure(
                id,
                &url,
                worker_connection_failure_detail(
                    WorkerConnectionStage::InputValidation,
                    &LoomError::invalid_request("missing access token"),
                    None,
                ),
            );
            cx.notify();
            return;
        };
        let workspace_id = self.workspace_id;
        let submitted_value = value;
        let node_url = url.clone();
        let connect_url = url.clone();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_spawn(async move {
                    let connection =
                        ClientConnection::remote(connect_url, token.clone())
                        .map_err(|error| (WorkerConnectionStage::Transport, error, false))?;
                    if let Err(error) = negotiate(&connection) {
                        let cleanup_failed = connection.close().is_err();
                        return Err((
                            WorkerConnectionStage::Negotiation,
                            error,
                            cleanup_failed,
                        ));
                    }
                    let status = match worker_node_status(&connection) {
                        Ok(status) => status,
                        Err(error) => {
                            let cleanup_failed = connection.close().is_err();
                            return Err((
                                WorkerConnectionStage::Status,
                                error,
                                cleanup_failed,
                            ));
                        }
                    };
                    let credential_detail = PeerCredentialStore::new()
                        .set(workspace_id, &node_url, &token)
                        .err()
                        .map(|error| {
                            worker_connection_failure_detail(
                                WorkerConnectionStage::CredentialSave,
                                &error,
                                Some(&token),
                            )
                        });
                    Ok::<_, (WorkerConnectionStage, LoomError, bool)>((
                        connection,
                        status,
                        node_url,
                        credential_detail,
                    ))
                })
                .await;
            view.update(cx, |view, cx| {
                let is_pending = view.worker_nodes.iter().any(|node| {
                    node.id == id
                        && !node.is_local
                        && node.url.as_deref() == Some(url.as_str())
                        && node.connection_state == WorkerConnectionState::Connecting
                });
                match result {
                    Ok((connection, status, connected_url, credential_detail)) if is_pending => {
                        view.add_worker_node(
                            connection,
                            status,
                            connected_url,
                            credential_detail,
                            cx,
                        );
                        if view
                            .node_input_state
                            .as_ref()
                            .is_some_and(|input| input.read(cx).value().as_ref() == submitted_value)
                        {
                            view.clear_node_on_render = true;
                        }
                    }
                    Ok((connection, _, _, _)) => {
                        if let Err(error) = connection.close() {
                            view.record_status(format!(
                                "A completed worker connection was discarded, but the transport could not be closed: {}",
                                worker_connection_failure_detail(
                                    WorkerConnectionStage::Transport,
                                    &error,
                                    None,
                                )
                            ));
                        }
                    }
                    Err((stage, error, cleanup_failed)) if is_pending => {
                        view.fail_worker_node_connection(
                            id,
                            &url,
                            stage,
                            &error,
                            None,
                            cleanup_failed,
                        );
                    }
                    Err(_) => {}
                }
                cx.notify();
            })
        })
        .detach();
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn connect_worker_node(&mut self, cx: &mut Context<Self>) {
        let value = self
            .node_input_state
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_owned();
        let mut parts = value.split_whitespace();
        let Some(url) = parts.next() else {
            self.record_status("Enter a node URL followed by its access token");
            cx.notify();
            return;
        };
        let url = url.to_owned();
        let id = match self.begin_worker_node_connection(&url) {
            Ok(id) => id,
            Err(message) => {
                self.record_status(message);
                cx.notify();
                return;
            }
        };
        if worker_url_embeds_credential(&url) {
            self.set_worker_node_connection_failure(
                id,
                &url,
                "Do not include credentials in the URL. Enter the worker access token after the URL."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        let Some(token) = parts.next().map(str::to_owned) else {
            self.set_worker_node_connection_failure(
                id,
                &url,
                worker_connection_failure_detail(
                    WorkerConnectionStage::InputValidation,
                    &LoomError::invalid_request("missing access token"),
                    None,
                ),
            );
            cx.notify();
            return;
        };
        let submitted_value = value;
        if !self.connected {
            self.browser_startup_error = None;
            self.connect_browser_bootstrap(id, url, token, submitted_value, cx);
            return;
        }
        let node_url = url.clone();
        cx.notify();
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let result = async {
                let connection = ClientConnection::browser(&url, &token)
                    .map_err(|error| (WorkerConnectionStage::Transport, error, false))?;
                if let Err(error) = negotiate_async(&connection).await {
                    let cleanup_failed = connection.close().is_err();
                    return Err((
                        WorkerConnectionStage::Negotiation,
                        error,
                        cleanup_failed,
                    ));
                }
                let status = match worker_node_status_async(&connection).await {
                    Ok(status) => status,
                    Err(error) => {
                        let cleanup_failed = connection.close().is_err();
                        return Err((
                            WorkerConnectionStage::Status,
                            error,
                            cleanup_failed,
                        ));
                    }
                };
                Ok::<_, (WorkerConnectionStage, LoomError, bool)>((connection, status, node_url))
            }
            .await;
            view.update(cx, |view, cx| {
                let is_pending = view.worker_nodes.iter().any(|node| {
                    node.id == id
                        && !node.is_local
                        && node.url.as_deref() == Some(url.as_str())
                        && node.connection_state == WorkerConnectionState::Connecting
                });
                match result {
                    Ok((connection, status, connected_url)) if is_pending => {
                        view.add_worker_node(connection, status, connected_url, None, cx);
                        if view
                            .node_input_state
                            .as_ref()
                            .is_some_and(|input| input.read(cx).value().as_ref() == submitted_value)
                        {
                            view.clear_node_on_render = true;
                        }
                    }
                    Ok((connection, _, _)) => {
                        if let Err(error) = connection.close() {
                            view.record_status(format!(
                                "A completed worker connection was discarded, but the transport could not be closed: {}",
                                worker_connection_failure_detail(
                                    WorkerConnectionStage::Transport,
                                    &error,
                                    None,
                                )
                            ));
                        }
                    }
                    Err((stage, error, cleanup_failed)) if is_pending => {
                        view.fail_worker_node_connection(
                            id,
                            &url,
                            stage,
                            &error,
                            Some(&token),
                            cleanup_failed,
                        );
                    }
                    Err(_) => {}
                }
                cx.notify();
            })
        })
        .detach();
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn connect_browser_bootstrap(
        &mut self,
        id: u64,
        url: String,
        token: String,
        submitted_value: String,
        cx: &mut Context<Self>,
    ) {
        let options = BrowserOptions::from_connection(
            url.clone(),
            token.clone(),
            self.browser_workspace.clone(),
            self.browser_model.clone(),
        );
        let focus_handle = self.composer_focus_handle.clone();
        let view = cx.entity();
        cx.notify();
        cx.spawn(async move |_, cx| {
            let result = LoomView::try_new_browser(&options, focus_handle).await;
            view.update(cx, |view, cx| {
                let is_pending = view.worker_nodes.iter().any(|node| {
                    node.id == id
                        && node.url.as_deref() == Some(url.as_str())
                        && node.connection_state == WorkerConnectionState::Connecting
                });
                match result {
                    Ok(mut initialized) if is_pending => {
                        let active_session = initialized.active_session.clone();
                        initialized.settings_open = false;
                        initialized.browser_window_initialized = false;
                        *view = initialized;
                        view.reload_sessions(cx);
                        if !view.sessions.is_empty() {
                            view.select_session(active_session, cx);
                        }
                        if view
                            .node_input_state
                            .as_ref()
                            .is_some_and(|input| input.read(cx).value().as_ref() == submitted_value)
                        {
                            view.clear_node_on_render = true;
                        }
                    }
                    Ok(initialized) => {
                        if let Err(error) = initialized.connection.close() {
                            log::error!("could not close a stale bootstrap connection: {error}");
                        }
                    }
                    Err(error) if is_pending => {
                        view.fail_worker_node_connection(
                            id,
                            &url,
                            WorkerConnectionStage::Bootstrap,
                            &error,
                            Some(&token),
                            false,
                        );
                    }
                    Err(_) => {}
                }
                cx.notify();
            })
        })
        .detach();
    }
}
