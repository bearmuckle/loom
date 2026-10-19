use super::*;
#[cfg(not(target_family = "wasm"))]
use crate::connection::resume_github_identity;
#[cfg(target_family = "wasm")]
use crate::connection::resume_github_identity_async;
use crate::preferences::UiPreferences;

/// Resolves and starts the managed `loom-server` worker that replaces the
/// backend the client used to embed. It keeps the same shared state database,
/// and `--reset-state` is forwarded so an incompatible database can be wiped.
#[cfg(not(target_family = "wasm"))]
fn start_local_worker(options: &UiOptions) -> Result<loom_local::ManagedWorker, LoomError> {
    let binary = loom_local::worker::resolve_server_binary(options.server_binary.clone())?;
    let persistence = backend_persistence_path();
    let token_file = loom_core::state_dir()
        .join("local-worker")
        .join(format!("token-{}", std::process::id()));
    loom_local::ManagedWorker::start(&loom_local::ManagedWorkerOptions {
        binary,
        persistence,
        reset_state: options.reset_state,
        token_file,
        port: None,
    })
}

impl LoomView {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn try_new(
        options: &UiOptions,
        focus_handle: FocusHandle,
    ) -> Result<Self, LoomError> {
        info!("bootstrapping backend connection");
        // A remote worker without a token is an unsigned connection: it is
        // accepted as pre-login and signs in with GitHub instead.
        let mut unsigned = false;
        let (connection, workspace_root, owned_backend) = if let Some(remote_url) = &options.remote
        {
            info!("connecting to remote backend");
            if worker_url_embeds_credential(remote_url) {
                return Err(LoomError::invalid_request(
                    "remote URL must not contain credentials; provide the access token separately",
                ));
            }
            let token = options.token.clone().unwrap_or_default();
            unsigned = token.trim().is_empty();
            let connection = ClientConnection::remote(
                remote_url.clone(),
                token,
                &remote_connection_options(options),
            )?;
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
            (connection, workspace_root, None)
        } else {
            let workspace_root = prepare_workspace(options)?;
            info!("using workspace '{}'", workspace_root.display());
            let worker = start_local_worker(options)?;
            let connection = ClientConnection::remote(
                worker.websocket_url(),
                worker.token().to_owned(),
                &loom_local::RemoteConnectionOptions::default(),
            )?;
            (
                connection,
                workspace_root,
                Some(crate::local_backend::OwnedBackend::managed(worker)),
            )
        };
        let mut remote_cleanup_guard = Some(ConnectionCleanupGuard::new(connection.clone()));
        info!("worker transport connected; negotiating protocol");
        let negotiation = negotiate(&connection)?;
        let server_protocol_version = Some(negotiation.protocol_version);
        let backend_endpoint = options.remote.clone();
        let remote_options = remote_connection_options(options);
        let node_status = worker_node_status(&connection)?;
        let participant = current_participant(&connection)?;
        if !unsigned
            && participant.is_none()
            && let Some(saved) = crate::identity_store::load(&node_status.node_id)
        {
            match resume_github_identity(&connection, saved.id) {
                Ok(Some(_)) => {}
                // The worker no longer knows this person; ask again on the
                // next explicit sign-in instead of retrying forever.
                Ok(None) => crate::identity_store::clear(&node_status.node_id),
                Err(error) => log::debug!(
                    "[loom-ui] could not resume the GitHub identity for worker '{}': {}",
                    node_status.node_id,
                    error.message
                ),
            }
        }
        let pre_login = unsigned && participant.is_none();
        let mut view = if pre_login {
            info!(
                "worker accepted the connection without a credential; waiting for a GitHub \
                 sign-in"
            );
            Self::new_pre_login(
                connection.clone(),
                node_status,
                WorkspaceConfig::default(),
                server_protocol_version,
                backend_endpoint,
                negotiation
                    .capabilities
                    .contains(loom_core::Capability::GitHubSignIn),
                options.remote.is_none(),
                false,
                focus_handle,
            )
        } else {
            Self::initialize_from_connection(
                options,
                connection,
                workspace_root,
                false,
                remote_cleanup_guard.take(),
                focus_handle,
                true,
                server_protocol_version,
                backend_endpoint,
            )?
        };
        if let Some(guard) = &mut remote_cleanup_guard {
            guard.disarm();
        }
        view.remote_options = remote_options;
        view.github_sign_in_available = negotiation
            .capabilities
            .contains(loom_core::Capability::GitHubSignIn);
        view.owned_backend = owned_backend;
        Ok(view)
    }

    /// The bootstrap constructor threads the negotiated backend metadata into
    /// the view alongside the transport details, so it takes one argument per
    /// startup concern rather than a bag struct.
    #[allow(clippy::too_many_arguments)]
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn initialize_from_connection(
        options: &UiOptions,
        connection: ClientConnection,
        workspace_root: PathBuf,
        demo_workspace: bool,
        mut remote_cleanup_guard: Option<ConnectionCleanupGuard>,
        focus_handle: FocusHandle,
        discover_models: bool,
        server_protocol_version: Option<ProtocolVersion>,
        backend_endpoint: Option<String>,
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
                let response = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
                    FilesystemRequest::AttachSessionDirectory {
                        session_id: session.id,
                        source: workspace_root.display().to_string(),
                        path: "sources/local".to_owned(),
                    },
                )));
                match response.result? {
                    ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached {
                        ..
                    }) => {}
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
        let model = preferred_model_choice(&models, &options.model);
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
        let preferences = UiPreferences::load();
        let mut view = Self {
            backend,
            connection: connection.clone(),
            #[cfg(not(target_family = "wasm"))]
            owned_backend: None,
            server_protocol_version,
            backend_endpoint,
            remote_options: remote_connection_options(options),
            browser_client: false,
            default_backend_node_id: default_backend_node_id.clone(),
            node_backends,
            node_names,
            session_node_ids,
            workspace_id,
            workspaces,
            node_workspaces: BTreeMap::new(),
            local_directory_sources_available: options.remote.is_none(),
            unsigned_nodes: BTreeSet::new(),
            awaiting_sign_in_nodes: BTreeSet::new(),
            github_sign_in_available: false,
            github_login_node_id: None,
            local_current_directory: (options.remote.is_none()
                && !demo_workspace
                && !workspace_root.as_os_str().is_empty())
            .then(|| workspace_root.clone()),
            sessions: if has_session {
                vec![session.clone()]
            } else {
                Vec::new()
            },
            project_snapshot: None,
            project_tree_snapshots: Vec::new(),
            project_child_review: None,
            project_snapshot_stale: false,
            project_snapshot_refreshing: None,
            missing_project_snapshots_refreshing: false,
            project_feed_after_sequence: None,
            project_feed_epoch: None,
            project_poll_scheduled: false,
            project_list: None,
            project_list_subscription: None,
            project_list_view: None,
            project_list_active: None,
            session_sources: BTreeMap::new(),
            session_source_lookups: BTreeMap::new(),
            session_source_request_in_flight: false,
            node_session_load_errors: BTreeMap::new(),
            active_session: session.clone(),
            active_run: run.clone(),
            active_run_id: run.as_ref().map(|run| run.id),
            context_inspection: None,
            default_model: model.clone(),
            session_models: BTreeMap::new(),
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            ask_before_destructive_actions: false,
            session_ask_before_destructive_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            status_banner: None,
            reconnecting: None,
            connectivity_attempt_in_flight: false,
            status_details_open: false,
            model,
            models: models.clone(),
            default_models: models,
            node_model_catalogs,
            node_model_provider_names: BTreeMap::from([(
                default_backend_node_id.clone(),
                model_provider_names,
            )]),
            model_catalog_node_id: Some(default_backend_node_id.clone()),
            archived_sessions: ArchivedSessionsState::default(),
            model_refreshes_in_flight: BTreeSet::new(),
            model_catalog_refreshed_at: BTreeMap::new(),
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
            covering_surface: None,
            providers: Vec::new(),
            providers_node_id: None,
            provider_api_key_inputs: BTreeMap::new(),
            provider_setup_status: BTreeMap::new(),
            theme_choice: preferences.theme_choice,
            font_scale_percent: preferences.font_scale_percent,
            show_reasoning: preferences.show_reasoning,
            output_detail: preferences.output_detail,
            appearance_subscription: None,
            shortcut_interceptor: None,
            after_sequence: None,
            event_stream_epoch: None,
            timeline: Vec::new(),
            transcript_before_ordinal: None,
            transcript_loaded_ordinals: BTreeSet::new(),
            transcript_messages: BTreeMap::new(),
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            transcript_prepend_count: 0,
            timeline_view: None,
            plan: None,
            plan_collapsed: false,
            activity_records: BTreeMap::new(),
            expanded_tools: BTreeMap::new(),
            expanded_tool_groups: BTreeMap::new(),
            expanded_tool_usage: BTreeMap::new(),
            expanded_reasoning: BTreeMap::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            collaboration: crate::collaboration::CollaborationState::default(),
            composer_input: None,
            composer_placeholder: None,
            composer_completion: None,
            pending_completion_accept: false,
            suppress_completion_once: false,
            command_palette_open: false,
            command_palette_input: None,
            command_palette_selection: 0,
            session_filter_input: None,
            review_filter_input: None,
            input_subscriptions: Vec::new(),
            clear_composer_on_render: false,
            clear_node_on_render: false,
            access_note_input: None,
            access_deny_inputs: BTreeMap::new(),
            clear_access_note: false,
            clear_access_deny_reason: None,
            rename_input_state: None,
            sub_project_dialog: None,
            sub_project_input_state: None,
            source_path_input: None,
            repository_filter_input: None,
            cloned_repositories: Vec::new(),
            cloned_repositories_loading: false,
            repository_search_generation: 0,
            repository_search_query: String::new(),
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
            project_delete_dialog: None,
            source_dialog: None,
            #[cfg(target_family = "wasm")]
            welcome_dialog_dismissed: false,
            demo_workspace,
            login_enabled: true,
            github_connected: false,
            github_repository_connected: false,
            github_write_access: false,
            github_login: None,
            github_login_kind: GitHubLoginKind::Copilot,
            next_worker_node_id: worker_nodes.len() as u64,
            worker_nodes,
            worker_node_polls_scheduled: BTreeSet::new(),
            workspace_config,
            node_input_initial: String::new(),
            node_input_state: None,
            run_poll_scheduled: false,
            browser_startup_error: None,
            browser_starting: false,
            browser_connection_lost: None,
        };
        if discover_models {
            view.refresh_models();
        }
        view.refresh_sessions()?;
        let active_session = view.active_session.clone();
        if has_session {
            view.load_session(active_session);
        }
        // Cover every project so delegated sub-tasks nest in the sidebar tree
        // from the first frame, not just the active project's.
        view.refresh_project_snapshots();
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
        let preferences = UiPreferences::load();

        Self {
            connected: demo_mode,
            unsigned_nodes: BTreeSet::new(),
            awaiting_sign_in_nodes: BTreeSet::new(),
            github_sign_in_available: false,
            github_login_node_id: None,
            browser_demo_mode: demo_mode,
            backend,
            connection,
            server_protocol_version: None,
            backend_endpoint: (!options.remote().is_empty()).then(|| options.remote().to_owned()),
            browser_client: true,
            default_backend_node_id: String::new(),
            node_backends: BTreeMap::new(),
            node_names: BTreeMap::new(),
            session_node_ids: BTreeMap::new(),
            workspace_id,
            workspaces: Vec::new(),
            node_workspaces: BTreeMap::new(),
            local_directory_sources_available: false,
            local_current_directory: None,
            sessions: if demo_mode {
                vec![active_session.clone()]
            } else {
                Vec::new()
            },
            project_snapshot: None,
            project_tree_snapshots: Vec::new(),
            project_child_review: None,
            project_snapshot_stale: false,
            project_snapshot_refreshing: None,
            missing_project_snapshots_refreshing: false,
            project_feed_after_sequence: None,
            project_feed_epoch: None,
            project_poll_scheduled: false,
            project_list: None,
            project_list_subscription: None,
            project_list_view: None,
            project_list_active: None,
            session_sources: BTreeMap::new(),
            session_source_lookups: BTreeMap::new(),
            session_source_request_in_flight: false,
            node_session_load_errors: BTreeMap::new(),
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
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            ask_before_destructive_actions: false,
            session_ask_before_destructive_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            status_banner: None,
            reconnecting: None,
            connectivity_attempt_in_flight: false,
            status_details_open: false,
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
            model_catalog_refreshed_at: BTreeMap::new(),
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
            // An unconfigured client opens on the settings dialog, which is the
            // covering surface on screen until the user closes it.
            covering_surface: options
                .is_configured()
                .then_some(CoveringSurface::Settings(SettingsSection::Agents)),
            archived_sessions: ArchivedSessionsState::default(),
            providers: Vec::new(),
            providers_node_id: None,
            provider_api_key_inputs: BTreeMap::new(),
            provider_setup_status: BTreeMap::new(),
            theme_choice: preferences.theme_choice,
            font_scale_percent: preferences.font_scale_percent,
            show_reasoning: preferences.show_reasoning,
            output_detail: preferences.output_detail,
            appearance_subscription: None,
            shortcut_interceptor: None,
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
            transcript_prepend_count: 0,
            timeline_view: None,
            plan: None,
            plan_collapsed: false,
            activity_records: BTreeMap::new(),
            expanded_tools: BTreeMap::new(),
            expanded_tool_groups: BTreeMap::new(),
            expanded_tool_usage: BTreeMap::new(),
            expanded_reasoning: BTreeMap::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            collaboration: crate::collaboration::CollaborationState::default(),
            composer_input: None,
            composer_placeholder: None,
            composer_completion: None,
            pending_completion_accept: false,
            suppress_completion_once: false,
            command_palette_open: false,
            command_palette_input: None,
            command_palette_selection: 0,
            session_filter_input: None,
            review_filter_input: None,
            input_subscriptions: Vec::new(),
            clear_composer_on_render: false,
            clear_node_on_render: false,
            access_note_input: None,
            access_deny_inputs: BTreeMap::new(),
            clear_access_note: false,
            clear_access_deny_reason: None,
            rename_input_state: None,
            sub_project_dialog: None,
            sub_project_input_state: None,
            source_path_input: None,
            repository_filter_input: None,
            cloned_repositories: Vec::new(),
            cloned_repositories_loading: false,
            repository_search_generation: 0,
            repository_search_query: String::new(),
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
            project_delete_dialog: None,
            source_dialog: None,
            #[cfg(target_family = "wasm")]
            welcome_dialog_dismissed: false,
            demo_workspace: demo_mode,
            login_enabled: false,
            github_connected: false,
            github_repository_connected: false,
            github_write_access: false,
            github_login: None,
            github_login_kind: GitHubLoginKind::Copilot,
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
            visibility_subscription: None,
            browser_resume_in_flight: false,
            browser_startup_error: startup_error,
            browser_starting: options.is_configured() && !options.demo(),
            browser_connection_lost: None,
        }
    }

    /// Builds the view for a worker that accepted the connection without a
    /// credential.
    ///
    /// Such a connection is pre-login: the worker refuses the workspace,
    /// session, provider, and collaboration reads until a person signs in, so
    /// the view starts with no workspace state and shows the sign-in prompt in
    /// place of the composer. Signing in then runs the same load path a
    /// successful connect uses.
    #[allow(clippy::too_many_arguments)]
    fn new_pre_login(
        connection: ClientConnection,
        node_status: WorkerNodeStatus,
        workspace_config: WorkspaceConfig,
        server_protocol_version: Option<ProtocolVersion>,
        backend_endpoint: Option<String>,
        github_sign_in_available: bool,
        local_directory_sources_available: bool,
        browser_client: bool,
        focus_handle: FocusHandle,
    ) -> Self {
        let default_backend_node_id = node_status.node_id.clone();
        let worker_nodes = initial_worker_nodes(
            node_status,
            connection.clone(),
            &workspace_config,
            backend_endpoint.as_deref(),
        );
        let backend = BackendWorker::spawn(connection.clone());
        let node_backends = BTreeMap::from([(default_backend_node_id.clone(), backend.clone())]);
        let node_names = worker_nodes
            .iter()
            .map(|node| (node.status.node_id.clone(), worker_node_display_name(node)))
            .collect();
        let workspace_id = WorkspaceId::new();
        let active_session = empty_session_snapshot(workspace_id);
        let model = ModelId::new("default");
        let preferences = UiPreferences::load();
        let mut unsigned_nodes = BTreeSet::new();
        unsigned_nodes.insert(default_backend_node_id.clone());
        let mut awaiting_sign_in_nodes = BTreeSet::new();
        awaiting_sign_in_nodes.insert(default_backend_node_id.clone());
        let mut collaboration = crate::collaboration::CollaborationState::default();
        // The identity answer is known to be "no person": the worker accepted
        // the upgrade without a credential and refuses everything else.
        collaboration.identity_loaded = true;
        collaboration.support = crate::collaboration::CollaborationSupport::Supported;
        Self {
            #[cfg(target_family = "wasm")]
            connected: true,
            #[cfg(target_family = "wasm")]
            browser_demo_mode: false,
            connection: connection.clone(),
            #[cfg(not(target_family = "wasm"))]
            owned_backend: None,
            server_protocol_version,
            backend_endpoint,
            #[cfg(not(target_family = "wasm"))]
            remote_options: RemoteConnectionOptions::default(),
            browser_client,
            backend,
            default_backend_node_id: default_backend_node_id.clone(),
            node_backends,
            node_names,
            session_node_ids: BTreeMap::new(),
            workspace_id,
            workspaces: Vec::new(),
            node_workspaces: BTreeMap::new(),
            local_directory_sources_available,
            local_current_directory: None,
            sessions: Vec::new(),
            project_snapshot: None,
            project_tree_snapshots: Vec::new(),
            project_child_review: None,
            project_snapshot_stale: false,
            project_snapshot_refreshing: None,
            missing_project_snapshots_refreshing: false,
            project_feed_after_sequence: None,
            project_feed_epoch: None,
            project_poll_scheduled: false,
            project_list: None,
            project_list_subscription: None,
            project_list_view: None,
            project_list_active: None,
            session_sources: BTreeMap::new(),
            session_source_lookups: BTreeMap::new(),
            session_source_request_in_flight: false,
            node_session_load_errors: BTreeMap::new(),
            active_session: active_session.clone(),
            active_run: None,
            active_run_id: None,
            context_inspection: None,
            default_model: model.clone(),
            session_models: BTreeMap::new(),
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            ask_before_destructive_actions: false,
            session_ask_before_destructive_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            status_banner: None,
            reconnecting: None,
            connectivity_attempt_in_flight: false,
            status_details_open: false,
            model: model.clone(),
            models: vec![model.clone()],
            default_models: vec![model.clone()],
            node_model_catalogs: BTreeMap::from([(default_backend_node_id.clone(), vec![model])]),
            node_model_provider_names: BTreeMap::new(),
            model_catalog_node_id: Some(default_backend_node_id.clone()),
            model_refreshes_in_flight: BTreeSet::new(),
            model_catalog_refreshed_at: BTreeMap::new(),
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
            covering_surface: None,
            providers: Vec::new(),
            providers_node_id: None,
            provider_api_key_inputs: BTreeMap::new(),
            provider_setup_status: BTreeMap::new(),
            theme_choice: preferences.theme_choice,
            font_scale_percent: preferences.font_scale_percent,
            show_reasoning: preferences.show_reasoning,
            output_detail: preferences.output_detail,
            appearance_subscription: None,
            shortcut_interceptor: None,
            after_sequence: None,
            event_stream_epoch: None,
            timeline: Vec::new(),
            transcript_before_ordinal: None,
            transcript_loaded_ordinals: BTreeSet::new(),
            transcript_messages: BTreeMap::new(),
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            transcript_prepend_count: 0,
            timeline_view: None,
            plan: None,
            plan_collapsed: false,
            activity_records: BTreeMap::new(),
            expanded_tools: BTreeMap::new(),
            expanded_tool_groups: BTreeMap::new(),
            expanded_tool_usage: BTreeMap::new(),
            expanded_reasoning: BTreeMap::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            archived_sessions: ArchivedSessionsState::default(),
            pending_approval: None,
            pending_input: None,
            collaboration,
            unsigned_nodes,
            awaiting_sign_in_nodes,
            github_sign_in_available,
            github_login_node_id: None,
            composer_input: None,
            composer_placeholder: None,
            composer_completion: None,
            pending_completion_accept: false,
            suppress_completion_once: false,
            command_palette_open: false,
            command_palette_input: None,
            command_palette_selection: 0,
            session_filter_input: None,
            review_filter_input: None,
            input_subscriptions: Vec::new(),
            clear_composer_on_render: false,
            clear_node_on_render: false,
            access_note_input: None,
            access_deny_inputs: BTreeMap::new(),
            clear_access_note: false,
            clear_access_deny_reason: None,
            rename_input_state: None,
            sub_project_dialog: None,
            sub_project_input_state: None,
            source_path_input: None,
            repository_filter_input: None,
            cloned_repositories: Vec::new(),
            cloned_repositories_loading: false,
            repository_search_generation: 0,
            repository_search_query: String::new(),
            pending_source_path: None,
            composer_focus_handle: focus_handle,
            session_state: active_session.state,
            run_state: None,
            review: ReviewState::default(),
            session_repositories: Vec::new(),
            session_directories: Vec::new(),
            selected_repository_id: None,
            session_drawer_open: false,
            rename_dialog: None,
            project_delete_dialog: None,
            source_dialog: None,
            #[cfg(target_family = "wasm")]
            welcome_dialog_dismissed: false,
            demo_workspace: false,
            login_enabled: true,
            github_connected: false,
            github_repository_connected: false,
            github_write_access: false,
            github_login: None,
            github_login_kind: GitHubLoginKind::Copilot,
            next_worker_node_id: worker_nodes.len() as u64,
            worker_nodes,
            worker_node_polls_scheduled: BTreeSet::new(),
            workspace_config,
            node_input_initial: String::new(),
            node_input_state: None,
            run_poll_scheduled: false,
            #[cfg(target_family = "wasm")]
            browser_workspace: None,
            #[cfg(target_family = "wasm")]
            browser_model: None,
            #[cfg(target_family = "wasm")]
            browser_window_initialized: false,
            #[cfg(target_family = "wasm")]
            visibility_subscription: None,
            #[cfg(target_family = "wasm")]
            browser_resume_in_flight: false,
            browser_startup_error: None,
            browser_starting: false,
            browser_connection_lost: None,
        }
    }

    /// Builds the view for the browser client: connects to a remote backend
    /// over the in-page WebSocket transport and resolves the same project /
    /// session / model state that native's remote-mode bootstrap resolves,
    /// using the `_async` request helpers since nothing may block the page's
    /// single JS thread.
    ///
    /// Everything the first workspace frame shows is resolved here, including
    /// every workspace's sessions, the project snapshots that group them, and
    /// the sources of the project the client lands on. The caller still opens
    /// that session once the view is mounted, because the conversation load
    /// needs a `Context`, but the view it mounts already describes the
    /// workspace it is about to show.
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
        let negotiation = negotiate_async(&connection).await?;
        let node_status = worker_node_status_async(&connection).await?;
        let github_sign_in_available = negotiation
            .capabilities
            .contains(loom_core::Capability::GitHubSignIn);
        // A page that names a worker but no token connects unsigned: the worker
        // accepts it as pre-login, so there is no workspace to load until the
        // person signs in.
        let unsigned = options.token().trim().is_empty();
        let participant = current_participant_async(&connection).await?;
        if !unsigned && participant.is_none() {
            if let Some(saved) = crate::identity_store::load(&node_status.node_id) {
                match resume_github_identity_async(&connection, saved.id).await {
                    Ok(Some(_)) => {}
                    Ok(None) => crate::identity_store::clear(&node_status.node_id),
                    Err(error) => log::debug!(
                        "[loom-ui] could not resume the GitHub identity for worker '{}': {}",
                        node_status.node_id,
                        error.message
                    ),
                }
            }
        }
        let pre_login = unsigned && participant.is_none();
        if pre_login {
            if let Err(error) = options.persist_connection() {
                log::warn!("[loom-ui] could not save the bootstrap worker config: {error}");
            }
            cleanup_guard.disarm();
            return Ok(Self::new_pre_login(
                connection,
                node_status,
                WorkspaceConfig::default(),
                Some(negotiation.protocol_version),
                (!options.remote().is_empty()).then(|| options.remote().to_owned()),
                github_sign_in_available,
                false,
                true,
                focus_handle,
            ));
        }
        let default_backend_node_id = node_status.node_id.clone();
        let mut workspaces = list_workspaces_async(&connection).await?;
        let workspace = match active_workspace(&workspaces) {
            Some(workspace) => workspace.clone(),
            None => {
                let workspace = create_workspace_async(&connection, "Default").await?;
                workspaces.push(workspace.clone());
                workspace
            }
        };
        let workspace_id = workspace.id;
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
        // A worker keeps its projects under its own workspaces, so the list is
        // read from all of them: reading only the home workspace made the rest
        // of the sidebar appear a moment after the workspace did.
        let mut sessions = Vec::new();
        for workspace in &workspaces {
            sessions.extend(list_workspace_sessions_async(&connection, workspace.id).await?);
        }
        // Newest activity first, which is the order the landing project is
        // chosen from; the sidebar orders its own rows.
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        let workspace_root = options.workspace().map(PathBuf::from).unwrap_or_default();
        // A project is created only for a page that named a source, and only
        // when the worker has none at all; otherwise the client opens what is
        // already there.
        if sessions.is_empty() && options.workspace().is_some() {
            let created = create_session_in_workspace_async(
                &connection,
                workspace_id,
                session_name_for_path(&workspace_root)
                    .as_deref()
                    .unwrap_or("New session"),
            )
            .await?;
            if !workspace_root.as_os_str().is_empty() {
                attach_session_repository_async(
                    &connection,
                    created.id,
                    &workspace_root.display().to_string(),
                    "repo",
                )
                .await?;
            }
            sessions.push(created);
        }
        // One request per project covers its root and every delegated agent, so
        // loading these before the first frame keeps a sub-agent session from
        // painting as a project of its own and then moving under its project.
        let projects = load_project_snapshots(&connection, &sessions).await;
        // The most recently active session is often a delegated sub-agent, so
        // the client opens the project that owns it rather than its transcript.
        let session = landing_session(&sessions, &projects)
            .unwrap_or_else(|| empty_session_snapshot(workspace_id));
        let project_snapshot = projects
            .iter()
            .find(|project| project.root_session_id == session.id)
            .cloned();
        // The header and the landing project's row show the session's sources,
        // so they are read with the rest of the startup state instead of
        // appearing after the first frame.
        let (session_repositories, session_directories) =
            load_session_sources(&connection, session.id).await;
        let selected_repository_id = session_repositories.first().map(|repository| repository.id);
        let session_sources = session_source_label(&session_repositories, &session_directories)
            .map(|label| BTreeMap::from([(session.id, label)]))
            .unwrap_or_default();
        let model_catalog = list_models_async(&connection).await?;
        let models = model_catalog.models;
        let model_provider_names = model_catalog.provider_names;
        let model = options
            .model()
            .map(|model| preferred_model_choice(&models, model))
            .or_else(|| models.first().cloned())
            .unwrap_or_else(|| ModelId::new("default"));
        let node_model_catalogs =
            BTreeMap::from([(default_backend_node_id.clone(), models.clone())]);
        let backend = BackendWorker::spawn(connection.clone());
        let node_backends = BTreeMap::from([(default_backend_node_id.clone(), backend.clone())]);
        let session_node_ids = sessions
            .iter()
            .map(|session| (session.id, default_backend_node_id.clone()))
            .collect();
        let node_names = worker_nodes
            .iter()
            .map(|node| (node.status.node_id.clone(), worker_node_display_name(node)))
            .collect();

        let preferences = UiPreferences::load();
        let view = Self {
            connected: true,
            unsigned_nodes: BTreeSet::new(),
            awaiting_sign_in_nodes: BTreeSet::new(),
            github_sign_in_available,
            github_login_node_id: None,
            browser_demo_mode: false,
            backend,
            connection: connection.clone(),
            server_protocol_version: Some(negotiation.protocol_version),
            backend_endpoint: (!options.remote().is_empty()).then(|| options.remote().to_owned()),
            browser_client: true,
            default_backend_node_id: default_backend_node_id.clone(),
            node_backends,
            node_names,
            session_node_ids,
            workspace_id,
            workspaces,
            node_workspaces: BTreeMap::new(),
            local_directory_sources_available: false,
            local_current_directory: None,
            sessions,
            project_snapshot,
            project_tree_snapshots: projects,
            project_child_review: None,
            project_snapshot_stale: false,
            project_snapshot_refreshing: None,
            missing_project_snapshots_refreshing: false,
            project_feed_after_sequence: None,
            project_feed_epoch: None,
            project_poll_scheduled: false,
            project_list: None,
            project_list_subscription: None,
            project_list_view: None,
            project_list_active: None,
            session_sources,
            session_source_lookups: BTreeMap::new(),
            session_source_request_in_flight: false,
            node_session_load_errors: BTreeMap::new(),
            active_session: session.clone(),
            active_run: None,
            active_run_id: None,
            context_inspection: None,
            default_model: model.clone(),
            session_models: BTreeMap::new(),
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            ask_before_destructive_actions: false,
            session_ask_before_destructive_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            status_banner: None,
            reconnecting: None,
            connectivity_attempt_in_flight: false,
            status_details_open: false,
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
            model_catalog_refreshed_at: BTreeMap::new(),
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
            covering_surface: None,
            providers: Vec::new(),
            providers_node_id: None,
            provider_api_key_inputs: BTreeMap::new(),
            provider_setup_status: BTreeMap::new(),
            theme_choice: preferences.theme_choice,
            font_scale_percent: preferences.font_scale_percent,
            show_reasoning: preferences.show_reasoning,
            output_detail: preferences.output_detail,
            appearance_subscription: None,
            shortcut_interceptor: None,
            after_sequence: None,
            event_stream_epoch: None,
            timeline: Vec::new(),
            transcript_before_ordinal: None,
            transcript_loaded_ordinals: BTreeSet::new(),
            transcript_messages: BTreeMap::new(),
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            transcript_prepend_count: 0,
            timeline_view: None,
            plan: None,
            plan_collapsed: false,
            activity_records: BTreeMap::new(),
            expanded_tools: BTreeMap::new(),
            expanded_tool_groups: BTreeMap::new(),
            expanded_tool_usage: BTreeMap::new(),
            expanded_reasoning: BTreeMap::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            collaboration: crate::collaboration::CollaborationState::default(),
            composer_input: None,
            composer_placeholder: None,
            composer_completion: None,
            pending_completion_accept: false,
            suppress_completion_once: false,
            command_palette_open: false,
            command_palette_input: None,
            command_palette_selection: 0,
            session_filter_input: None,
            review_filter_input: None,
            input_subscriptions: Vec::new(),
            clear_composer_on_render: false,
            clear_node_on_render: false,
            access_note_input: None,
            access_deny_inputs: BTreeMap::new(),
            clear_access_note: false,
            clear_access_deny_reason: None,
            rename_input_state: None,
            sub_project_dialog: None,
            sub_project_input_state: None,
            source_path_input: None,
            repository_filter_input: None,
            cloned_repositories: Vec::new(),
            cloned_repositories_loading: false,
            repository_search_generation: 0,
            repository_search_query: String::new(),
            pending_source_path: None,
            composer_focus_handle: focus_handle,
            session_state: session.state,
            run_state: None,
            review: ReviewState::default(),
            session_repositories,
            session_directories,
            selected_repository_id,
            session_drawer_open: false,
            rename_dialog: None,
            project_delete_dialog: None,
            source_dialog: None,
            #[cfg(target_family = "wasm")]
            welcome_dialog_dismissed: false,
            demo_workspace: false,
            archived_sessions: ArchivedSessionsState::default(),
            login_enabled: true,
            github_connected: false,
            github_repository_connected: false,
            github_write_access: false,
            github_login: None,
            github_login_kind: GitHubLoginKind::Copilot,
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
            visibility_subscription: None,
            browser_resume_in_flight: false,
            browser_startup_error: None,
            browser_starting: false,
            browser_connection_lost: None,
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
            let answered = response.result.is_ok();
            if let Err(error) = view.update(cx, |view, cx| {
                apply(view, response, cx);
                // The reconnecting state follows the answers rather than the
                // requests: an answer ends it, and a failure that did not come
                // back starts or advances the automatic reconnect.
                if answered {
                    view.note_worker_answered(cx);
                }
                #[cfg(target_family = "wasm")]
                view.detect_browser_connection_loss(cx);
                view.schedule_connectivity_retry(cx);
                cx.notify();
            }) {
                log::warn!("[loom-ui] applying a backend response failed: {error}");
            }
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
            let answered = response.result.is_ok();
            if let Err(error) = view.update(cx, |view, cx| {
                apply(view, response, cx);
                if answered {
                    view.note_worker_answered(cx);
                }
                #[cfg(target_family = "wasm")]
                view.detect_browser_connection_loss(cx);
                view.schedule_connectivity_retry(cx);
                cx.notify();
            }) {
                log::warn!("[loom-ui] applying a backend response failed: {error}");
            }
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
            self.record_status("Enter a worker URL, optionally followed by its access token");
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
        // The token is optional: without one the client connects unsigned and
        // signs in with GitHub, and with one it is an operator connection that
        // works exactly as before.
        let token = parts.next().map(str::to_owned).unwrap_or_default();
        let workspace_id = self.workspace_id;
        let remote_options = self.remote_options.clone();
        let submitted_value = value;
        let node_url = url.clone();
        let connect_url = url.clone();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_spawn(async move {
                    let unsigned = token.trim().is_empty();
                    let connection = ClientConnection::remote(
                        connect_url,
                        token.clone(),
                        &remote_options,
                    )
                    .map_err(|error| (WorkerConnectionStage::Transport, error, false))?;
                    let negotiation = match negotiate(&connection) {
                        Ok(negotiation) => negotiation,
                        Err(error) => {
                            let cleanup_failed = connection.close().is_err();
                            return Err((
                                WorkerConnectionStage::Negotiation,
                                error,
                                cleanup_failed,
                            ));
                        }
                    };
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
                    // A worker that accepted the upgrade without a credential
                    // is pre-login: it reports no person and refuses the rest
                    // until someone signs in.
                    let pre_login = unsigned
                        && current_participant(&connection)
                            .map(|participant| participant.is_none())
                            .unwrap_or(false);
                    if !unsigned
                        && let Some(saved) = crate::identity_store::load(&status.node_id)
                    {
                        match resume_github_identity(&connection, saved.id) {
                            Ok(Some(_)) => {}
                            Ok(None) => crate::identity_store::clear(&status.node_id),
                            Err(error) => log::debug!(
                                "[loom-ui] could not resume the GitHub identity for worker '{}': \
                                 {}",
                                status.node_id,
                                error.message
                            ),
                        }
                    }
                    let credentials = PeerCredentialStore::new();
                    let credential_result = if token.is_empty() {
                        credentials.delete(workspace_id, &node_url)
                    } else {
                        credentials.set(workspace_id, &node_url, &token)
                    };
                    let credential_detail = credential_result.err().map(|error| {
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
                        pre_login,
                        negotiation
                            .capabilities
                            .contains(loom_core::Capability::GitHubSignIn),
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
                    Ok((
                        connection,
                        status,
                        connected_url,
                        credential_detail,
                        pre_login,
                        offers_sign_in,
                    )) if is_pending => {
                        view.github_sign_in_available |= offers_sign_in;
                        // Marked before the node is added, because adding it
                        // reloads every connected worker's sessions and a
                        // pre-login connection is refused that read.
                        if pre_login {
                            view.mark_node_pre_login(status.node_id.clone());
                        }
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
                    Ok((connection, _, _, _, _, _)) => {
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
        // Only a bootstrap this call actually starts may hold the startup
        // screen; a validation refusal below must leave the client on the
        // settings screen instead of a connecting screen that never connects.
        self.browser_starting = false;
        let mut parts = value.split_whitespace();
        let Some(url) = parts.next() else {
            self.record_status("Enter a worker URL, optionally followed by its access token");
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
        // Without a token the browser connects unsigned and signs in with
        // GitHub; with one it is an operator connection as before.
        let token = parts.next().map(str::to_owned).unwrap_or_default();
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
                let unsigned = token.trim().is_empty();
                let connection = ClientConnection::browser(&url, &token)
                    .map_err(|error| (WorkerConnectionStage::Transport, error, false))?;
                let negotiation = match negotiate_async(&connection).await {
                    Ok(negotiation) => negotiation,
                    Err(error) => {
                        let cleanup_failed = connection.close().is_err();
                        return Err((
                            WorkerConnectionStage::Negotiation,
                            error,
                            cleanup_failed,
                        ));
                    }
                };
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
                let pre_login = unsigned
                    && current_participant_async(&connection)
                        .await
                        .map(|participant| participant.is_none())
                        .unwrap_or(false);
                if !unsigned
                    && let Some(saved) = crate::identity_store::load(&status.node_id)
                {
                    match resume_github_identity_async(&connection, saved.id).await {
                        Ok(Some(_)) => {}
                        Ok(None) => crate::identity_store::clear(&status.node_id),
                        Err(error) => log::debug!(
                            "[loom-ui] could not resume the GitHub identity for worker '{}': {}",
                            status.node_id,
                            error.message
                        ),
                    }
                }
                Ok::<_, (WorkerConnectionStage, LoomError, bool)>((
                    connection,
                    status,
                    node_url,
                    pre_login,
                    negotiation
                        .capabilities
                        .contains(loom_core::Capability::GitHubSignIn),
                ))
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
                    Ok((connection, status, connected_url, pre_login, offers_sign_in))
                        if is_pending =>
                    {
                        view.github_sign_in_available |= offers_sign_in;
                        // Marked before the node is added, because adding it
                        // reloads every connected worker's sessions and a
                        // pre-login connection is refused that read.
                        if pre_login {
                            view.mark_node_pre_login(status.node_id.clone());
                        }
                        view.add_worker_node(connection, status, connected_url, None, cx);
                        if view
                            .node_input_state
                            .as_ref()
                            .is_some_and(|input| input.read(cx).value().as_ref() == submitted_value)
                        {
                            view.clear_node_on_render = true;
                        }
                    }
                    Ok((connection, _, _, _, _)) => {
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
        // The startup screen covers the bootstrap until the landing project is
        // on screen, whether it was started automatically or from Settings.
        self.browser_starting = true;
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
                        initialized.covering_surface = None;
                        initialized.browser_window_initialized = false;
                        // Keep the startup screen until the landing project's
                        // conversation is applied, so the workspace is revealed
                        // in one paint instead of growing row by row.
                        initialized.browser_starting = !initialized.sessions.is_empty();
                        *view = initialized;
                        if !view.sessions.is_empty() {
                            view.open_browser_landing_session(active_session, cx);
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
                        // The bootstrap is over, so the startup screen gives way
                        // to the disconnected screen and its worker settings.
                        view.browser_starting = false;
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

    /// Detects a closed browser transport and swaps the live workspace for a
    /// reconnect screen. Safe to call after any request and on each poll, so a
    /// dropped socket is never mistaken for a working connection.
    #[cfg(target_family = "wasm")]
    pub(crate) fn detect_browser_connection_loss(&mut self, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        if let Some(reason) = self.backend.connection_closed_reason() {
            self.on_browser_connection_lost(reason, cx);
        }
    }

    /// Registers the page-visibility observer that resumes the worker
    /// connection once the browser tab becomes visible again. Called from the
    /// wasm-only window-initialization block; a no-op once registered.
    #[cfg(target_family = "wasm")]
    pub(crate) fn observe_browser_visibility(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.visibility_subscription.is_some() {
            return;
        }
        let view = cx.entity().downgrade();
        self.visibility_subscription = Some(window.observe_window_visibility(
            move |visibility, _window, cx| {
                if !visibility.is_visible() {
                    return;
                }
                let _ = view.update(cx, |view, cx| view.resume_browser_connection(cx));
            },
        ));
    }

    /// Restores the browser connection after the page became visible again.
    ///
    /// A close the browser reported while the tab was hidden is folded in
    /// first, because nothing observed it at the time, and then the recorded
    /// state decides between reconnecting, confirming a socket that still
    /// looks open, and doing nothing.
    #[cfg(target_family = "wasm")]
    fn resume_browser_connection(&mut self, cx: &mut Context<Self>) {
        self.detect_browser_connection_loss(cx);
        let configured = match BrowserOptions::from_location() {
            Ok(options) => options.is_configured(),
            Err(error) => {
                log::warn!("[loom-ui] could not read saved worker settings: {error}");
                false
            }
        };
        match browser_resumption(
            self.browser_demo_mode,
            self.connected,
            self.browser_connection_lost.is_some(),
            configured,
        ) {
            BrowserResumption::Ignore => {}
            BrowserResumption::Reconnect => self.reconnect_browser(cx),
            BrowserResumption::Verify => self.verify_browser_connection(cx),
        }
    }

    /// Probes a connection the view still believes is open, and reconnects when
    /// the probe fails. A socket that died while the tab was hidden reports no
    /// close, so without the probe every later request would wait out its
    /// timeout before the view noticed.
    #[cfg(target_family = "wasm")]
    fn verify_browser_connection(&mut self, cx: &mut Context<Self>) {
        if self.browser_resume_in_flight {
            return;
        }
        self.browser_resume_in_flight = true;
        let connection = self.connection.clone();
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let result = probe_worker_connection(&connection).await;
            view.update(cx, |view, cx| {
                view.browser_resume_in_flight = false;
                match result {
                    Ok(()) => log::debug!("[loom-ui] the worker answered the resume probe"),
                    Err(error) => {
                        log::warn!("[loom-ui] the worker did not answer the resume probe: {error}");
                        view.record_status("Worker did not answer; reconnecting");
                        view.on_browser_connection_lost(error.to_string(), cx);
                        view.reconnect_browser(cx);
                    }
                }
            });
        })
        .detach();
    }

    #[cfg(target_family = "wasm")]
    fn on_browser_connection_lost(&mut self, reason: String, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        self.connected = false;
        // The reason is a transport string: it is logged here and then kept for
        // the reconnect screen's details, so the log does not need a copy.
        log::warn!(
            "[loom-ui] worker connection lost ({reason}); showing the reconnect screen and reconnecting automatically"
        );
        self.browser_connection_lost = Some(reason);
        self.run_poll_scheduled = false;
        self.project_poll_scheduled = false;
        self.worker_node_polls_scheduled.clear();
        cx.notify();
    }

    /// Reconnects the browser client to the saved worker by re-running the same
    /// bootstrap as a first connect, so projects, sessions, and models reload
    /// from the backend instead of from client state that may be stale. Refuses
    /// to start a second bootstrap while one is already in flight.
    #[cfg(target_family = "wasm")]
    pub(crate) fn reconnect_browser(&mut self, cx: &mut Context<Self>) {
        if self.browser_resume_in_flight {
            log::debug!("[loom-ui] a browser reconnect attempt is already in flight");
            return;
        }
        let options = match BrowserOptions::from_location() {
            Ok(options) if options.is_configured() && !options.demo() => options,
            Ok(_) => {
                self.browser_connection_lost = None;
                self.open_settings_from_menu(cx);
                self.record_status("Enter a worker URL to reconnect");
                cx.notify();
                return;
            }
            Err(error) => {
                self.record_status(format!("Could not read saved worker settings: {error}"));
                cx.notify();
                return;
            }
        };
        let focus_handle = self.composer_focus_handle.clone();
        self.browser_resume_in_flight = true;
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let result = LoomView::try_new_browser(&options, focus_handle).await;
            view.update(cx, |view, cx| match result {
                Ok(mut initialized) => {
                    let active_session = initialized.active_session.clone();
                    initialized.covering_surface = None;
                    initialized.browser_window_initialized = false;
                    // The reconnect holds the startup screen until the landing
                    // project is on screen, exactly like the first connect.
                    initialized.browser_starting = !initialized.sessions.is_empty();
                    *view = initialized;
                    if !view.sessions.is_empty() {
                        view.open_browser_landing_session(active_session, cx);
                    }
                }
                Err(error) => {
                    view.browser_resume_in_flight = false;
                    view.connected = false;
                    view.browser_starting = false;
                    // The raw reason stays as the reconnect screen's details;
                    // the notice itself is plain language and keeps the
                    // automatic attempts running.
                    view.browser_connection_lost = Some(error.to_string());
                    if let Some(failure) = classify_connectivity_failure(&error) {
                        view.record_connectivity_failure(
                            "reconnect to the worker",
                            failure,
                            &error,
                        );
                    } else {
                        view.record_backend_error("reconnect to the worker", error);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Ends the browser startup screen once the landing project is on screen.
    ///
    /// The screen stays up from the first connect until the landing session's
    /// conversation is applied, so the workspace is never revealed with a
    /// half-loaded project list or an empty transcript. Native builds never set
    /// the state; the hook is shared so the load paths that end it need no cfg.
    pub(crate) fn finish_browser_startup(&mut self) {
        self.browser_starting = false;
    }
}

/// Loads the project snapshot of every session no loaded snapshot covers.
///
/// A returned snapshot covers its root and every delegated agent, so a project
/// costs one request and the sessions it covers are skipped. This mirrors the
/// native bootstrap's `refresh_project_snapshots`, but awaits each answer since
/// nothing may block the page.
#[cfg(target_family = "wasm")]
async fn load_project_snapshots(
    connection: &ClientConnection,
    sessions: &[AgentSessionSnapshot],
) -> Vec<loom_core::ProjectSnapshot> {
    let mut projects = Vec::new();
    let mut attempted = BTreeSet::new();
    // A failed request covers nothing, so each session is attempted once. The
    // loop re-reads the uncovered set after every answer.
    while let Some(session_id) = uncovered_session_ids(sessions, &projects)
        .into_iter()
        .find(|session_id| !attempted.contains(session_id))
    {
        attempted.insert(session_id);
        let response = connection
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::GetProjectSnapshotForSession { session_id },
            )))
            .await;
        let Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot))) =
            response.result
        else {
            continue;
        };
        projects.retain(|known| known.project_id != snapshot.project_id);
        projects.push(snapshot);
    }
    projects
}

/// The landing session's repositories and directories, read with the rest of
/// the startup state so the header and the project row are complete on the
/// first frame.
#[cfg(target_family = "wasm")]
async fn load_session_sources(
    connection: &ClientConnection,
    session_id: AgentSessionId,
) -> (Vec<SessionRepository>, Vec<SessionDirectory>) {
    let repositories = connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::ListSessionRepositories { session_id },
        )))
        .await;
    let repositories = match repositories.result {
        Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
            repositories,
        })) => repositories,
        Err(error) => {
            log::warn!("[loom-ui] could not load the landing session repositories: {error}");
            Vec::new()
        }
        Ok(response) => {
            log::warn!("[loom-ui] unexpected landing session repository response: {response:?}");
            Vec::new()
        }
    };
    let directories = connection
        .request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ListSessionDirectories { session_id },
        )))
        .await;
    let directories = match directories.result {
        Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectories { directories })) => {
            directories
        }
        Err(error) => {
            log::warn!("[loom-ui] could not load the landing session directories: {error}");
            Vec::new()
        }
        Ok(response) => {
            log::warn!("[loom-ui] unexpected landing session directory response: {response:?}");
            Vec::new()
        }
    };
    (repositories, directories)
}

/// The project row's source label from a session's repositories or, when it has
/// none, its folders. A repository outranks a folder, matching the interactive
/// source discovery.
#[cfg(any(target_family = "wasm", test))]
fn session_source_label(
    repositories: &[SessionRepository],
    directories: &[SessionDirectory],
) -> Option<String> {
    repositories
        .first()
        .map(|repository| source_context_label(&repository.source))
        .or_else(|| {
            directories
                .first()
                .map(|directory| source_context_label(&directory.source))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The landing project's row reads a repository when it has one, because a
    /// checkout outranks the folder that holds it.
    #[test]
    fn the_session_source_label_prefers_a_repository_over_a_folder() {
        let repository = SessionRepository {
            id: RepositoryId::new(),
            source: "https://github.com/bearmuckle/loom.git".to_owned(),
            path: "sources/repo".to_owned(),
            revision: None,
            attached_at: Timestamp::from_unix_millis(1),
        };
        let directory = SessionDirectory {
            source: "/home/user/project".to_owned(),
            path: "sources/project".to_owned(),
        };

        assert_eq!(
            session_source_label(
                std::slice::from_ref(&repository),
                std::slice::from_ref(&directory),
            ),
            Some("bearmuckle/loom".to_owned())
        );
        assert_eq!(
            session_source_label(&[], std::slice::from_ref(&directory)),
            Some("user/project".to_owned())
        );
        assert_eq!(session_source_label(&[], &[]), None);
    }
}
