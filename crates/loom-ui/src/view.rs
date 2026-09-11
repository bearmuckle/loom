//! The GPUI view: session navigator, run canvas, composer, and review drawer.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::PathBuf,
    sync::atomic::Ordering,
    time::Duration,
};

use gpui::{
    App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Decorations, Element,
    EntityInputHandler, FocusHandle, Focusable, HitboxBehavior, MouseButton, MouseDownEvent,
    Pixels, Point, Render, ResizeEdge, Tiling, UTF16Selection, Window, WindowAppearance,
    WindowControlArea, canvas, div, point, prelude::*, px, transparent_black,
};
use gpui_base::TextSelectionLayer;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{
    Icon, IconName, Sizable,
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    text::TextView,
};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, ErrorCode, EventSequence, LoomError,
    ProjectId, RunId,
};
use loom_model::{MessageRole, ModelId, ToolCall};
use loom_protocol::{
    AgentEvent, AgentRunSnapshot, AgentRunSnapshotProjection, AgentRunState, ClientRequest,
    ProjectSnapshot, RequestEnvelope, ResponseEnvelope, ServerEvent, ServerResponse, TaskSnapshot,
    TaskStatus,
};
#[cfg(not(target_family = "wasm"))]
use loom_providers::{
    CredentialRef, CredentialStore, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF,
    GITHUB_COPILOT_DEFAULT_MODEL, GitHubCopilotAuthenticator, GitHubDeviceCode,
};
#[cfg(not(target_family = "wasm"))]
use loom_server::InProcessBackend;

use crate::{
    MAX_REVIEW_CHANGES, MAX_REVIEW_DIFF,
    connection::{BackendWorker, ClientConnection, select_remote_project, unexpected_response},
    state::{
        AgentMode, BackendStatus, GitHubLoginState, RenameDialogState, ReviewPanel, ReviewState,
        ThemeChoice, TimelineItem, bounded, bounded_to, run_state_name, session_state_for_run,
        session_state_name, session_title_from_task,
    },
    text_input::{
        Backspace, Copy, Delete, End, Home, InputField, Left, LoomTooltip, Paste, Right, SelectAll,
        Submit, TextBufferState, TextInputElement,
    },
    theme::{
        CLIENT_DECORATION_SHADOW, ClientCorners, DARK_THEME_ACTIVE, change_color, resize_edge, rgb,
        state_color,
    },
};
#[cfg(target_family = "wasm")]
use crate::{
    browser::BrowserOptions,
    connection::{
        create_session_async, list_models_async, list_projects_async, list_sessions_async,
        negotiate_async, open_workspace_async,
    },
};
#[cfg(not(target_family = "wasm"))]
use crate::{
    connection::{
        create_session, list_models, list_projects, list_provider_ids, list_sessions, negotiate,
        open_workspace, start_run,
    },
    platform::{UiOptions, backend_persistence_path, prepare_workspace, stable_project_id},
};

/// Opens a URL in a new tab/window. Natively this shells out to the OS's
/// "open" handler; in the browser it's just `window.open`.
#[cfg(not(target_family = "wasm"))]
fn open_external_url(url: &str) -> Result<(), std::io::Error> {
    open::that(url)
}

#[cfg(target_family = "wasm")]
fn open_external_url(url: &str) -> Result<(), std::io::Error> {
    let opened = web_sys::window().and_then(|window| window.open_with_url(url).ok().flatten());
    if opened.is_some() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "the browser blocked opening a new tab",
        ))
    }
}

pub(crate) struct LoomView {
    /// Used for the synchronous bootstrap before the window exists.
    pub(crate) connection: ClientConnection,
    /// Used for every request made once the view is interactive.
    pub(crate) backend: BackendWorker,
    pub(crate) project_id: ProjectId,
    pub(crate) project: Option<ProjectSnapshot>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) projects: Vec<ProjectSnapshot>,
    pub(crate) sessions: Vec<AgentSessionSnapshot>,
    pub(crate) active_session: AgentSessionSnapshot,
    pub(crate) active_run: Option<AgentRunSnapshot>,
    pub(crate) active_run_id: Option<RunId>,
    pub(crate) model: ModelId,
    pub(crate) default_model: ModelId,
    pub(crate) session_models: BTreeMap<AgentSessionId, ModelId>,
    pub(crate) agent_mode: AgentMode,
    pub(crate) agent_mode_picker_open: bool,
    pub(crate) session_task_cache: BTreeMap<AgentSessionId, String>,
    pub(crate) optimistic_messages: Vec<String>,
    pub(crate) sending_message: bool,
    pub(crate) models: Vec<ModelId>,
    pub(crate) model_picker_open: bool,
    pub(crate) settings_open: bool,
    pub(crate) theme_choice: ThemeChoice,
    pub(crate) dark_theme: bool,
    pub(crate) after_sequence: Option<EventSequence>,
    pub(crate) timeline: Vec<TimelineItem>,
    pub(crate) pending_approval: Option<ToolCall>,
    pub(crate) pending_input: Option<String>,
    pub(crate) composer: TextBufferState,
    pub(crate) composer_focus_handle: FocusHandle,
    pub(crate) input_field: InputField,
    pub(crate) session_state: AgentSessionState,
    pub(crate) run_state: Option<AgentRunState>,
    pub(crate) summary: Option<String>,
    pub(crate) review: ReviewState,
    pub(crate) tasks: Vec<TaskSnapshot>,
    pub(crate) rename_dialog: Option<RenameDialogState>,
    pub(crate) rename_focus_handle: FocusHandle,
    pub(crate) backend_status: BackendStatus,
    pub(crate) demo_workspace: bool,
    pub(crate) login_enabled: bool,
    pub(crate) github_connected: bool,
    pub(crate) github_login: Option<GitHubLoginState>,
    pub(crate) run_poll_scheduled: bool,
}

impl LoomView {
    pub(crate) fn input_state(&self, field: InputField) -> Option<&TextBufferState> {
        match field {
            InputField::Composer => Some(&self.composer),
            InputField::Rename => self.rename_dialog.as_ref().map(|dialog| &dialog.input),
        }
    }

    pub(crate) fn input_state_mut(&mut self, field: InputField) -> Option<&mut TextBufferState> {
        match field {
            InputField::Composer => Some(&mut self.composer),
            InputField::Rename => self.rename_dialog.as_mut().map(|dialog| &mut dialog.input),
        }
    }

    pub(crate) fn input_focus_handle(&self, field: InputField) -> FocusHandle {
        match field {
            InputField::Composer => self.composer_focus_handle.clone(),
            InputField::Rename => self.rename_focus_handle.clone(),
        }
    }

    pub(crate) fn edit_input(&self) -> &TextBufferState {
        self.input_state(self.input_field)
            .expect("focused input field is present")
    }

    pub(crate) fn edit_input_mut(&mut self) -> &mut TextBufferState {
        let field = self.input_field;
        self.input_state_mut(field)
            .expect("focused input field is present")
    }
}

impl EntityInputHandler for LoomView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let input = self.input_state(self.input_field)?;
        let range = input.range_from_utf16(&range_utf16);
        actual_range.replace(input.range_to_utf16(&range));
        input.text.get(range).map(str::to_owned)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let input = self.input_state(self.input_field)?;
        Some(UTF16Selection {
            range: input.range_to_utf16(&input.selected_range),
            reversed: input.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.input_state(self.input_field)?
            .marked_range
            .as_ref()
            .map(|range| {
                self.input_state(self.input_field)
                    .expect("composer input exists")
                    .range_to_utf16(range)
            })
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(input) = self.input_state_mut(self.input_field) {
            input.marked_range = None;
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(input) = self.input_state_mut(self.input_field) {
            input.replace_utf16(range_utf16, new_text);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.input_state_mut(self.input_field) else {
            return;
        };
        let replacement = input.replace_utf16(range_utf16, new_text);
        if let Some(selected) = new_selected_range_utf16 {
            let selected = input.range_from_utf16(&selected);
            input.selected_range =
                replacement.start + selected.start..replacement.start + selected.end;
            input.marked_range = Some(replacement);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Focusable for LoomView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.composer_focus_handle.clone()
    }
}

impl LoomView {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn try_new(
        options: &UiOptions,
        focus_handle: FocusHandle,
        rename_focus_handle: FocusHandle,
    ) -> Result<Self, LoomError> {
        let (connection, workspace_root, project_id, demo_workspace) =
            if let Some(remote_url) = &options.remote {
                let token = options.token.as_deref().ok_or_else(|| {
                    LoomError::invalid_request("remote connections require LOOM_TOKEN to be set")
                })?;
                let connection = ClientConnection::remote(remote_url.clone(), token.to_owned())?;
                negotiate(&connection)?;
                let projects = list_projects(&connection)?;
                let project = select_remote_project(
                    &projects,
                    options.workspace.as_deref().and_then(|path| path.to_str()),
                )?;
                let workspace_root = project.root.clone().map(PathBuf::from).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        "selected remote project has no configured workspace root",
                        false,
                    )
                })?;
                (connection, workspace_root, project.id, false)
            } else {
                let (workspace_root, demo_workspace) = prepare_workspace(options)?;
                let project_id = if demo_workspace {
                    ProjectId::new()
                } else {
                    stable_project_id(&workspace_root)
                };
                let backend = if demo_workspace {
                    InProcessBackend::demo_with_github_copilot()?
                } else if let Some(endpoint) = &options.endpoint {
                    InProcessBackend::with_openai_compatible_persistent_with_github_copilot(
                        endpoint,
                        options.api_key.as_deref().unwrap_or_default(),
                        options.model.clone(),
                        backend_persistence_path(&workspace_root)?,
                    )?
                } else {
                    InProcessBackend::new_persistent_with_github_copilot(backend_persistence_path(
                        &workspace_root,
                    )?)?
                };
                (
                    ClientConnection::InProcess(backend.connect()),
                    workspace_root,
                    project_id,
                    demo_workspace,
                )
            };
        if options.remote.is_none() {
            negotiate(&connection)?;
            open_workspace(&connection, project_id, &workspace_root)?;
        }
        let sessions = list_sessions(&connection, project_id)?;
        let session = sessions.into_iter().next().map_or_else(
            || create_session(&connection, project_id, "New session"),
            Ok,
        )?;
        let models = list_models(&connection)?;
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
        let run = if demo_workspace {
            Some(start_run(
                &connection,
                &session,
                &workspace_root,
                &model,
                &options.task,
            )?)
        } else {
            None
        };
        let mut view = Self {
            backend: BackendWorker::spawn(connection.clone()),
            connection,
            project_id,
            project: None,
            workspace_root,
            projects: Vec::new(),
            sessions: vec![session.clone()],
            active_session: session.clone(),
            active_run: run.clone(),
            active_run_id: run.as_ref().map(|run| run.id),
            default_model: model.clone(),
            session_models: BTreeMap::new(),
            agent_mode: AgentMode::Agent,
            agent_mode_picker_open: false,
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model,
            models,
            model_picker_open: false,
            settings_open: false,
            theme_choice: ThemeChoice::System,
            dark_theme: true,
            after_sequence: None,
            timeline: Vec::new(),
            pending_approval: None,
            pending_input: None,
            composer: TextBufferState::new(""),
            composer_focus_handle: focus_handle,
            input_field: InputField::Composer,
            session_state: session.state,
            run_state: run.as_ref().map(|run| run.state),
            summary: run.as_ref().and_then(|run| run.summary.clone()),
            review: ReviewState::default(),
            tasks: Vec::new(),
            rename_dialog: None,
            rename_focus_handle,
            backend_status: BackendStatus::Connected,
            demo_workspace,
            login_enabled: options.remote.is_none(),
            github_connected: FileCredentialStore::open(FileCredentialStore::default_path())
                .ok()
                .and_then(|credentials| {
                    credentials
                        .resolve(&CredentialRef::new(GITHUB_COPILOT_CREDENTIAL_REF))
                        .ok()
                })
                .is_some(),
            github_login: None,
            run_poll_scheduled: false,
        };
        view.refresh_models();
        view.refresh_sessions()?;
        let active_session = view.active_session.clone();
        view.load_session(active_session);
        Ok(view)
    }

    /// Builds the view for the browser client: connects to a remote backend
    /// over the in-page WebSocket transport and resolves the same project /
    /// session / model state that native's remote-mode bootstrap resolves,
    /// using the `_async` request helpers since nothing may block the page's
    /// single JS thread. Unlike [`Self::try_new`], this does not load the
    /// active session's snapshot/events itself (that requires a `Context`,
    /// which does not exist yet); the caller finishes bootstrapping once the
    /// view is mounted, via [`Self::select_session`], [`Self::reload_sessions`]
    /// and [`Self::refresh_models_async`].
    #[cfg(target_family = "wasm")]
    pub(crate) async fn try_new_browser(
        options: &BrowserOptions,
        focus_handle: FocusHandle,
        rename_focus_handle: FocusHandle,
    ) -> Result<Self, LoomError> {
        let connection = ClientConnection::browser(options.remote(), options.token())?;
        negotiate_async(&connection).await?;
        let projects = list_projects_async(&connection).await?;
        // A freshly started `--serve` backend has no projects open yet; if
        // none match (or none exist), open the requested workspace as a new
        // project ourselves, the same way the native remote-mode M4 demo
        // does.
        let (project, project_id, workspace_root) =
            match select_remote_project(&projects, options.workspace()) {
                Ok(project) => {
                    let workspace_root =
                        project.root.clone().map(PathBuf::from).ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::WorkspaceAccessDenied,
                                "selected remote project has no configured workspace root",
                                false,
                            )
                        })?;
                    let project_id = project.id;
                    (Some(project), project_id, workspace_root)
                }
                Err(error) => {
                    let root = options.workspace().ok_or(error)?;
                    let project_id = ProjectId::new();
                    open_workspace_async(&connection, project_id, root).await?;
                    (None, project_id, PathBuf::from(root))
                }
            };
        let sessions = list_sessions_async(&connection, project_id).await?;
        let session = match sessions.into_iter().next() {
            Some(session) => session,
            None => create_session_async(&connection, project_id, "New session").await?,
        };
        let models = list_models_async(&connection).await?;
        let model = options
            .model()
            .cloned()
            .filter(|model| models.contains(model))
            .or_else(|| models.first().cloned())
            .unwrap_or_else(|| ModelId::new("default"));

        Ok(Self {
            backend: BackendWorker::spawn(connection.clone()),
            connection,
            project_id,
            project,
            workspace_root,
            projects,
            sessions: vec![session.clone()],
            active_session: session.clone(),
            active_run: None,
            active_run_id: None,
            default_model: model.clone(),
            session_models: BTreeMap::new(),
            agent_mode: AgentMode::Agent,
            agent_mode_picker_open: false,
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model,
            models,
            model_picker_open: false,
            settings_open: false,
            theme_choice: ThemeChoice::System,
            dark_theme: true,
            after_sequence: None,
            timeline: Vec::new(),
            pending_approval: None,
            pending_input: None,
            composer: TextBufferState::new(""),
            composer_focus_handle: focus_handle,
            input_field: InputField::Composer,
            session_state: session.state,
            run_state: None,
            summary: None,
            review: ReviewState::default(),
            tasks: Vec::new(),
            rename_dialog: None,
            rename_focus_handle,
            backend_status: BackendStatus::Connected,
            demo_workspace: false,
            login_enabled: false,
            github_connected: false,
            github_login: None,
            run_poll_scheduled: false,
        })
    }

    /// Submits a backend request without blocking the UI thread and applies the
    /// answer on the UI thread once it arrives.
    pub(crate) fn dispatch(
        &self,
        cx: &mut Context<Self>,
        request: ClientRequest,
        apply: impl FnOnce(&mut Self, ResponseEnvelope, &mut Context<Self>) + 'static,
    ) {
        let pending = self.backend.submit(RequestEnvelope::new(request));
        cx.spawn(async move |view, cx| {
            let response = cx
                .background_spawn(async move { pending.wait().await })
                .await;
            view.update(cx, |view, cx| {
                apply(view, response, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn record_status(&mut self, status: impl Into<String>) {
        self.timeline.push(TimelineItem::Status(status.into()));
    }

    pub(crate) fn record_backend_error(&mut self, operation: &str, error: LoomError) {
        self.backend_status = BackendStatus::Error(error.clone());
        self.timeline.push(TimelineItem::Error {
            operation: operation.to_owned(),
            error,
        });
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
        for provider_id in provider_ids {
            let response = self.connection.request(RequestEnvelope::new(
                ClientRequest::DiscoverProviderModels {
                    provider_id: provider_id.clone(),
                },
            ));
            if let Err(error) = response.result {
                self.record_status(format!(
                    "Model refresh unavailable for {}: {}",
                    provider_id.as_str(),
                    error.message
                ));
            }
        }
        match list_models(&self.connection) {
            Ok(models) => self.apply_models(models),
            Err(error) => self.record_status(format!("Could not refresh models: {error}")),
        }
    }

    /// Refreshes the model list from a UI handler, one request at a time, on the
    /// connection worker.
    pub(crate) fn refresh_models_async(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::ListProviders,
            |view, response, cx| match response.result {
                Ok(ServerResponse::Providers { providers }) => {
                    for provider in providers {
                        let provider_id = provider.id.clone();
                        view.dispatch(
                            cx,
                            ClientRequest::DiscoverProviderModels {
                                provider_id: provider_id.clone(),
                            },
                            move |view, response, cx| {
                                if let Err(error) = response.result {
                                    view.record_status(format!(
                                        "Model refresh unavailable for {}: {}",
                                        provider_id.as_str(),
                                        error.message
                                    ));
                                }
                                view.dispatch(
                                    cx,
                                    ClientRequest::ListModels,
                                    |view, response, _| match response.result {
                                        Ok(ServerResponse::Models { models }) => view.apply_models(
                                            models.into_iter().map(|model| model.id).collect(),
                                        ),
                                        Err(error) => view.record_status(format!(
                                            "Could not refresh models: {error}"
                                        )),
                                        Ok(response) => view.record_backend_error(
                                            "model refresh",
                                            unexpected_response("model list", response),
                                        ),
                                    },
                                );
                            },
                        );
                    }
                }
                Err(error) => view.record_status(format!(
                    "Could not list providers for model refresh: {error}"
                )),
                Ok(response) => view.record_backend_error(
                    "model refresh",
                    unexpected_response("provider list", response),
                ),
            },
        );
    }

    fn apply_models(&mut self, models: Vec<ModelId>) {
        if !models.contains(&self.model)
            && let Some(model) = models
                .iter()
                .find(|model| model.as_str() == self.default_model.as_str())
                .cloned()
                .or_else(|| models.first().cloned())
        {
            self.model = model;
        }
        self.models = models;
        self.record_status(format!(
            "Model list refreshed ({} available)",
            self.models.len()
        ));
    }

    /// Loads the session and project lists synchronously for the startup
    /// bootstrap. Interactive refreshes use [`Self::reload_sessions`].
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn refresh_sessions(&mut self) -> Result<(), LoomError> {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::ListAgentSessions {
                    project_id: Some(self.project_id),
                    include_archived: false,
                }));
        match response.result? {
            ServerResponse::AgentSessions { sessions } => {
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

        let response = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::ListProjects));
        match response.result? {
            ServerResponse::Projects { projects } => {
                self.projects = projects;
                self.project = self
                    .projects
                    .iter()
                    .find(|project| project.id == self.project_id)
                    .cloned();
            }
            response => return Err(unexpected_response("project list", response)),
        }
        self.backend_status = BackendStatus::Connected;
        Ok(())
    }

    /// Reloads the session and project lists through the connection worker.
    pub(crate) fn reload_sessions(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::ListAgentSessions {
                project_id: Some(self.project_id),
                include_archived: false,
            },
            |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::AgentSessions { sessions }) => {
                        view.sessions = sessions;
                        if let Some(active) = view
                            .sessions
                            .iter()
                            .find(|session| session.id == view.active_session.id)
                        {
                            view.active_session = active.clone();
                            view.session_state = active.state;
                        }
                        view.backend_status = BackendStatus::Connected;
                    }
                    Err(error) => view.record_backend_error("session list refresh", error),
                    Ok(response) => view.record_backend_error(
                        "session list refresh",
                        unexpected_response("session list", response),
                    ),
                }
                view.dispatch(
                    cx,
                    ClientRequest::ListProjects,
                    |view, response, _| match response.result {
                        Ok(ServerResponse::Projects { projects }) => {
                            view.projects = projects;
                            view.project = view
                                .projects
                                .iter()
                                .find(|project| project.id == view.project_id)
                                .cloned();
                        }
                        Err(error) => view.record_backend_error("project list refresh", error),
                        Ok(response) => view.record_backend_error(
                            "project list refresh",
                            unexpected_response("project list", response),
                        ),
                    },
                );
            },
        );
    }

    pub(crate) fn reset_projection(&mut self) {
        self.timeline.clear();
        self.pending_approval = None;
        self.pending_input = None;
        self.active_run = None;
        self.active_run_id = None;
        self.run_state = None;
        self.summary = None;
        self.after_sequence = None;
    }

    pub(crate) fn activate_session(&mut self, session: AgentSessionSnapshot) {
        self.active_session = session;
        self.session_state = self.active_session.state;
        self.model = self
            .session_models
            .get(&self.active_session.id)
            .cloned()
            .unwrap_or_else(|| self.default_model.clone());
        self.reset_projection();
        self.review.selected_file = None;
        self.review.changes.clear();
        self.review.diff = None;
        self.review.diff_path = None;
        self.review.evidence.clear();
        self.tasks.clear();
    }

    /// Loads a session synchronously.
    ///
    /// Only used by the startup bootstrap, before a window exists; every
    /// interactive path uses [`Self::select_session`], which goes through the
    /// connection worker.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn load_session(&mut self, session: AgentSessionSnapshot) {
        self.activate_session(session);
        let projection_sequence = match self
            .connection
            .request(RequestEnvelope::new(
                ClientRequest::GetAgentSessionSnapshot {
                    session_id: self.active_session.id,
                },
            ))
            .result
        {
            Ok(ServerResponse::AgentSessionSnapshot(projection)) => {
                self.active_session = projection.session;
                self.session_state = self.active_session.state;
                if let Some(run) = projection.active_run {
                    self.model = run.run.model.clone();
                    self.session_task_cache
                        .insert(self.active_session.id, run.run.task.clone());
                    self.apply_run_projection(run);
                }
                Some(projection.latest_sequence)
            }
            Err(error) => {
                self.record_backend_error("load session snapshot", error);
                None
            }
            Ok(response) => {
                self.record_backend_error(
                    "load session snapshot",
                    unexpected_response("session snapshot", response),
                );
                None
            }
        };
        if let Err(error) = self.collect_events_since(projection_sequence) {
            self.record_backend_error("load session events", error);
        }
        self.ensure_session_task_message(self.active_session.id);
        if let Some(run) = &self.active_run {
            self.session_task_cache
                .insert(self.active_session.id, run.task.clone());
            self.ensure_session_task_message(self.active_session.id);
        }
        // The session projection and event stream are sufficient for the
        // central view. Review/VCS data is loaded when the review pane opens.
    }

    #[cfg(not(target_family = "wasm"))]
    fn collect_events_since(
        &mut self,
        after_sequence: Option<EventSequence>,
    ) -> Result<(), LoomError> {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(self.active_session.id),
                    after_sequence,
                }));
        match response.result? {
            ServerResponse::SessionEvents { events } => {
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
            }
            ServerResponse::SessionEventsSnapshot {
                session,
                events,
                latest_sequence,
                ..
            } => {
                self.active_session = session;
                self.reset_projection();
                self.after_sequence = Some(latest_sequence);
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
            }
            response => return Err(unexpected_response("session event stream", response)),
        }
        self.backend_status = BackendStatus::Connected;
        Ok(())
    }

    /// Applies newly journaled session events through the connection worker.
    fn poll_run_once(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::GetSessionEvents {
                session_id: Some(self.active_session.id),
                after_sequence: self.after_sequence,
            },
            |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::SessionEvents { events }) => {
                        for event in events {
                            view.after_sequence = Some(event.sequence);
                            view.consume_event(&event.event);
                        }
                        view.backend_status = BackendStatus::Connected;
                    }
                    Ok(ServerResponse::SessionEventsSnapshot {
                        session,
                        events,
                        latest_sequence,
                        ..
                    }) => {
                        view.active_session = session;
                        view.reset_projection();
                        view.after_sequence = Some(latest_sequence);
                        for event in events {
                            view.after_sequence = Some(event.sequence);
                            view.consume_event(&event.event);
                        }
                        view.backend_status = BackendStatus::Connected;
                    }
                    Err(error) => view.record_backend_error("session event stream", error),
                    Ok(response) => view.record_backend_error(
                        "session event stream",
                        unexpected_response("session event stream", response),
                    ),
                }
                view.run_poll_scheduled = false;
                view.schedule_run_poll(cx);
            },
        );
    }

    fn run_is_active(&self) -> bool {
        matches!(
            self.run_state,
            Some(AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating)
        )
    }

    /// Polls the journal while a run is active. The protocol keeps the event
    /// stream resumable, so polling is sufficient for both local and remote
    /// connections without making the UI depend on a transport-specific push
    /// implementation.
    pub(crate) fn schedule_run_poll(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled || !self.run_is_active() {
            return;
        }
        self.run_poll_scheduled = true;
        cx.spawn(async move |view, cx| {
            cx.background_spawn(async {
                std::thread::sleep(Duration::from_millis(250));
            })
            .await;
            view.update(cx, |view, cx| {
                if view.run_is_active() {
                    view.poll_run_once(cx);
                } else {
                    view.run_poll_scheduled = false;
                }
            })
            .ok();
        })
        .detach();
    }

    fn start_run_polling(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled || self.active_run_id.is_none() {
            return;
        }
        self.run_poll_scheduled = true;
        self.poll_run_once(cx);
    }

    pub(crate) fn consume_event(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::AgentSessionCreated { snapshot } => {
                self.active_session = snapshot.clone();
                self.session_state = snapshot.state;
            }
            ServerEvent::AgentSessionStateChanged { current, .. } => {
                self.session_state = *current;
                self.active_session.state = *current;
            }
            ServerEvent::AgentSessionForked { .. } => {}
            ServerEvent::AgentSessionRenamed { name, .. } => {
                self.active_session.name = name.clone();
            }
            ServerEvent::AgentSessionArchived { .. } => {
                self.session_state = AgentSessionState::Archived;
                self.active_session.state = AgentSessionState::Archived;
            }
            ServerEvent::Agent { event } => self.consume_agent_event(event),
            ServerEvent::WorkspaceChanged { change } => {
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
                self.active_run = Some(snapshot.clone());
                self.active_run_id = Some(snapshot.id);
                self.run_state = Some(snapshot.state);
                self.timeline
                    .push(TimelineItem::Status("Agent run started".to_owned()));
            }
            AgentEvent::PlanProposed { plan, .. } => self.timeline.push(TimelineItem::Plan(
                plan.steps
                    .iter()
                    .map(|step| step.description.clone())
                    .collect(),
            )),
            AgentEvent::UserMessage { text, .. } => {
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
                if text.is_empty() {
                    return;
                }
                if let Some(TimelineItem::Assistant(message)) = self.timeline.last_mut() {
                    message.push_str(text);
                } else {
                    self.timeline.push(TimelineItem::Assistant(text.clone()));
                }
            }
            AgentEvent::StepStarted { index, .. } => {
                self.timeline
                    .push(TimelineItem::StepStarted { index: *index });
            }
            AgentEvent::StepCompleted { index, .. } => {
                self.timeline
                    .push(TimelineItem::StepCompleted { index: *index });
            }
            AgentEvent::ContextInspected { inspection, .. } => self.record_status(format!(
                "Context: {} input tokens, {} omitted",
                inspection.included_tokens, inspection.omitted_tokens
            )),
            AgentEvent::ProviderError { error, .. } | AgentEvent::ContextError { error, .. } => {
                self.timeline.push(TimelineItem::Error {
                    operation: "agent".to_owned(),
                    error: error.clone(),
                });
            }
            AgentEvent::ToolCallRequested { call, .. } => {
                self.timeline.push(TimelineItem::ToolRequested {
                    name: call.name.clone(),
                    arguments: bounded(&serde_json::to_string(&call.arguments).unwrap_or_default()),
                });
            }
            AgentEvent::ToolApprovalRequired { call, .. } => {
                for item in &mut self.timeline {
                    if let TimelineItem::Approval { active, .. } = item {
                        *active = false;
                    }
                }
                self.pending_approval = Some(call.clone());
                self.timeline.push(TimelineItem::Approval {
                    name: call.name.clone(),
                    active: true,
                });
            }
            AgentEvent::ToolPolicyEvaluated { evaluation, .. } => self.record_status(format!(
                "Policy {:?}: {}",
                evaluation.decision, evaluation.reason
            )),
            AgentEvent::ToolApprovalDecided { decision, .. } => {
                self.pending_approval = None;
                for item in &mut self.timeline {
                    if let TimelineItem::Approval { active, .. } = item {
                        *active = false;
                    }
                }
                self.record_status(format!("Approval: {decision:?}"));
            }
            AgentEvent::ToolCallStarted { call, .. } => {
                self.timeline
                    .push(TimelineItem::ToolStarted(call.name.clone()));
            }
            AgentEvent::ToolOutputChunk { chunk, .. } => {
                if let Some(TimelineItem::ToolOutput(output)) = self.timeline.last_mut() {
                    output.push_str(chunk);
                    *output = bounded(output);
                } else {
                    self.timeline.push(TimelineItem::ToolOutput(bounded(chunk)));
                }
            }
            AgentEvent::ToolCallCompleted { result, .. } => {
                self.timeline.push(TimelineItem::ToolCompleted {
                    name: result.name.clone(),
                    success: result.success,
                });
            }
            AgentEvent::NeedsInput { prompt, .. } => {
                self.pending_input = Some(prompt.clone());
                self.timeline.push(TimelineItem::NeedsInput(prompt.clone()));
            }
            AgentEvent::RunUsage { usage, .. } => self.record_status(format!(
                "Usage: {} input / {} output tokens",
                usage.input_tokens, usage.output_tokens
            )),
            AgentEvent::RunUsageUpdated { usage, .. } => self.record_status(format!(
                "Total usage: {} input / {} output / {} tool calls",
                usage.input_tokens, usage.output_tokens, usage.tool_calls
            )),
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
            }
            AgentEvent::RunCompleted { snapshot } => {
                self.active_run = Some(snapshot.clone());
                self.active_run_id = Some(snapshot.id);
                self.run_state = Some(snapshot.state);
                self.session_state = session_state_for_run(snapshot.state);
                self.active_session.state = self.session_state;
                self.summary = snapshot.summary.clone();
                if let Some(summary) = &snapshot.summary {
                    self.timeline.push(TimelineItem::Summary {
                        text: summary.clone(),
                        evidence: snapshot
                            .evidence
                            .iter()
                            .map(|link| format!("{} ({})", link.label, link.uri))
                            .collect(),
                    });
                }
            }
        }
    }

    pub(crate) fn apply_run_projection(&mut self, projection: AgentRunSnapshotProjection) {
        self.active_run_id = Some(projection.run.id);
        self.active_run = Some(projection.run.clone());
        self.run_state = Some(projection.run.state);
        self.summary = projection.run.summary.clone();
        self.pending_approval = projection.pending_approval;
        self.pending_input = projection.pending_input;
        if self.timeline.is_empty() {
            let mut timeline = Vec::new();
            for message in projection.messages {
                match message.role {
                    MessageRole::User => timeline.push(TimelineItem::User(message.content)),
                    MessageRole::Assistant => {
                        if message.content.is_empty() {
                            continue;
                        }
                        if let Some(TimelineItem::Assistant(previous)) = timeline.last_mut() {
                            if !previous.is_empty() && !message.content.is_empty() {
                                previous.push_str("\n\n");
                            }
                            previous.push_str(&message.content);
                        } else {
                            timeline.push(TimelineItem::Assistant(message.content));
                        }
                    }
                    MessageRole::Tool => {
                        timeline.push(TimelineItem::ToolOutput(bounded(&message.content)))
                    }
                    MessageRole::System => {}
                }
            }
            if !projection.plan.is_empty() {
                self.timeline.insert(
                    0,
                    TimelineItem::Plan(
                        projection
                            .plan
                            .into_iter()
                            .map(|step| step.description)
                            .collect(),
                    ),
                );
            }
            self.timeline = timeline;
        }
        if let Some(summary) = &projection.run.summary
            && !self
                .timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::Summary { .. }))
        {
            self.timeline.push(TimelineItem::Summary {
                text: summary.clone(),
                evidence: projection
                    .run
                    .evidence
                    .iter()
                    .map(|link| format!("{} ({})", link.label, link.uri))
                    .collect(),
            });
        }
    }

    /// Loads the review projections through the connection worker.
    pub(crate) fn refresh_review(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::GetWorkspaceChanges {
                project_id: self.project_id,
                after_sequence: None,
            },
            |view, response, _| match response.result {
                Ok(ServerResponse::WorkspaceChanges { changes, truncated }) => {
                    let mut seen = BTreeSet::new();
                    view.review.changes = changes
                        .into_iter()
                        .rev()
                        .filter(|change| seen.insert(change.path.clone()))
                        .take(MAX_REVIEW_CHANGES)
                        .collect();
                    if truncated {
                        view.record_status(
                            "Workspace review is showing the most recent changes".to_owned(),
                        );
                    }
                }
                Err(error) => view.record_backend_error("workspace review refresh", error),
                Ok(response) => view.record_backend_error(
                    "workspace review refresh",
                    unexpected_response("workspace changes", response),
                ),
            },
        );
        self.dispatch(
            cx,
            ClientRequest::GetVcsStatus {
                project_id: self.project_id,
            },
            |view, response, _| match response.result {
                Ok(ServerResponse::VcsStatus(status)) => view.review.vcs = Some(status),
                Err(error) => {
                    view.review.vcs = None;
                    view.record_status(format!("VCS review unavailable: {error}"));
                }
                Ok(response) => view.record_backend_error(
                    "VCS review refresh",
                    unexpected_response("VCS status", response),
                ),
            },
        );
    }

    pub(crate) fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let name = dialog.input.text.trim().to_owned();
        if name.is_empty() {
            self.record_status("Session name cannot be empty");
            self.rename_dialog = Some(dialog);
            return;
        }
        self.dispatch(
            cx,
            ClientRequest::RenameAgentSession {
                session_id: dialog.session.id,
                name,
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::AgentSessionRenamed(snapshot)) => {
                    view.active_session = snapshot;
                    view.reload_sessions(cx);
                }
                Err(error) => {
                    view.rename_dialog = Some(dialog);
                    view.record_backend_error("rename session", error);
                }
                Ok(response) => view.record_backend_error(
                    "rename session",
                    unexpected_response("session rename", response),
                ),
            },
        );
    }

    pub(crate) fn archive_active(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::ArchiveAgentSession {
                session_id: self.active_session.id,
            },
            |view, response, cx| match response.result {
                Ok(ServerResponse::AgentSessionArchived(_)) => {
                    view.dispatch(
                        cx,
                        ClientRequest::ListAgentSessions {
                            project_id: Some(view.project_id),
                            include_archived: false,
                        },
                        |view, response, cx| match response.result {
                            Ok(ServerResponse::AgentSessions { sessions }) => {
                                view.sessions = sessions;
                                if let Some(session) = view.sessions.first().cloned() {
                                    view.select_session(session, cx);
                                } else {
                                    view.create_session_async("New session".to_owned(), cx);
                                }
                                view.reload_sessions(cx);
                            }
                            Err(error) => view.record_backend_error("session list refresh", error),
                            Ok(response) => view.record_backend_error(
                                "session list refresh",
                                unexpected_response("session list", response),
                            ),
                        },
                    );
                }
                Err(error) => view.record_backend_error("archive session", error),
                Ok(response) => view.record_backend_error(
                    "archive session",
                    unexpected_response("session archive", response),
                ),
            },
        );
    }

    pub(crate) fn send_message(&mut self, message: String, cx: &mut Context<Self>) {
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
            self.optimistic_messages.push(message.clone());
            ClientRequest::SendAgentMessage {
                run_id,
                message: message.clone(),
            }
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
            ClientRequest::StartAgentRun {
                session_id: self.active_session.id,
                task: message.clone(),
                model: self.model.clone(),
                workspace_root: self.workspace_root.display().to_string(),
                system_instructions: Some(
                    "Work methodically, use the available tools, and report validation.".to_owned(),
                ),
                repository_instructions: Some(
                    "Keep the change focused and provide reviewable evidence.".to_owned(),
                ),
            }
        };
        self.timeline.push(TimelineItem::User(message));
        let session_id = self.active_session.id;
        let rename_request = session_title.map(|title| {
            self.backend
                .submit(RequestEnvelope::new(ClientRequest::RenameAgentSession {
                    session_id,
                    name: title,
                }))
        });
        let run_request = self.backend.submit(RequestEnvelope::new(request));
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
            Ok(ServerResponse::AgentRunStarted(run)) | Ok(ServerResponse::AgentRun(run)) => {
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

    pub(crate) fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer.set_text("");
        self.send_message(text, cx);
    }

    pub(crate) fn focus_composer(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer_focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn focus_rename(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rename_focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.edit_input().selected_range.is_empty() {
            let cursor = self.edit_input().cursor_offset();
            if cursor == 0 {
                return;
            }
            let input = self.edit_input_mut();
            input.selected_range = input.previous_boundary(cursor)..cursor;
        }
        self.edit_input_mut().replace_utf16(None, "");
        cx.notify();
    }

    pub(crate) fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.edit_input().selected_range.is_empty() {
            let cursor = self.edit_input().cursor_offset();
            if cursor >= self.edit_input().text.len() {
                return;
            }
            let input = self.edit_input_mut();
            input.selected_range = cursor..input.next_boundary(cursor);
        }
        self.edit_input_mut().replace_utf16(None, "");
        cx.notify();
    }

    pub(crate) fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.edit_input().selected_range.is_empty() {
            self.edit_input()
                .previous_boundary(self.edit_input().cursor_offset())
        } else {
            self.edit_input().selected_range.start
        };
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    pub(crate) fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.edit_input().selected_range.is_empty() {
            self.edit_input()
                .next_boundary(self.edit_input().cursor_offset())
        } else {
            self.edit_input().selected_range.end
        };
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    pub(crate) fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_input_mut().select_all();
        cx.notify();
    }

    pub(crate) fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self
            .edit_input()
            .line_and_column(self.edit_input().cursor_offset());
        let offset = self.edit_input().line_ranges()[line].start;
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    pub(crate) fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self
            .edit_input()
            .line_and_column(self.edit_input().cursor_offset());
        let offset = self.edit_input().line_ranges()[line].end;
        self.edit_input_mut().move_to(offset, false);
        cx.notify();
    }

    pub(crate) fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.edit_input_mut().replace_utf16(None, &text);
            cx.notify();
        }
    }

    pub(crate) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_input().selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.edit_input().text[self.edit_input().selected_range.clone()].to_owned(),
            ));
        }
    }

    pub(crate) fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if self.rename_dialog.is_some() {
            self.confirm_rename(cx);
        } else {
            self.submit_composer(cx);
        }
        cx.notify();
    }

    pub(crate) fn select_session(&mut self, session: AgentSessionSnapshot, cx: &mut Context<Self>) {
        self.github_login = None;
        self.settings_open = false;
        self.review.open = false;
        self.activate_session(session.clone());
        self.ensure_session_task_message(session.id);
        let session_id = session.id;
        let backend = self.backend.clone();
        let snapshot_request = self.backend.submit(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot { session_id },
        ));
        cx.spawn(async move |view, cx| {
            let snapshot = cx
                .background_spawn(async move { snapshot_request.wait().await })
                .await;
            let after_sequence = match &snapshot.result {
                Ok(ServerResponse::AgentSessionSnapshot(projection)) => {
                    Some(projection.latest_sequence)
                }
                _ => None,
            };
            let events_request =
                backend.submit(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    after_sequence,
                }));
            let events = cx
                .background_spawn(async move { events_request.wait().await })
                .await;
            view.update(cx, |view, cx| {
                view.finish_async_session_load(session_id, snapshot, events, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn finish_async_session_load(
        &mut self,
        session_id: AgentSessionId,
        snapshot_response: ResponseEnvelope,
        events_response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        if self.active_session.id != session_id {
            return;
        }
        match snapshot_response.result {
            Ok(ServerResponse::AgentSessionSnapshot(projection)) => {
                self.active_session = projection.session;
                self.reset_projection();
                self.after_sequence = Some(projection.latest_sequence);
                if let Some(run) = projection.active_run {
                    self.session_task_cache
                        .insert(session_id, run.run.task.clone());
                    self.model = run.run.model.clone();
                    self.apply_run_projection(run);
                }
            }
            Err(error) => {
                self.record_backend_error("load session snapshot", error);
                self.reset_projection();
            }
            Ok(response) => {
                self.record_backend_error(
                    "load session snapshot",
                    unexpected_response("session snapshot", response),
                );
                self.reset_projection();
            }
        }
        match events_response.result {
            Ok(ServerResponse::SessionEvents { events }) => {
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
            }
            Ok(ServerResponse::SessionEventsSnapshot {
                session,
                events,
                latest_sequence,
                ..
            }) => {
                self.active_session = session;
                self.reset_projection();
                self.after_sequence = Some(latest_sequence);
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
            }
            Err(error) => self.record_backend_error("load session events", error),
            Ok(response) => self.record_backend_error(
                "load session events",
                unexpected_response("session event stream", response),
            ),
        }
        self.ensure_session_task_message(session_id);
        cx.notify();
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
    fn start_github_login_flow(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async { GitHubCopilotAuthenticator::default().begin() });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| view.handle_github_device_code(result, cx))
                .ok();
        })
        .detach();
    }

    /// GitHub Copilot device-code sign-in relies on `loom-providers`'
    /// credential store and OAuth flow, which have no browser equivalent (no
    /// OS keychain, no writable local file). The browser client cannot offer
    /// this login path.
    #[cfg(target_family = "wasm")]
    fn start_github_login_flow(&mut self, cx: &mut Context<Self>) {
        self.github_login = Some(GitHubLoginState::Error(
            "GitHub sign-in isn't available in the browser client yet.".to_owned(),
        ));
        cx.notify();
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
        match FileCredentialStore::open(FileCredentialStore::default_path())
            .and_then(|credentials| credentials.insert(GITHUB_COPILOT_CREDENTIAL_REF, token))
        {
            Ok(()) => {}
            Err(error) => {
                self.github_login = Some(GitHubLoginState::Error(error.message));
                cx.notify();
                return;
            }
        };
        if self.model.as_str() == "deterministic/demo" {
            self.model = ModelId::new(GITHUB_COPILOT_DEFAULT_MODEL);
        }
        self.refresh_models_async(cx);
        self.github_login = Some(GitHubLoginState::Success);
        self.github_connected = true;
        self.record_status("GitHub Copilot login succeeded");
        cx.notify();
    }

    pub(crate) fn toggle_model_picker(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model_picker_open = !self.model_picker_open;
        cx.notify();
    }

    pub(crate) fn refresh_models_button(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_models_async(cx);
        cx.notify();
    }

    pub(crate) fn select_model(&mut self, model: ModelId, cx: &mut Context<Self>) {
        self.session_models
            .insert(self.active_session.id, model.clone());
        self.model = model;
        self.model_picker_open = false;
        cx.notify();
    }

    pub(crate) fn toggle_agent_mode_picker(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.agent_mode_picker_open = !self.agent_mode_picker_open;
        cx.notify();
    }

    pub(crate) fn select_agent_mode(&mut self, mode: AgentMode, cx: &mut Context<Self>) {
        self.agent_mode = mode;
        self.agent_mode_picker_open = false;
        cx.notify();
    }

    pub(crate) fn select_default_model(&mut self, model: ModelId, cx: &mut Context<Self>) {
        self.default_model = model;
        self.settings_open = false;
        cx.notify();
    }

    pub(crate) fn open_settings_from_menu(&mut self, cx: &mut Context<Self>) {
        self.github_login = None;
        self.review.open = false;
        self.settings_open = true;
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
        self.dark_theme = matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        DARK_THEME_ACTIVE.store(self.dark_theme, Ordering::Relaxed);
        gpui_component::Theme::change(appearance, Some(window), cx);
        cx.notify();
    }

    pub(crate) fn new_session(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = format!("Session {}", self.sessions.len().saturating_add(1));
        self.create_session_async(name, cx);
    }

    /// Creates a session through the connection worker and selects it.
    pub(crate) fn create_session_async(&mut self, name: String, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::CreateAgentSession {
                project_id: self.project_id,
                name,
            },
            |view, response, cx| match response.result {
                Ok(ServerResponse::AgentSessionCreated(snapshot)) => {
                    view.sessions.push(snapshot.clone());
                    view.backend_status = BackendStatus::Connected;
                    view.select_session(snapshot, cx);
                }
                Err(error) => view.record_backend_error("create session", error),
                Ok(response) => view.record_backend_error(
                    "create session",
                    unexpected_response("session creation", response),
                ),
            },
        );
    }

    pub(crate) fn begin_session_rename(
        &mut self,
        session: AgentSessionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_session(session, cx);
        self.rename_dialog = Some(RenameDialogState {
            session: self.active_session.clone(),
            input: TextBufferState::new(self.active_session.name.clone()),
        });
        self.rename_focus_handle.focus(window, cx);
    }

    pub(crate) fn toggle_changes_sidebar(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.review.panel = ReviewPanel::Changes;
        self.review.open = !self.review.open;
        self.refresh_review(cx);
        cx.notify();
    }

    pub(crate) fn show_review(
        &mut self,
        _panel: ReviewPanel,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_open = false;
        self.github_login = None;
        self.toggle_changes_sidebar(event, window, cx);
    }

    pub(crate) fn close_review(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.review.open = false;
        cx.notify();
    }

    pub(crate) fn open_review_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::ReadWorkspaceFile {
                project_id: self.project_id,
                path,
            },
            |view, response, _| match response.result {
                Ok(ServerResponse::WorkspaceFile(mut file)) => {
                    file.content = bounded_to(&file.content, MAX_REVIEW_DIFF);
                    view.review.diff_path = Some(file.path.clone());
                    view.review.selected_file = Some(file);
                    view.review.open = true;
                    view.review.panel = ReviewPanel::Changes;
                }
                Err(error) => view.record_backend_error("read review file", error),
                Ok(response) => view.record_backend_error(
                    "read review file",
                    unexpected_response("workspace file", response),
                ),
            },
        );
    }

    pub(crate) fn render_session_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap_1();
        let view = cx.entity();
        for (index, session) in self.sessions.iter().enumerate() {
            let active = session.id == self.active_session.id;
            let session = session.clone();
            let selected_session = session.clone();
            let card = div()
                .id(("session", index))
                .relative()
                .w_full()
                .px_2()
                .py_2()
                .rounded_sm()
                .bg(if active { rgb(0x293244) } else { rgb(0x1b1d24) })
                .text_color(if active { rgb(0xf3f4f6) } else { rgb(0xb7c0d0) })
                .cursor_pointer()
                .child(
                    div()
                        .text_sm()
                        .child(session.name.clone())
                        .when(session.state == AgentSessionState::Archived, |element| {
                            element.text_color(rgb(0x64748b))
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(state_color(session.state))
                        .child(session_state_name(session.state)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_session(selected_session.clone(), cx);
                }));
            let rename_session = session.clone();
            let archive_session = session.clone();
            let context_view = view.clone();
            list = list.child(card.context_menu(move |menu, _, _| {
                let rename_view = context_view.clone();
                let archive_view = context_view.clone();
                let rename_session = rename_session.clone();
                let archive_session = archive_session.clone();
                menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                    let rename_session = rename_session.clone();
                    rename_view.update(cx, |view, cx| {
                        view.begin_session_rename(rename_session, window, cx);
                    });
                }))
                .item(PopupMenuItem::new("Archive").on_click(move |_, _, _cx| {
                    let archive_session = archive_session.clone();
                    archive_view.update(_cx, |view, cx| {
                        view.select_session(archive_session, cx);
                        view.archive_active(cx);
                    });
                }))
            }));
        }
        if self.sessions.is_empty() {
            list = list.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("No sessions"),
            );
        }
        list
    }

    pub(crate) fn render_model_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut picker = div()
            .w_full()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .bg(rgb(0x20242c))
            .border_1()
            .border_color(rgb(0x3b4555));
        for (index, model) in self.models.iter().enumerate() {
            let model = model.clone();
            let active = model == self.model;
            picker = picker.child(
                div()
                    .id(("model-option", index))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(if active { rgb(0x293244) } else { rgb(0x1b1d24) })
                    .text_sm()
                    .text_color(if active { rgb(0xe5e7eb) } else { rgb(0xb7c0d0) })
                    .cursor_pointer()
                    .child(model.as_str().to_owned())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_model(model.clone(), cx);
                    })),
            );
        }
        if self.models.is_empty() {
            picker = picker.child(
                div()
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child("No model is configured"),
            );
        }
        picker
    }

    pub(crate) fn render_agent_mode_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut picker = div()
            .absolute()
            .bottom(px(34.))
            .left(px(8.))
            .w(px(140.))
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_sm()
            .bg(rgb(0x20242c))
            .border_1()
            .border_color(rgb(0x3b4555))
            .shadow_lg();
        for (index, mode) in AgentMode::ALL.into_iter().enumerate() {
            let active = mode == self.agent_mode;
            picker = picker.child(
                div()
                    .id(("agent-mode", index))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(if active { rgb(0x293244) } else { rgb(0x20242c) })
                    .text_xs()
                    .text_color(if active { rgb(0xf3f4f6) } else { rgb(0xb7c0d0) })
                    .cursor_pointer()
                    .child(mode.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_agent_mode(mode, cx);
                    })),
            );
        }
        picker.into_any()
    }

    pub(crate) fn render_timeline_item(
        &self,
        item: &TimelineItem,
        index: usize,
        _cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let user_background = if self.dark_theme {
            gpui::rgb(0x1f4f78)
        } else {
            gpui::rgb(0xdbeafe)
        };
        let user_foreground = if self.dark_theme {
            gpui::rgb(0xdbeafe)
        } else {
            gpui::rgb(0x1e3a8a)
        };
        match item {
            TimelineItem::User(text) => div()
                .w_full()
                .flex()
                .justify_end()
                .child(
                    div()
                        .w_full()
                        .max_w(px(520.))
                        .p_3()
                        .rounded_lg()
                        .bg(user_background)
                        .text_color(user_foreground)
                        .child(div().text_xs().text_color(user_foreground).child("You"))
                        .child(
                            TextView::markdown(format!("transcript-user-{index}"), text.clone())
                                .selectable(true)
                                .w_full()
                                .text_color(user_foreground),
                        ),
                )
                .into_any(),
            TimelineItem::Assistant(text) => div()
                .p_3()
                .rounded_lg()
                .bg(rgb(0x17191f))
                .text_color(rgb(0xf3f4f6))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x9ad7bd))
                        .child("GitHub Copilot"),
                )
                .child(
                    TextView::markdown(format!("transcript-assistant-{index}"), text.clone())
                        .selectable(true)
                        .w_full()
                        .text_color(rgb(0xf3f4f6)),
                )
                .into_any(),
            TimelineItem::Plan(steps) => {
                let mut card = div()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(0x1e293b))
                    .text_color(rgb(0xdbeafe))
                    .child(div().text_xs().text_color(rgb(0x93c5fd)).child("PLAN"));
                for (index, step) in steps.iter().enumerate() {
                    card = card.child(div().text_sm().child(format!("{}. {}", index + 1, step)));
                }
                card.into_any()
            }
            TimelineItem::StepStarted { index } => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(rgb(0xfef3c7))
                .child(format!("Step {} started", index + 1))
                .into_any(),
            TimelineItem::StepCompleted { index } => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(rgb(0x9ad7bd))
                .child(format!("Step {} completed", index + 1))
                .into_any(),
            TimelineItem::ToolRequested { name, arguments } => div()
                .p_2()
                .rounded_sm()
                .bg(rgb(0x242833))
                .text_color(rgb(0xcbd5e1))
                .child(format!("Tool requested  {name}"))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(arguments.clone()),
                )
                .into_any(),
            TimelineItem::Approval { name, active } => div()
                .p_2()
                .rounded_sm()
                .bg(if *active {
                    rgb(0x493b1a)
                } else {
                    rgb(0x242833)
                })
                .text_color(if *active {
                    rgb(0xfef3c7)
                } else {
                    rgb(0x94a3b8)
                })
                .child(if *active {
                    format!("Approval required  {name}")
                } else {
                    format!("Approval resolved  {name}")
                })
                .into_any(),
            TimelineItem::ToolStarted(name) => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(rgb(0xcbd5e1))
                .child(format!("Tool started  {name}"))
                .into_any(),
            TimelineItem::ToolOutput(output) => div()
                .mx_2()
                .p_2()
                .rounded_sm()
                .bg(rgb(0x0f1115))
                .text_xs()
                .text_color(rgb(0x94a3b8))
                .child(output.clone())
                .into_any(),
            TimelineItem::ToolCompleted { name, success } => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(if *success {
                    rgb(0x9ad7bd)
                } else {
                    rgb(0xfca5a5)
                })
                .child(format!(
                    "Tool completed  {name} [{}]",
                    if *success { "ok" } else { "failed" }
                ))
                .into_any(),
            TimelineItem::Status(status) => div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(rgb(0x94a3b8))
                .child(status.clone())
                .into_any(),
            TimelineItem::Error { operation, error } => div()
                .p_2()
                .rounded_lg()
                .bg(rgb(0x3a1f24))
                .text_sm()
                .text_color(rgb(0xfca5a5))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xfda4af))
                        .child(format!("{} · {}", operation, error.code)),
                )
                .child(
                    div().mt_1().child(
                        TextView::markdown(
                            format!("timeline-error-{index}"),
                            error.message.clone(),
                        )
                        .selectable(true),
                    ),
                )
                .when(error.retryable, |element| {
                    element.child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(0xfda4af))
                            .child("This operation can be retried."),
                    )
                })
                .into_any(),
            TimelineItem::NeedsInput(prompt) => div()
                .p_2()
                .rounded_sm()
                .bg(rgb(0x3b2f66))
                .text_color(rgb(0xe9d5ff))
                .child(div().text_xs().child("Agent needs input"))
                .child(
                    TextView::markdown(format!("timeline-input-{index}"), prompt.clone())
                        .selectable(true),
                )
                .into_any(),
            TimelineItem::Summary { text, evidence } => {
                let mut card = div()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(0x064e3b))
                    .text_color(rgb(0xd1fae5))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x9ad7bd))
                            .child("FINAL SUMMARY"),
                    )
                    .child(text.clone());
                for link in evidence {
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x9ad7bd))
                            .child(format!("Evidence: {link}")),
                    );
                }
                card.into_any()
            }
        }
    }

    pub(crate) fn render_timeline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut timeline = div().flex().flex_col().gap_3().p_4();
        if self.timeline.is_empty() {
            timeline = timeline.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("Start a task to see the agent run here."),
            );
        }
        for (index, item) in self.timeline.iter().enumerate() {
            if matches!(
                item,
                TimelineItem::User(_)
                    | TimelineItem::Assistant(_)
                    | TimelineItem::Approval { .. }
                    | TimelineItem::Error { .. }
                    | TimelineItem::NeedsInput(_)
            ) {
                timeline = timeline.child(self.render_timeline_item(item, index, cx));
            }
        }
        timeline
    }

    pub(crate) fn render_review(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.review.panel {
            ReviewPanel::Changes => "CHANGED FILES",
            ReviewPanel::Diff => "READ-ONLY DIFF",
            ReviewPanel::Evidence => "TASK EVIDENCE",
        };
        let mut body = div()
            .flex_1()
            .id("changes-sidebar-scroll")
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_2();
        match self.review.panel {
            ReviewPanel::Changes => {
                if self.review.changes.is_empty() {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No workspace changes recorded"),
                    );
                }
                for (index, change) in self.review.changes.iter().take(24).enumerate() {
                    let path = change.path.clone();
                    body = body.child(
                        div()
                            .id(("review-file", index))
                            .text_sm()
                            .text_color(change_color(change.kind))
                            .cursor_pointer()
                            .child(format!("{:?}  {}", change.kind, change.path))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_review_file(path.clone(), cx);
                                cx.notify();
                            })),
                    );
                }
                if let Some(status) = &self.review.vcs {
                    for (index, file) in status.files.iter().take(24).enumerate() {
                        let path = file.path.clone();
                        body = body.child(
                            div()
                                .id(("git-file", index))
                                .text_sm()
                                .text_color(rgb(0xfef3c7))
                                .cursor_pointer()
                                .child(format!("Git  {:?}  {}", file.worktree, file.path))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_review_file(path.clone(), cx);
                                    cx.notify();
                                })),
                        );
                    }
                }
                if let Some(file) = &self.review.selected_file {
                    body = body.child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(0x93c5fd))
                            .child(format!("READ-ONLY FILE  {}", file.path)),
                    );
                    body = body.child(
                        div()
                            .p_2()
                            .bg(rgb(0x0f1115))
                            .text_xs()
                            .text_color(rgb(0xcbd5e1))
                            .child(file.content.clone()),
                    );
                }
            }
            ReviewPanel::Diff => {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("Projection only; editing and staging are not available in M5."),
                );
                body = body.child(
                    div()
                        .mt_1()
                        .p_2()
                        .bg(rgb(0x0f1115))
                        .text_xs()
                        .text_color(rgb(0xcbd5e1))
                        .child(
                            self.review
                                .diff
                                .as_ref()
                                .map(|diff| diff.patch.clone())
                                .unwrap_or_else(|| "No diff available".to_owned()),
                        ),
                );
            }
            ReviewPanel::Evidence => {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("TASK RESULT AND EVIDENCE"),
                );
                if let Some(summary) = &self.summary {
                    body = body.child(
                        div()
                            .p_2()
                            .rounded_sm()
                            .bg(rgb(0x064e3b))
                            .text_sm()
                            .text_color(rgb(0xd1fae5))
                            .child(summary.clone()),
                    );
                }
                if self.tasks.is_empty() {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No validation tasks have been run"),
                    );
                }
                for task in self.tasks.iter().take(6) {
                    let status_color = match task.status {
                        TaskStatus::Completed => rgb(0x9ad7bd),
                        TaskStatus::Failed | TaskStatus::Cancelled => rgb(0xfca5a5),
                        TaskStatus::Queued | TaskStatus::Running => rgb(0xfef3c7),
                    };
                    body = body.child(
                        div()
                            .p_2()
                            .rounded_sm()
                            .bg(rgb(0x20242c))
                            .text_sm()
                            .text_color(status_color)
                            .child(format!("{}  {:?}", task.label, task.status))
                            .child(
                                div()
                                    .mt_1()
                                    .text_xs()
                                    .text_color(rgb(0x94a3b8))
                                    .child(bounded(&task.output)),
                            ),
                    );
                    for artifact in task.artifacts.iter().take(3) {
                        body =
                            body.child(div().text_xs().text_color(rgb(0x9ad7bd)).child(format!(
                                "Artifact  {}{}",
                                artifact.path,
                                if artifact.exists { "" } else { " (missing)" }
                            )));
                    }
                }
                if self.review.evidence.is_empty() {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No evidence links attached"),
                    );
                }
                for evidence in &self.review.evidence {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9ad7bd))
                            .child(evidence.clone()),
                    );
                }
            }
        }
        div()
            .w(px(340.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0x17191f))
            .border_l_1()
            .border_color(rgb(0x30343f))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().text_color(rgb(0x93c5fd)).child(title))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div().text_xs().text_color(rgb(0x8f98a6)).child(
                                    self.review
                                        .vcs
                                        .as_ref()
                                        .map(|status| {
                                            format!(
                                                "{}  {}",
                                                status.branch.as_deref().unwrap_or("detached"),
                                                if status.clean { "clean" } else { "modified" }
                                            )
                                        })
                                        .unwrap_or_else(|| "VCS unavailable".to_owned()),
                                ),
                            )
                            .child(
                                Button::new("close-review")
                                    .icon(Icon::new(IconName::FileText))
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.review.panel = ReviewPanel::Changes;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("toggle-review-sidebar-close")
                                    .icon(Icon::new(IconName::PanelRight))
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(Self::close_review)),
                            ),
                    ),
            )
            .child(body)
    }

    pub(crate) fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let placeholder = if self.pending_input.is_some() {
            "Answer the agent question..."
        } else if self.sending_message {
            "Sending direction..."
        } else if self.active_run_id.is_some() {
            "Send follow-up direction..."
        } else {
            "Describe a task..."
        };
        div()
            .w_full()
            .p_2()
            .bg(rgb(0x17191f))
            .border_t_1()
            .border_color(rgb(0x30343f))
            .child(
                div()
                    .w_full()
                    .min_h(px(42.))
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(0x0f1115))
                    .border_1()
                    .border_color(rgb(0x3b4555))
                    .text_color(rgb(0xe5e7eb))
                    .key_context("Composer")
                    .track_focus(&self.composer_focus_handle)
                    .cursor(CursorStyle::IBeam)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::focus_composer))
                    .on_action(cx.listener(Self::backspace))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::left))
                    .on_action(cx.listener(Self::right))
                    .on_action(cx.listener(Self::select_all))
                    .on_action(cx.listener(Self::home))
                    .on_action(cx.listener(Self::end))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(Self::copy))
                    .on_action(cx.listener(Self::submit))
                    .child(TextInputElement {
                        view: cx.entity(),
                        field: InputField::Composer,
                    })
                    .when(self.composer.text.is_empty(), |element| {
                        element.child(div().text_sm().text_color(rgb(0x64748b)).child(placeholder))
                    }),
            )
            .child(
                div().mt_1().flex().items_center().justify_between().child(
                    div()
                        .flex()
                        .relative()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .id("composer-agent-mode")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .text_color(rgb(0xb7c0d0))
                                .cursor_pointer()
                                .child(self.agent_mode.label())
                                .on_click(cx.listener(Self::toggle_agent_mode_picker)),
                        )
                        .when(self.agent_mode_picker_open, |element| {
                            element.child(self.render_agent_mode_picker(cx))
                        })
                        .child(
                            div()
                                .id("composer-model-picker")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .text_color(rgb(0xb7c0d0))
                                .cursor_pointer()
                                .child(format!("Model  {}", self.model.as_str()))
                                .on_click(cx.listener(Self::toggle_model_picker)),
                        )
                        .when(self.model_picker_open, |element| {
                            element.child(self.render_model_picker(cx))
                        })
                        .when(self.sending_message, |element| {
                            element
                                .child(div().text_xs().text_color(rgb(0x64748b)).child("Working…"))
                        }),
                ),
            )
    }

    pub(crate) fn render_rename_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dialog) = &self.rename_dialog else {
            return div().into_any();
        };
        div()
            .id("rename-dialog")
            .absolute()
            .top(px(120.))
            .left(px(280.))
            .w(px(420.))
            .p_3()
            .rounded_lg()
            .bg(rgb(0x1b1d24))
            .border_1()
            .border_color(rgb(0x3b4555))
            .shadow_lg()
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf3f4f6))
                    .child("Rename session"),
            )
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child(format!("Current name: {}", dialog.session.name)),
            )
            .child(
                div()
                    .mt_3()
                    .w_full()
                    .min_h(px(34.))
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(0x0f1115))
                    .border_1()
                    .border_color(rgb(0x3b4555))
                    .text_color(rgb(0xe5e7eb))
                    .key_context("RenameDialog")
                    .track_focus(&self.rename_focus_handle)
                    .cursor(CursorStyle::IBeam)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::focus_rename))
                    .on_action(cx.listener(Self::backspace))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::left))
                    .on_action(cx.listener(Self::right))
                    .on_action(cx.listener(Self::select_all))
                    .on_action(cx.listener(Self::home))
                    .on_action(cx.listener(Self::end))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(Self::copy))
                    .on_action(cx.listener(Self::submit))
                    .child(TextInputElement {
                        view: cx.entity(),
                        field: InputField::Rename,
                    }),
            )
            .child(
                div()
                    .mt_3()
                    .flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        div()
                            .id("cancel-rename")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x242833))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_sm()
                            .cursor_pointer()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename_dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("confirm-rename")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x2563eb))
                            .text_sm()
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .child("Rename")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm_rename(cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any()
    }

    pub(crate) fn render_github_login_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(state) = &self.github_login else {
            return div().into_any();
        };
        let body = match state {
            GitHubLoginState::Starting => div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Requesting a GitHub device code..."),
            GitHubLoginState::Awaiting {
                verification_uri,
                user_code,
                expires_in,
            } => div()
                .text_sm()
                .text_color(rgb(0xe5e7eb))
                .child("Open this URL in a browser:")
                .child(
                    div()
                        .mt_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_color(rgb(0x93c5fd))
                                .child(verification_uri.clone()),
                        )
                        .child(
                            div()
                                .id("open-github-verification-url")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x2563eb))
                                .hover(|style| style.bg(rgb(0x1d4ed8)))
                                .text_xs()
                                .text_color(rgb(0xffffff))
                                .cursor_pointer()
                                .child("Open")
                                .on_click({
                                    let verification_uri = verification_uri.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        if let Err(error) = open_external_url(&verification_uri) {
                                            this.record_status(format!(
                                                "Could not open GitHub URL: {error}"
                                            ));
                                        }
                                        cx.notify();
                                    })
                                }),
                        )
                        .child(
                            div()
                                .id("copy-github-verification-url")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .cursor_pointer()
                                .child("Copy")
                                .on_click({
                                    let verification_uri = verification_uri.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.copy_github_login_value(
                                            verification_uri.clone(),
                                            "verification URL",
                                            cx,
                                        );
                                    })
                                }),
                        ),
                )
                .child(
                    div()
                        .mt_3()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_color(rgb(0xfef3c7))
                                .child(format!("Enter code: {user_code}")),
                        )
                        .child(
                            div()
                                .id("copy-github-user-code")
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x20242c))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_xs()
                                .cursor_pointer()
                                .child("Copy code")
                                .on_click({
                                    let user_code = user_code.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.copy_github_login_value(
                                            user_code.clone(),
                                            "device code",
                                            cx,
                                        );
                                    })
                                }),
                        ),
                )
                .child(
                    div()
                        .mt_1()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(format!(
                            "Waiting for authorization (expires in {expires_in}s)"
                        )),
                ),
            GitHubLoginState::Completing => div()
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Authorization received. Saving credentials..."),
            GitHubLoginState::Success => div()
                .text_sm()
                .text_color(rgb(0x9ad7bd))
                .child("GitHub Copilot is connected. You can now use its models."),
            GitHubLoginState::Error(error) => div()
                .text_sm()
                .text_color(rgb(0xfca5a5))
                .child(error.clone()),
        };
        div()
            .id("github-login-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(transparent_black())
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf3f4f6))
                    .child("Log in to GitHub Copilot"),
            )
            .child(
                div()
                    .absolute()
                    .top(px(24.))
                    .right(px(24.))
                    .id("close-github-login")
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x20242c))
                    .hover(|style| style.bg(rgb(0x293244)))
                    .text_xs()
                    .text_color(rgb(0xb7c0d0))
                    .cursor_pointer()
                    .child("Back to session")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.github_login = None;
                        cx.notify();
                    })),
            )
            .child(div().mt_4().child(body))
            .into_any()
    }

    pub(crate) fn render_settings_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div().flex().flex_col().gap_1();
        if self.models.is_empty() {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("No configured models are available."),
            );
        } else {
            for (index, model) in self.models.iter().enumerate() {
                let model = model.clone();
                let selected = model == self.default_model;
                body = body.child(
                    div()
                        .id(("default-model", index))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(if selected {
                            rgb(0x293244)
                        } else {
                            rgb(0x20242c)
                        })
                        .text_sm()
                        .text_color(if selected {
                            rgb(0xf3f4f6)
                        } else {
                            rgb(0xb7c0d0)
                        })
                        .cursor_pointer()
                        .child(model.as_str().to_owned())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_default_model(model.clone(), cx);
                        })),
                );
            }
        }
        div()
            .id("settings-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .p_6()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().text_color(rgb(0xf3f4f6)).child("Settings"))
                    .child(
                        div()
                            .id("close-settings")
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_xs()
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .child("Back to session")
                            .on_click(cx.listener(Self::close_settings)),
                    ),
            )
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("DEFAULT MODEL FOR NEW SESSIONS"),
            )
            .child(
                div()
                    .id("settings-refresh-models")
                    .mt_3()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x20242c))
                    .hover(|style| style.bg(rgb(0x293244)))
                    .text_xs()
                    .text_color(rgb(0xb7c0d0))
                    .cursor_pointer()
                    .child("Refresh available models")
                    .on_click(cx.listener(Self::refresh_models_button)),
            )
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("THEME"),
            )
            .child(
                div()
                    .mt_2()
                    .flex()
                    .gap_1()
                    .children(
                        ThemeChoice::ALL
                            .into_iter()
                            .enumerate()
                            .map(|(index, choice)| {
                                let selected = choice == self.theme_choice;
                                div()
                                    .id(("theme-choice", index))
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(if selected {
                                        rgb(0x293244)
                                    } else {
                                        rgb(0x20242c)
                                    })
                                    .text_xs()
                                    .text_color(if selected {
                                        rgb(0xf3f4f6)
                                    } else {
                                        rgb(0xb7c0d0)
                                    })
                                    .cursor_pointer()
                                    .child(choice.label())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.select_theme(choice, window, cx);
                                    }))
                            }),
                    ),
            )
            .child(div().mt_2().child(body))
            .into_any()
    }
}

impl Render for LoomView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.schedule_run_poll(cx);
        let view = cx.entity();
        let (backend_label, backend_color) = match &self.backend_status {
            BackendStatus::Connected => (
                format!("{} / connected", self.connection.description()),
                rgb(0x9ad7bd),
            ),
            BackendStatus::Error(error) => (
                if error.retryable {
                    format!("{} / retryable error", self.connection.description())
                } else {
                    format!("{} / backend error", self.connection.description())
                },
                rgb(0xfca5a5),
            ),
        };
        let project_name = self
            .project
            .as_ref()
            .map(|project| project.name.as_str())
            .unwrap_or("Project");
        let login_label = match (&self.github_login, self.github_connected) {
            (_, true) => "GitHub connected",
            (None, false) => "Log in",
            (Some(GitHubLoginState::Success), false) => "GitHub connected",
            (Some(_), false) => "GitHub login...",
        };
        let decorations = window.window_decorations();
        let client_decorated = matches!(decorations, Decorations::Client { .. });
        let shadow_size = CLIENT_DECORATION_SHADOW;
        let mut tiling = match decorations {
            Decorations::Client { tiling } => tiling,
            Decorations::Server => Tiling::default(),
        };
        if window.is_maximized() || window.is_fullscreen() {
            tiling = Tiling::tiled();
        }
        let decoration_inset = if tiling.is_tiled() {
            px(0.)
        } else {
            shadow_size
        };
        if client_decorated {
            window.set_client_inset(shadow_size);
        }
        let content = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .text_size(px(13.))
            .child(TextSelectionLayer)
            .child(
                div()
                    .h(px(40.))
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(rgb(0x1b1d24))
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .rounded_client_top(client_decorated, tiling)
                    .child(
                        div()
                            .id("window-titlebar-drag")
                            .flex()
                            .flex_1()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .window_control_area(WindowControlArea::Drag)
                            .on_mouse_down(MouseButton::Left, |event, window, _| {
                                if event.click_count == 2 {
                                    window.zoom_window();
                                } else {
                                    window.start_window_move();
                                }
                            })
                            .child("Loom")
                            .child(div().text_sm().text_color(rgb(0x8f98a6)).child(format!(
                                "{}  /  {}",
                                project_name, self.active_session.name
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .text_color(backend_color)
                            .child(format!(
                                "{}  {}",
                                session_state_name(self.session_state),
                                backend_label
                            ))
                            .when(self.login_enabled, |element| {
                                element.child(
                                    div()
                                        .id("github-login")
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .bg(rgb(0x20242c))
                                        .hover(|style| style.bg(rgb(0x293244)))
                                        .text_xs()
                                        .cursor_pointer()
                                        .child(login_label)
                                        .on_click(cx.listener(Self::toggle_github_login)),
                                )
                            })
                            .child(
                                div().flex().items_center().gap_1().ml_2().child(
                                    div()
                                        .id("window-close")
                                        .w(px(22.))
                                        .h(px(22.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_sm()
                                        .text_sm()
                                        .text_color(rgb(0xfca5a5))
                                        .hover(|style| style.bg(rgb(0x7f1d1d)))
                                        .cursor_pointer()
                                        .tooltip(|_, cx| {
                                            cx.new(|_| LoomTooltip {
                                                text: "Close window".into(),
                                            })
                                            .into()
                                        })
                                        .child(Icon::new(IconName::Close).size_4())
                                        .on_click(|_, window, _| {
                                            window.remove_window();
                                        }),
                                ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(250.))
                            .h_full()
                            .relative()
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .bg(rgb(0x17191f))
                            .border_r_1()
                            .border_color(rgb(0x30343f))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div().text_xs().text_color(rgb(0x93c5fd)).child("SESSIONS"),
                                    )
                                    .child(
                                        Button::new("top-menu-button")
                                            .icon(Icon::new(IconName::Ellipsis))
                                            .ghost()
                                            .dropdown_menu({
                                                let view = view.clone();
                                                move |menu, _, _| {
                                                    menu.item(
                                                        PopupMenuItem::new("Settings").on_click({
                                                            let view = view.clone();
                                                            move |_, _, cx| {
                                                                view.update(cx, |view, cx| {
                                                                    view.open_settings_from_menu(
                                                                        cx,
                                                                    );
                                                                });
                                                            }
                                                        }),
                                                    )
                                                }
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div().text_xs().text_color(rgb(0x93c5fd)).child("SESSIONS"),
                                    )
                                    .child(
                                        div()
                                            .id("new-session")
                                            .w(px(26.))
                                            .h(px(26.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_sm()
                                            .bg(rgb(0x20242c))
                                            .hover(|style| style.bg(rgb(0x293244)))
                                            .text_sm()
                                            .text_color(rgb(0x94a3b8))
                                            .cursor_pointer()
                                            .tooltip(|_, cx| {
                                                cx.new(|_| LoomTooltip {
                                                    text: "Create session".into(),
                                                })
                                                .into()
                                            })
                                            .child(Icon::new(IconName::Plus).size_4())
                                            .on_click(cx.listener(Self::new_session)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .id("session-list")
                                    .overflow_y_scroll()
                                    .child(self.render_session_list(cx)),
                            )
                            .child(
                                div()
                                    .pt_2()
                                    .border_t_1()
                                    .border_color(rgb(0x30343f))
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child(format!(
                                        "{} sessions  |  {}",
                                        self.sessions.len(),
                                        self.workspace_root.display()
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .relative()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .child(
                                div()
                                    .w_full()
                                    .px_3()
                                    .py_2()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .bg(rgb(0x14161a))
                                    .border_b_1()
                                    .border_color(rgb(0x30343f))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .child(self.active_session.name.clone())
                                            .child(
                                                div().text_xs().text_color(rgb(0x8f98a6)).child(
                                                    format!(
                                                        "{}  |  {}  |  {} model{}",
                                                        self.run_state
                                                            .map(run_state_name)
                                                            .unwrap_or("idle"),
                                                        self.model.as_str(),
                                                        self.models.len(),
                                                        if self.models.len() == 1 {
                                                            ""
                                                        } else {
                                                            "s"
                                                        }
                                                    ),
                                                ),
                                            ),
                                    )
                                    .when(false, |element| {
                                        element.child(
                                            div()
                                                .flex()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .id("review-changes")
                                                        .w(px(28.))
                                                        .h(px(26.))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .rounded_sm()
                                                        .bg(rgb(0x20242c))
                                                        .hover(|style| style.bg(rgb(0x293244)))
                                                        .text_sm()
                                                        .text_color(rgb(0x94a3b8))
                                                        .cursor_pointer()
                                                        .tooltip(|_, cx| {
                                                            cx.new(|_| LoomTooltip {
                                                                text: "Changed files".into(),
                                                            })
                                                            .into()
                                                        })
                                                        .child(
                                                            Icon::new(IconName::FileText).size_4(),
                                                        )
                                                        .on_click(cx.listener(
                                                            |this, event, window, cx| {
                                                                this.show_review(
                                                                    ReviewPanel::Changes,
                                                                    event,
                                                                    window,
                                                                    cx,
                                                                )
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .id("review-diff")
                                                        .w(px(28.))
                                                        .h(px(26.))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .rounded_sm()
                                                        .bg(rgb(0x20242c))
                                                        .hover(|style| style.bg(rgb(0x293244)))
                                                        .text_sm()
                                                        .text_color(rgb(0x94a3b8))
                                                        .cursor_pointer()
                                                        .tooltip(|_, cx| {
                                                            cx.new(|_| LoomTooltip {
                                                                text: "Read-only diff".into(),
                                                            })
                                                            .into()
                                                        })
                                                        .child(
                                                            Icon::new(IconName::FileText).size_4(),
                                                        )
                                                        .on_click(cx.listener(
                                                            |this, event, window, cx| {
                                                                this.show_review(
                                                                    ReviewPanel::Diff,
                                                                    event,
                                                                    window,
                                                                    cx,
                                                                )
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .id("review-evidence")
                                                        .w(px(28.))
                                                        .h(px(26.))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .rounded_sm()
                                                        .bg(rgb(0x20242c))
                                                        .hover(|style| style.bg(rgb(0x293244)))
                                                        .text_sm()
                                                        .text_color(rgb(0x94a3b8))
                                                        .cursor_pointer()
                                                        .tooltip(|_, cx| {
                                                            cx.new(|_| LoomTooltip {
                                                                text: "Task evidence".into(),
                                                            })
                                                            .into()
                                                        })
                                                        .child(Icon::new(IconName::Check).size_4())
                                                        .on_click(cx.listener(
                                                            |this, event, window, cx| {
                                                                this.show_review(
                                                                    ReviewPanel::Evidence,
                                                                    event,
                                                                    window,
                                                                    cx,
                                                                )
                                                            },
                                                        )),
                                                )
                                                .when(
                                                    self.workspace_root
                                                        .join("Cargo.toml")
                                                        .is_file(),
                                                    |element| {
                                                        element
                                                            .child(
                                                                div()
                                                                    .id("run-build")
                                                                    .w(px(28.))
                                                                    .h(px(26.))
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .rounded_sm()
                                                                    .bg(rgb(0x20242c))
                                                                    .hover(|style| {
                                                                        style.bg(rgb(0x293244))
                                                                    })
                                                                    .text_sm()
                                                                    .text_color(rgb(0x94a3b8))
                                                                    .cursor_pointer()
                                                                    .tooltip(|_, cx| {
                                                                        cx.new(|_| LoomTooltip {
                                                                            text: "Run cargo check"
                                                                                .into(),
                                                                        })
                                                                        .into()
                                                                    })
                                                                    .child(
                                                                        Icon::new(IconName::Check)
                                                                            .size_4(),
                                                                    )
                                                                    .on_click(
                                                                        cx.listener(
                                                                            |_, _, _, _| {},
                                                                        ),
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .id("run-tests")
                                                                    .w(px(28.))
                                                                    .h(px(26.))
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .rounded_sm()
                                                                    .bg(rgb(0x20242c))
                                                                    .hover(|style| {
                                                                        style.bg(rgb(0x293244))
                                                                    })
                                                                    .text_sm()
                                                                    .text_color(rgb(0x94a3b8))
                                                                    .cursor_pointer()
                                                                    .tooltip(|_, cx| {
                                                                        cx.new(|_| LoomTooltip {
                                                                            text: "Run cargo test"
                                                                                .into(),
                                                                        })
                                                                        .into()
                                                                    })
                                                                    .child("T")
                                                                    .on_click(
                                                                        cx.listener(
                                                                            |_, _, _, _| {},
                                                                        ),
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .id("run-lint")
                                                                    .w(px(28.))
                                                                    .h(px(26.))
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .rounded_sm()
                                                                    .bg(rgb(0x20242c))
                                                                    .hover(|style| {
                                                                        style.bg(rgb(0x293244))
                                                                    })
                                                                    .text_sm()
                                                                    .text_color(rgb(0x94a3b8))
                                                                    .cursor_pointer()
                                                                    .tooltip(|_, cx| {
                                                                        cx.new(|_| LoomTooltip {
                                                                            text:
                                                                                "Run cargo clippy"
                                                                                    .into(),
                                                                        })
                                                                        .into()
                                                                    })
                                                                    .child(
                                                                        Icon::new(
                                                                            IconName::FileText,
                                                                        )
                                                                        .size_4(),
                                                                    )
                                                                    .on_click(
                                                                        cx.listener(
                                                                            |_, _, _, _| {},
                                                                        ),
                                                                    ),
                                                            )
                                                    },
                                                )
                                                .when(false, |element| element),
                                        )
                                    })
                                    .when(!self.review.open, |element| {
                                        element.child(
                                            Button::new("toggle-review-sidebar")
                                                .icon(Icon::new(IconName::PanelRight))
                                                .ghost()
                                                .xsmall()
                                                .on_click(cx.listener(
                                                    |this, event, window, cx| {
                                                        this.show_review(
                                                            ReviewPanel::Changes,
                                                            event,
                                                            window,
                                                            cx,
                                                        );
                                                    },
                                                )),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .id("timeline-scroll")
                                    .overflow_y_scroll()
                                    .child(self.render_timeline(cx)),
                            )
                            .child(self.render_composer(cx))
                            .when(self.rename_dialog.is_some(), |element| {
                                element.child(self.render_rename_dialog(cx))
                            })
                            .when(self.settings_open, |element| {
                                element.child(self.render_settings_dialog(cx))
                            })
                            .when(self.github_login.is_some(), |element| {
                                element.child(self.render_github_login_dialog(cx))
                            }),
                    )
                    .when(
                        self.review.open && !self.settings_open && self.github_login.is_none(),
                        |element| element.child(self.render_review(cx)),
                    ),
            )
            .child(
                div()
                    .h(px(24.))
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .bg(rgb(0x1b1d24))
                    .border_t_1()
                    .border_color(rgb(0x30343f))
                    .rounded_client_bottom(client_decorated, tiling)
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child(format!(
                        "{}  |  {} timeline items  |  {}",
                        session_state_name(self.session_state),
                        self.timeline.len(),
                        if self.demo_workspace {
                            "demo workspace"
                        } else {
                            "workspace"
                        }
                    )),
            );
        let content = content.rounded_client_corners(client_decorated, tiling);
        match decorations {
            Decorations::Server => div().size_full().child(content),
            Decorations::Client { .. } => div()
                .size_full()
                .bg(transparent_black())
                .when(!tiling.top, |element| element.pt(shadow_size))
                .when(!tiling.bottom, |element| element.pb(shadow_size))
                .when(!tiling.left, |element| element.pl(shadow_size))
                .when(!tiling.right, |element| element.pr(shadow_size))
                .child(
                    div()
                        .size_full()
                        .rounded_client_corners(true, tiling)
                        .when(!tiling.is_tiled(), |element| {
                            element.shadow(vec![gpui::BoxShadow {
                                color: gpui::hsla(0., 0., 0., 0.4),
                                blur_radius: shadow_size / 2.,
                                spread_radius: px(0.),
                                offset: point(px(0.), px(0.)),
                                inset: false,
                            }])
                        })
                        .child(content),
                )
                .child(
                    canvas(
                        |_bounds, window, _cx| {
                            window.insert_hitbox(
                                Bounds::new(
                                    point(px(0.), px(0.)),
                                    window.window_bounds().get_bounds().size,
                                ),
                                HitboxBehavior::Normal,
                            )
                        },
                        move |_bounds, hitbox, window, _cx| {
                            let size = window.window_bounds().get_bounds().size;
                            let Some(edge) =
                                resize_edge(window.mouse_position(), decoration_inset, size)
                            else {
                                return;
                            };
                            window.set_cursor_style(
                                match edge {
                                    ResizeEdge::Top | ResizeEdge::Bottom => {
                                        CursorStyle::ResizeUpDown
                                    }
                                    ResizeEdge::Left | ResizeEdge::Right => {
                                        CursorStyle::ResizeLeftRight
                                    }
                                    ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                                        CursorStyle::ResizeUpLeftDownRight
                                    }
                                    ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                                        CursorStyle::ResizeUpRightDownLeft
                                    }
                                },
                                &hitbox,
                            );
                        },
                    )
                    .size_full()
                    .absolute(),
                )
                .on_mouse_move(|_, window, _| window.refresh())
                .on_mouse_down(MouseButton::Left, move |event, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    if let Some(edge) = resize_edge(event.position, decoration_inset, size) {
                        window.start_window_resize(edge);
                    }
                }),
        }
    }
}
