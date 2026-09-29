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
