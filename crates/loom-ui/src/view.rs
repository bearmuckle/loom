//! The GPUI view: session navigator, run canvas, composer, and review drawer.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gpui_kit::TestSupportExt as _;
use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::base::{Disableable, Selectable, TextSelectionLayer};
use gpui_kit::component::bubble::{Bubble, BubbleContent, BubbleVariant};
use gpui_kit::component::button::{Button, ButtonVariants};
// Only the wasm disconnected screen still uses the dialog component.
#[cfg(target_family = "wasm")]
use gpui_kit::component::dialog::Dialog;
use gpui_kit::component::input::{
    Input as KitInput, InputEvent, InputState, Textarea, TextareaState,
};
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    Icon, IconName, IndexPath, Sizable,
    badge::Badge,
    h_resizable,
    menu::{DropdownMenu, PopupMenu, PopupMenuItem},
    message::MessageAlignment,
    resizable_panel,
    select::{SearchableVec, Select, SelectEvent, SelectState},
    switch::Switch,
    tab::{Tab, TabBar},
    text::TextView,
};
use gpui_kit::{
    Animation, AnimationExt, App, AsyncApp, ClickEvent, ClipboardItem, Context, Element, Entity,
    FocusHandle, Focusable, FontWeight, HighlightStyle, MouseButton, Pixels, Render, Rgba,
    StyledText, Subscription, WeakEntity, Window, WindowAppearance, WindowControlArea, div,
    prelude::*, px,
};
use loom_core::{
    ActivityId, AgentSessionId, AgentSessionSnapshot, AgentSessionState, CapabilitySet, ErrorCode,
    EventSequence, LoomError, ProtocolVersion, RepositoryId, RunId, Timestamp, ToolCallId,
    UsageSnapshot, WorkspaceId, WorkspaceRecord,
};
#[cfg(not(target_family = "wasm"))]
use loom_model::GITHUB_COPILOT_DEFAULT_MODEL;
use loom_model::{MessageRole, ModelId, ModelMessage, ProviderKind, ProviderSummary, ToolCall};
use loom_protocol::GitHubCopilotLoginStatus;
use loom_protocol::{
    AgentActivityData, AgentActivityRecord, AgentActivityStatus, AgentEvent, AgentRunSnapshot,
    AgentRunSnapshotProjection, AgentRunState, CURRENT_PROTOCOL_VERSION, ClientRequest,
    ClonedRepository, ContextRequest, ContextResponse, ControlRequest, ControlResponse,
    EventsRequest, EventsResponse, FileActivityOperation, FilesystemRequest, FilesystemResponse,
    GITHUB_REPOSITORY_QUERY_MIN_CHARS, GitFileStatusKind, GitHubRepository, GitRepositoryStatus,
    MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE, ProjectChildControlAction, ProjectRequest, ProjectResponse,
    ProviderRequest, ProviderResponse, RepositoryRequest, RepositoryResponse, RequestEnvelope,
    ResponseEnvelope, RunRequest, RunResponse, ServerEvent, ServerResponse, SessionDirectory,
    SessionRepository, SessionRequest, SessionResponse, TaskRequest, TerminalRequest, UsageRequest,
    UsageResponse, WorkerNodeConfig, WorkerNodeResources, WorkerNodeStatus, WorkspaceConfig,
    WorkspaceFeedEvent, WorkspaceRequest, WorkspaceResponse,
};
#[cfg(not(target_family = "wasm"))]
#[cfg(not(target_family = "wasm"))]
#[cfg(not(target_family = "wasm"))]
use std::fs;

use crate::{
    MAX_REVIEW_CHANGES, MAX_REVIEW_DIFF,
    connection::{BackendWorker, ClientConnection, ConnectionCleanupGuard},
    preferences::UiPreferences,
    state::{
        AssistantPart, AssistantTurn, EvidenceText, GitHubLoginKind, GitHubLoginState,
        InspectorTab, OutputDetail, PlanState, PlanStepStatus, ProjectDeleteDialogState,
        RenameDialogState, ReviewState, SystemNote, SystemTone, ThemeChoice, TimelineItem,
        ToolPart, ToolPartStatus, UsageState, bounded, bounded_to, finish_assistant_turn,
        has_tool_part, push_assistant_evidence, push_assistant_reasoning, push_assistant_text,
        session_state_for_run, upsert_tool_part,
    },
    syntax::{self, Language},
    theme::{
        BASE_FONT_SIZE, CONVERSATION_FONT_SIZE, ERROR_CARD_ACCENT, ERROR_CARD_FOREGROUND,
        ERROR_CARD_SURFACE, change_color, mono_font, mono_size, rgb,
    },
};

#[cfg(not(target_family = "wasm"))]
use crate::connection::{
    attach_session_repository, create_session_in_workspace, create_workspace, current_participant,
    list_models_from_backend, list_workspace_sessions, list_workspaces, redact_secret,
    unexpected_response,
};
#[cfg(target_family = "wasm")]
use crate::connection::{
    attach_session_repository_async, create_session_in_workspace_async, create_workspace_async,
    current_participant_async, list_models_from_backend, list_workspace_sessions_async,
    list_workspaces_async, probe_worker_connection,
};
#[cfg(target_family = "wasm")]
use crate::connection::{redact_secret, unexpected_response};

const COMPACT_LAYOUT_WIDTH: Pixels = px(960.);
const PHONE_LAYOUT_WIDTH: Pixels = px(700.);
const COMPACT_SIDEBAR_WIDTH: Pixels = px(210.);
const FULL_SIDEBAR_WIDTH: Pixels = px(260.);
const COMPACT_REVIEW_WIDTH: Pixels = px(400.);
const FULL_REVIEW_WIDTH: Pixels = px(540.);
const TIMELINE_CONTENT_MAX_WIDTH: Pixels = px(680.);
pub(crate) const DEFAULT_FONT_SCALE_PERCENT: u16 = 100;
pub(crate) const MIN_FONT_SCALE_PERCENT: u16 = 75;
pub(crate) const MAX_FONT_SCALE_PERCENT: u16 = 150;
const FONT_SCALE_STEP_PERCENT: i16 = 5;
/// Consecutive tool calls of the same kind collapse into one summary row once
/// they reach this count.
const TOOL_GROUP_THRESHOLD: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ResponsiveLayout {
    phone: bool,
    sidebar_width: Pixels,
    review_width: Pixels,
}

/// The kind of inline completion the composer is offering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionKind {
    Command,
    File,
    /// A person in the session roster, so `@name` reaches people and is
    /// discoverable while typing.
    Person,
}

/// The active pane in the settings dialog's section navigation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingsSection {
    Agents,
    Providers,
    Workers,
    Appearance,
    About,
}

const SETTINGS_SECTIONS: [(SettingsSection, &str); 5] = [
    (SettingsSection::Agents, "Agents"),
    (SettingsSection::Providers, "Providers"),
    (SettingsSection::Workers, "Workers"),
    (SettingsSection::Appearance, "Appearance"),
    (SettingsSection::About, "About"),
];

/// The one surface that covers the conversation, if any.
///
/// At most one of these is shown at a time, and which one is a property of the
/// view's [`CoveringSurface`] value rather than a convention the openers and the
/// renderer have to agree on: each opener sets the value, the renderer draws the
/// surface the value names, and every check for "is this on screen" asks the
/// same value. About and Providers are the settings dialog with another section,
/// so they are this one surface with its section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoveringSurface {
    /// The settings dialog, on the section it shows. About and Providers are
    /// this surface; only the section differs.
    Settings(SettingsSection),
    /// The archived-session management surface.
    ArchivedSessions,
    /// The new-project pane, which is also how a source is attached to a
    /// session.
    NewProject,
    /// The GitHub device-login pane.
    GitHubLogin,
    /// The rename dialog.
    Rename,
}

impl CoveringSurface {
    /// Every covering surface, so a test can open each one and each pair. The
    /// `id` match beside it is exhaustive, so a new variant has to be named
    /// there and here, and has to satisfy the exclusivity test.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::Settings(SettingsSection::Agents),
        Self::ArchivedSessions,
        Self::NewProject,
        Self::GitHubLogin,
        Self::Rename,
    ];

    /// The id of the surface's own root element, which is what a render test
    /// looks for to tell which surface is on screen.
    #[cfg(test)]
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Settings(_) => "settings-dialog",
            Self::ArchivedSessions => "archived-sessions-dialog",
            Self::NewProject => "source-dialog-pane",
            Self::GitHubLogin => "github-login-dialog",
            Self::Rename => "rename-dialog",
        }
    }

    /// Whether this surface takes the width the review column would use, so the
    /// column stays out of the way while it is shown.
    pub(crate) fn hides_review_panel(self) -> bool {
        matches!(self, Self::Settings(_) | Self::GitHubLogin)
    }
}

/// The active inline completion in the composer, derived from the text before
/// the cursor. Slash commands and `@` file references share one popup.
#[derive(Clone, Debug)]
struct ComposerCompletion {
    kind: CompletionKind,
    query: String,
    selected: usize,
}

/// One entry in a command surface (slash menu or command palette).
#[derive(Clone, Copy, Debug)]
struct CommandSpec {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    shortcut: Option<&'static str>,
}

/// Every command the composer and palette can run.
const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "new",
        title: "New session",
        description: "Start a session from a source",
        shortcut: None,
    },
    CommandSpec {
        name: "repo",
        title: "Add source",
        description: "Attach a repository or folder",
        shortcut: None,
    },
    CommandSpec {
        name: "review",
        title: "Toggle review panel",
        description: "Show the changed files and diffs",
        shortcut: Some("B"),
    },
    CommandSpec {
        name: "stop",
        title: "Stop run",
        description: "Interrupt the active run",
        shortcut: Some("esc"),
    },
    CommandSpec {
        name: "archived",
        title: "Archived sessions",
        description: "Review and delete archived sessions",
        shortcut: None,
    },
    CommandSpec {
        name: "providers",
        title: "Providers",
        description: "Connect and configure model providers",
        shortcut: None,
    },
    CommandSpec {
        name: "settings",
        title: "Settings",
        description: "Themes, workers, and preferences",
        shortcut: Some(","),
    },
    CommandSpec {
        name: "about",
        title: "About Loom",
        description: "Build version, platform, and project links",
        shortcut: None,
    },
    CommandSpec {
        name: "help",
        title: "Help",
        description: "List the available commands",
        shortcut: None,
    },
];

/// Renders a command shortcut with the platform's primary modifier so the UI
/// does not show the macOS symbol on other operating systems.
pub(crate) fn command_shortcut_label(shortcut: &str) -> String {
    if shortcut == "esc" {
        return "esc".to_owned();
    }
    if cfg!(target_os = "macos") {
        format!("⌘{shortcut}")
    } else {
        format!("Ctrl+{shortcut}")
    }
}

/// The command palette shortcut label for the current platform.
pub(crate) fn command_palette_shortcut_label() -> String {
    if cfg!(target_os = "macos") {
        "⌘⇧P".to_owned()
    } else {
        "Ctrl+Shift+P".to_owned()
    }
}

/// The command palette icon: the macOS command glyph only on macOS, since it is
/// meaningless elsewhere.
pub(crate) fn command_palette_icon() -> AssetIconName {
    if cfg!(target_os = "macos") {
        AssetIconName::Command
    } else {
        AssetIconName::Search
    }
}

#[cfg(not(target_family = "wasm"))]
use crate::connection::{
    list_models, negotiate, set_workspace_config, start_run, worker_node_status, workspace_config,
};
#[cfg(target_family = "wasm")]
use crate::{
    browser::BrowserOptions,
    connection::{
        list_models_async, negotiate_async, set_workspace_config_async, worker_node_status_async,
        workspace_config_async,
    },
};
#[cfg(not(target_family = "wasm"))]
use log::info;

use archive::ArchivedSessionsState;
// Only the disconnected screen spells out the lost-connection sentence, and
// that screen exists in the browser and in its tests.
#[cfg(any(target_family = "wasm", test))]
use connectivity::CONNECTION_LOST_COPY;
use connectivity::{
    ConnectivityFailure, Reconnecting, classify_connectivity_failure, error_details,
};
use helpers::*;
#[cfg(not(target_family = "wasm"))]
use loom_local::{
    PeerCredentialStore, RemoteConnectionOptions, UiOptions, backend_persistence_path,
    prepare_workspace,
};

/// The native remote transport settings implied by the launcher options: the
/// additional CA to trust for `wss://`, and whether plaintext `ws://` beyond
/// loopback was explicitly allowed.
#[cfg(not(target_family = "wasm"))]
fn remote_connection_options(options: &UiOptions) -> RemoteConnectionOptions {
    RemoteConnectionOptions {
        ca_certificate: options.ca.clone(),
        allow_insecure_remote: options.allow_insecure_remote,
    }
}

#[cfg(target_family = "wasm")]
use futures_channel::oneshot;
#[cfg(target_family = "wasm")]
use wasm_bindgen::{JsCast, closure::Closure};

type ModelSelectState = SelectState<SearchableVec<String>>;

const ACTIVE_BACKEND_NODE_ENTRY_ID: u64 = 0;

#[cfg(target_family = "wasm")]
async fn browser_delay(duration: Duration) -> Result<(), LoomError> {
    let window = web_sys::window().ok_or_else(|| {
        LoomError::new(
            ErrorCode::Internal,
            "could not schedule a browser timer",
            false,
        )
    })?;
    let (sender, receiver) = oneshot::channel();
    let callback = Closure::once(move || {
        let _ = sender.send(());
    });
    window
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            callback.as_ref().unchecked_ref(),
            duration.as_millis().min(i32::MAX as u128) as i32,
        )
        .map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "could not schedule a browser timer",
                false,
            )
        })?;
    callback.forget();
    receiver.await.map_err(|_| {
        LoomError::new(
            ErrorCode::Internal,
            "browser timer ended before it completed",
            false,
        )
    })
}

type TranscriptMessage = (u64, u64, ModelMessage);
type TranscriptPage = (Vec<TranscriptMessage>, Option<u64>, bool);

async fn load_transcript_page(
    backend: BackendWorker,
    run_id: RunId,
    before_ordinal: Option<u64>,
) -> Result<TranscriptPage, LoomError> {
    let response = backend
        .submit(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRunTranscriptPage {
                run_id,
                before_ordinal,
                limit: MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE,
            },
        )))
        .wait()
        .await;
    let (messages, next_before, has_older) = match response.result? {
        ServerResponse::Run(RunResponse::AgentRunTranscriptPage {
            run_id: response_run_id,
            messages,
            next_before,
            has_older,
        }) if response_run_id == run_id => (messages, next_before, has_older),
        response => {
            return Err(unexpected_response("run transcript page", response));
        }
    };
    Ok((
        messages
            .into_iter()
            .map(|message| (message.ordinal, message.timeline_ordinal, message.message))
            .collect(),
        next_before,
        has_older,
    ))
}

#[derive(Clone)]
struct WorkerNodeEntry {
    id: u64,
    status: WorkerNodeStatus,
    is_local: bool,
    url: Option<String>,
    connection: Option<ClientConnection>,
    connection_state: WorkerConnectionState,
    connection_detail: Option<String>,
    severe_load_streak: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkerConnectionStage {
    #[cfg(target_family = "wasm")]
    Bootstrap,
    InputValidation,
    Transport,
    Negotiation,
    Status,
    CredentialRead,
    CredentialSave,
    BootstrapSave,
}

impl WorkerNodeEntry {
    fn is_local(&self) -> bool {
        self.is_local
    }
}

pub(crate) struct LoomView {
    #[cfg(target_family = "wasm")]
    connected: bool,
    #[cfg(target_family = "wasm")]
    browser_demo_mode: bool,
    /// Used for the synchronous bootstrap before the window exists.
    pub(crate) connection: ClientConnection,
    #[cfg(not(target_family = "wasm"))]
    owned_backend: Option<crate::local_backend::OwnedBackend>,
    /// The negotiated protocol version of the active backend, or `None` when no
    /// negotiation result is retained (for example a disconnected browser view).
    server_protocol_version: Option<ProtocolVersion>,
    /// The active backend's endpoint URL: `Some` for a native `--remote` or a
    /// browser client, `None` for the in-process and demo backends. Only the URL
    /// is stored; the access token is never part of view state.
    backend_endpoint: Option<String>,
    /// TLS and plaintext-opt-in settings applied to every native remote
    /// connection this view opens, including later worker-node reconnects.
    #[cfg(not(target_family = "wasm"))]
    remote_options: RemoteConnectionOptions,
    /// Whether this view is the browser client. A plain field rather than `cfg!`
    /// so the About pane can render and be tested in the browser configuration
    /// on native builds too.
    browser_client: bool,
    /// Used for every request made once the view is interactive.
    pub(crate) backend: BackendWorker,
    /// The startup backend remains the default for workspace requests and new sessions.
    default_backend_node_id: String,
    /// Connections are keyed by the backend's stable node identity.
    node_backends: BTreeMap<String, BackendWorker>,
    /// Last known display names remain available for sessions after node removal.
    node_names: BTreeMap<String, String>,
    /// Sessions stay pinned to the node that created them.
    session_node_ids: BTreeMap<AgentSessionId, String>,
    pub(crate) workspace_id: WorkspaceId,
    workspaces: Vec<WorkspaceRecord>,
    /// Workspaces reported by each worker node. A workspace belongs to exactly
    /// one worker, so the client never shares one across nodes; this is used to
    /// target creation and config at the worker's own workspace.
    node_workspaces: BTreeMap<String, Vec<WorkspaceRecord>>,
    local_directory_sources_available: bool,
    /// Native local worker working directory used to pre-fill the New project
    /// dialog. `None` for remote and browser clients, for the demo workspace,
    /// and when the launch directory was Loom's own state directory.
    local_current_directory: Option<PathBuf>,
    pub(crate) sessions: Vec<AgentSessionSnapshot>,
    project_snapshot: Option<loom_core::ProjectSnapshot>,
    /// Retain known project hierarchies while selection changes to another root.
    project_tree_snapshots: Vec<loom_core::ProjectSnapshot>,
    project_child_review: Option<(
        loom_core::ProjectWorktreeRecord,
        loom_protocol::GitRepositoryStatus,
        loom_protocol::GitDiff,
    )>,
    project_snapshot_stale: bool,
    /// At most one active-pane snapshot request, across selection changes.
    project_snapshot_refreshing: Option<AgentSessionId>,
    missing_project_snapshots_refreshing: bool,
    project_feed_after_sequence: Option<EventSequence>,
    project_feed_epoch: Option<String>,
    project_poll_scheduled: bool,
    /// The left pane's project list: one row per project, in one scrolling
    /// region shared by every worker section.
    project_list: Option<Entity<ListState<render::sidebar::ProjectListDelegate>>>,
    /// Keeps the list's own selection and confirm events wired to this view.
    project_list_subscription: Option<Subscription>,
    /// The rows, headings, layout, and shared snapshots the list was last
    /// synchronized to.
    project_list_view: Option<render::sidebar::ProjectListView>,
    /// The project row the list was last synchronized to highlight.
    project_list_active: Option<AgentSessionId>,
    /// The main source of each known project, shown as the second line of a
    /// project row: `owner/repository` for a clone, a short path for a folder.
    session_sources: BTreeMap<AgentSessionId, String>,
    /// Sources reported per session while its lookup is in flight, and the set
    /// of sessions already asked about, so a project is only looked up once.
    session_source_lookups: BTreeMap<AgentSessionId, (Option<String>, Option<String>)>,
    /// Serializes background source discovery across render-triggered refreshes.
    session_source_request_in_flight: bool,
    /// Last session-list error per worker node, so a worker that failed to load
    /// is distinguishable from one that simply has no projects.
    node_session_load_errors: BTreeMap<String, String>,
    pub(crate) active_session: AgentSessionSnapshot,
    pub(crate) active_run: Option<AgentRunSnapshot>,
    pub(crate) active_run_id: Option<RunId>,
    context_inspection: Option<loom_protocol::ContextInspection>,
    pub(crate) model: ModelId,
    pub(crate) default_model: ModelId,
    pub(crate) session_models: BTreeMap<AgentSessionId, ModelId>,
    pub(crate) auto_approve_actions: bool,
    session_auto_approve_actions: BTreeMap<AgentSessionId, bool>,
    /// Whether destructive actions pause for approval in this session. Off by
    /// default, so a destructive action runs automatically unless the user asks
    /// to be prompted.
    pub(crate) ask_before_destructive_actions: bool,
    session_ask_before_destructive_actions: BTreeMap<AgentSessionId, bool>,
    pub(crate) session_task_cache: BTreeMap<AgentSessionId, String>,
    pub(crate) optimistic_messages: Vec<String>,
    pub(crate) sending_message: bool,
    /// The latest operational status or backend error, shown as a dismissible
    /// banner above the composer rather than inside the conversation.
    pub(crate) status_banner: Option<SystemNote>,
    /// The client's single reconnecting state. `Some` while the worker is
    /// unreachable, so repeated connectivity failures update one state instead
    /// of replacing the banner with a series of coded errors.
    reconnecting: Option<Reconnecting>,
    /// Whether an automatic reconnect attempt is already waiting on its timer,
    /// so the scheduler never stacks two attempts.
    connectivity_attempt_in_flight: bool,
    /// Whether the status banner's raw details are expanded. A connectivity
    /// failure keeps its codes one disclosure away instead of in the headline.
    pub(crate) status_details_open: bool,
    pub(crate) models: Vec<ModelId>,
    default_models: Vec<ModelId>,
    node_model_catalogs: BTreeMap<String, Vec<ModelId>>,
    node_model_provider_names: BTreeMap<String, BTreeMap<ModelId, String>>,
    model_catalog_node_id: Option<String>,
    model_refreshes_in_flight: BTreeSet<String>,
    /// Wall-clock time of the last model-catalog refresh per node. Used to
    /// throttle on-demand provider discovery from the model selection.
    model_catalog_refreshed_at: BTreeMap<String, Timestamp>,
    model_select: Option<Entity<ModelSelectState>>,
    default_model_select: Option<Entity<ModelSelectState>>,
    model_select_subscription: Option<Subscription>,
    default_model_select_subscription: Option<Subscription>,
    model_select_items: Vec<String>,
    default_model_select_items: Vec<String>,
    model_select_value: Option<String>,
    default_model_select_value: Option<String>,
    model_select_choices: BTreeMap<String, ModelId>,
    default_model_select_choices: BTreeMap<String, ModelId>,
    /// The one covering surface on screen, if any. Set by the openers and read
    /// by the renderer, so the settings dialog, the archived-session surface,
    /// the new-project pane, the GitHub login pane, and the rename dialog can
    /// never be shown at the same time.
    covering_surface: Option<CoveringSurface>,
    pub(crate) providers: Vec<ProviderSummary>,
    providers_node_id: Option<String>,
    provider_api_key_inputs: BTreeMap<loom_model::ProviderId, Entity<InputState>>,
    provider_setup_status: BTreeMap<loom_model::ProviderId, String>,
    pub(crate) theme_choice: ThemeChoice,
    font_scale_percent: u16,
    /// Whether provider reasoning is shown in the transcript. Off by default;
    /// this is presentation-only and never changes what is sent to providers.
    pub(crate) show_reasoning: bool,
    /// How much agent activity the transcript shows. `Medium` is the default and
    /// matches the transcript without this setting; like `show_reasoning` it is
    /// presentation-only and never changes what is sent to providers.
    pub(crate) output_detail: OutputDetail,
    appearance_subscription: Option<Subscription>,
    /// App-wide keystroke interceptor for shortcuts that must work regardless
    /// of which element (if any) holds focus.
    shortcut_interceptor: Option<Subscription>,
    pub(crate) after_sequence: Option<EventSequence>,
    event_stream_epoch: Option<String>,
    pub(crate) timeline: Vec<TimelineItem>,
    transcript_before_ordinal: Option<u64>,
    transcript_loaded_ordinals: BTreeSet<u64>,
    transcript_messages: BTreeMap<u64, (u64, ModelMessage)>,
    transcript_has_older: bool,
    transcript_loading: bool,
    transcript_generation: u64,
    /// Rows added at the head by an older-page load, consumed by the timeline's
    /// virtual list so it can prepend them without jumping the scroll anchor.
    transcript_prepend_count: usize,
    timeline_view: Option<Entity<TimelineView>>,
    /// The active run's plan progress. Rendered in the inspector's Run tab,
    /// never as a timeline row.
    pub(crate) plan: Option<PlanState>,
    /// Whether the Run tab's plan checklist is collapsed to its heading.
    pub(crate) plan_collapsed: bool,
    pub(crate) activity_records: BTreeMap<ActivityId, AgentActivityRecord>,
    /// The user's expansion choice per transcript disclosure, where `true` is
    /// open. A recorded choice always wins over the output detail level and
    /// over a block that opens automatically because its tool is running or
    /// blocked on the user, so the disclosure the user closed stays closed.
    pub(crate) expanded_tools: BTreeMap<ToolCallId, bool>,
    pub(crate) expanded_tool_groups: BTreeMap<u64, bool>,
    pub(crate) expanded_tool_usage: BTreeMap<u64, bool>,
    pub(crate) expanded_reasoning: BTreeMap<u64, bool>,
    pub(crate) approval_request_in_flight: bool,
    approval_settings_request_in_flight: bool,
    pub(crate) archive_request_in_flight: bool,
    /// The archived-sessions management surface: whether it is open, what it
    /// loaded, and the user's force choice per row.
    pub(crate) archived_sessions: ArchivedSessionsState,
    pub(crate) pending_approval: Option<ToolCall>,
    pub(crate) pending_input: Option<String>,
    /// Multi-user identity, roster, composer recipient, run revision, and
    /// credential state for the active session.
    pub(crate) collaboration: crate::collaboration::CollaborationState,
    /// Worker nodes whose transport was opened without an operator token, so a
    /// fresh connection to them is pre-login and may only sign in with GitHub.
    pub(crate) unsigned_nodes: BTreeSet<String>,
    /// Worker nodes known to await a GitHub sign-in: the connection is bound to
    /// no person and the worker refuses everything but signing in. A signed-in
    /// node leaves this set while staying in `unsigned_nodes`, so signing out
    /// returns it to the sign-in prompt.
    pub(crate) awaiting_sign_in_nodes: BTreeSet<String>,
    /// Whether the connected worker advertises `git_hub_sign_in`, which is what
    /// lets a person join the roster by signing in.
    pub(crate) github_sign_in_available: bool,
    /// The node the GitHub sign-in pane is signing in to, remembered from the
    /// control that opened it so a device flow targets one worker.
    pub(crate) github_login_node_id: Option<String>,
    composer_input: Option<Entity<TextareaState>>,
    composer_placeholder: Option<String>,
    composer_completion: Option<ComposerCompletion>,
    pending_completion_accept: bool,
    suppress_completion_once: bool,
    command_palette_open: bool,
    command_palette_input: Option<Entity<InputState>>,
    command_palette_selection: usize,
    session_filter_input: Option<Entity<InputState>>,
    review_filter_input: Option<Entity<InputState>>,
    input_subscriptions: Vec<Subscription>,
    clear_composer_on_render: bool,
    clear_node_on_render: bool,
    /// The requester's note for an access request.
    access_note_input: Option<Entity<InputState>>,
    /// Per-request deny reasons, created lazily for pending requests.
    access_deny_inputs: BTreeMap<loom_protocol::AccessRequestId, Entity<InputState>>,
    clear_access_note: bool,
    clear_access_deny_reason: Option<loom_protocol::AccessRequestId>,
    rename_input_state: Option<Entity<InputState>>,
    source_path_input: Option<Entity<InputState>>,
    repository_filter_input: Option<Entity<InputState>>,
    /// Repositories already cloned on the active worker node, shown in the
    /// source dialog without requiring a search.
    cloned_repositories: Vec<ClonedRepository>,
    cloned_repositories_loading: bool,
    /// Monotonic token that invalidates stale repository search responses.
    repository_search_generation: u64,
    /// The query text of the most recently issued search.
    repository_search_query: String,
    pending_source_path: Option<String>,
    pub(crate) composer_focus_handle: FocusHandle,
    pub(crate) session_state: AgentSessionState,
    pub(crate) run_state: Option<AgentRunState>,
    pub(crate) review: ReviewState,
    session_repositories: Vec<SessionRepository>,
    session_directories: Vec<SessionDirectory>,
    selected_repository_id: Option<RepositoryId>,
    session_drawer_open: bool,
    pub(crate) rename_dialog: Option<RenameDialogState>,
    /// The confirmation an archived project deletion goes through. A whole tree
    /// is never removed without it.
    pub(crate) project_delete_dialog: Option<ProjectDeleteDialogState>,
    source_dialog: Option<SessionSourceDialog>,
    #[cfg(target_family = "wasm")]
    welcome_dialog_dismissed: bool,
    pub(crate) demo_workspace: bool,
    pub(crate) login_enabled: bool,
    pub(crate) github_connected: bool,
    pub(crate) github_repository_connected: bool,
    pub(crate) github_write_access: bool,
    pub(crate) github_login: Option<GitHubLoginState>,
    pub(crate) github_login_kind: GitHubLoginKind,
    worker_nodes: Vec<WorkerNodeEntry>,
    next_worker_node_id: u64,
    worker_node_polls_scheduled: BTreeSet<u64>,
    workspace_config: WorkspaceConfig,
    node_input_initial: String,
    node_input_state: Option<Entity<InputState>>,
    pub(crate) run_poll_scheduled: bool,
    #[cfg(target_family = "wasm")]
    browser_workspace: Option<String>,
    #[cfg(target_family = "wasm")]
    browser_model: Option<ModelId>,
    #[cfg(target_family = "wasm")]
    browser_window_initialized: bool,
    /// Keeps the page-visibility observer alive; a set value also marks the
    /// observer as registered so it is never registered twice.
    #[cfg(target_family = "wasm")]
    visibility_subscription: Option<Subscription>,
    /// Single-flight guard for the resume probe and the reconnect bootstrap,
    /// so overlapping visibility changes cannot start two of either.
    #[cfg(target_family = "wasm")]
    browser_resume_in_flight: bool,
    browser_startup_error: Option<String>,
    /// Whether the browser client is still preparing its first workspace:
    /// connecting, or loading the project it opens. While set, the client shows
    /// one neutral startup screen instead of the settings prompt, the reconnect
    /// screen, or a workspace whose project list and conversation are still
    /// filling in. Always false on native builds, which finish the same work
    /// before the window opens. A plain field (not `cfg(wasm)`) so the shared
    /// load paths can end it without a cfg on every call.
    browser_starting: bool,
    /// Set when the browser transport is known to be closed so the disconnected
    /// screen can offer a reconnect instead of a settings prompt. A plain field
    /// (not `cfg(wasm)`) so its copy can be unit-tested on native builds.
    browser_connection_lost: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionSourceDialogPurpose {
    StartSession,
    AddToSession,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionSourceChoice {
    Empty,
    LocalDirectory,
    GitHub,
}

/// One delegated agent of a project, as listed in that project row's menu.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectAgentRow {
    pub(crate) session_id: AgentSessionId,
    pub(crate) name: String,
    pub(crate) state: AgentSessionState,
}

/// One row of the left pane: a project, built from its root session.
///
/// Delegated agents never get their own row; they are summarised on the row's
/// second line and listed behind that summary's menu.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectListRow {
    /// The session the row selects: the project's root, or the session itself
    /// when no known project snapshot covers it.
    pub(crate) session_id: AgentSessionId,
    pub(crate) name: String,
    /// The project's main source when known, otherwise `updated <when>`.
    pub(crate) detail: String,
    /// The one-line agent summary, omitted when the project has no agents.
    pub(crate) summary: Option<String>,
    pub(crate) agents: Vec<ProjectAgentRow>,
    /// Whether the project or one of its agents has work in flight.
    pub(crate) active: bool,
    /// The project's own session pill. It colours the activity dots.
    pub(crate) status: SessionStatusPill,
    /// The pill shown beside the name, for attention states only.
    pub(crate) attention: Option<SessionStatusPill>,
}

/// The projects owned by one worker, in list order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectListSection {
    pub(crate) node_id: String,
    pub(crate) rows: Vec<ProjectListRow>,
}

/// The left pane's derived content: one row per project, grouped by worker,
/// plus the row that owns the active session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectListModel {
    pub(crate) sections: Vec<ProjectListSection>,
    /// The project row that owns the active session.
    pub(crate) selected: Option<AgentSessionId>,
}

/// Everything the left pane derives its rows from.
///
/// Passed in one struct so the ordering and summary rules stay a pure
/// function that tests can drive without rendering.
pub(crate) struct ProjectListInput<'a> {
    pub(crate) sessions: &'a [AgentSessionSnapshot],
    pub(crate) owners: &'a BTreeMap<AgentSessionId, String>,
    pub(crate) node_order: &'a [String],
    pub(crate) default_node_id: &'a str,
    pub(crate) projects: &'a [&'a loom_core::ProjectSnapshot],
    pub(crate) active_session_id: AgentSessionId,
    /// The main source of each project, shown as the row's second line.
    pub(crate) source_labels: &'a BTreeMap<AgentSessionId, String>,
    /// The lowercased filter text. It matches project names only.
    pub(crate) filter: &'a str,
    pub(crate) now_millis: u64,
}

/// Derive the left pane's project rows.
///
/// A row is built from a project's root session; its delegated agents are
/// summarised on the row instead of being listed. Sessions no known project
/// snapshot covers keep a row of their own, so nothing disappears from the
/// pane. Rows are listed newest-created first, with the incoming list position
/// breaking ties, in the worker order `group_sessions_by_worker` reports.
pub(crate) fn project_list_model(input: ProjectListInput<'_>) -> ProjectListModel {
    let sessions_by_id = input
        .sessions
        .iter()
        .map(|session| (session.id, session))
        .collect::<BTreeMap<_, _>>();
    let positions = input
        .sessions
        .iter()
        .enumerate()
        .map(|(position, session)| (session.id, position))
        .collect::<BTreeMap<_, _>>();
    let active_sessions = input
        .sessions
        .iter()
        .filter(|session| session_is_active(session.state))
        .map(|session| session.id)
        .collect::<BTreeSet<_>>();
    // The project each session belongs to. A project covers its root and every
    // agent record whose session is loaded, so an agent is never listed twice.
    let mut project_roots = BTreeMap::<AgentSessionId, AgentSessionId>::new();
    for project in input.projects {
        if !sessions_by_id.contains_key(&project.root_session_id) {
            continue;
        }
        project_roots.insert(project.root_session_id, project.root_session_id);
        for agent in &project.agents {
            if sessions_by_id.contains_key(&agent.session_id) {
                project_roots.insert(agent.session_id, project.root_session_id);
            }
        }
    }

    let mut sections = Vec::new();
    for (node_id, group) in group_sessions_by_worker(
        input.sessions,
        input.owners,
        input.node_order,
        input.default_node_id,
    ) {
        let mut rows = Vec::new();
        for session in &group {
            let root = project_roots.get(&session.id).copied();
            // An agent is summarised on its project's row, not listed.
            let (session_id, agents) = match root {
                Some(root) if root != session.id => continue,
                Some(root) => {
                    let agents = input
                        .projects
                        .iter()
                        .find(|project| project.root_session_id == root)
                        .map(|project| {
                            project
                                .agents
                                .iter()
                                .filter(|agent| agent.session_id != root)
                                .filter_map(|agent| {
                                    let session = sessions_by_id.get(&agent.session_id)?;
                                    Some(ProjectAgentRow {
                                        session_id: agent.session_id,
                                        name: session.name.clone(),
                                        // The snapshot's per-agent state is
                                        // authoritative for its agents.
                                        state: agent.state,
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    (root, agents)
                }
                None => (session.id, Vec::new()),
            };
            let Some(source) = sessions_by_id.get(&session_id) else {
                continue;
            };
            if !input.filter.is_empty() && !source.name.to_lowercase().contains(input.filter) {
                continue;
            }
            rows.push(project_list_row(
                source,
                agents,
                &active_sessions,
                input.source_labels,
                input.now_millis,
            ));
        }
        rows.sort_by_key(|row| {
            (
                std::cmp::Reverse(
                    sessions_by_id
                        .get(&row.session_id)
                        .map(|session| session.created_at.as_unix_millis())
                        .unwrap_or(0),
                ),
                positions
                    .get(&row.session_id)
                    .copied()
                    .unwrap_or(usize::MAX),
            )
        });
        sections.push(ProjectListSection { node_id, rows });
    }

    // Highlight the project that owns the active session, so selecting a sub
    // agent keeps its project row highlighted.
    let selected = sessions_by_id
        .contains_key(&input.active_session_id)
        .then(|| {
            project_roots
                .get(&input.active_session_id)
                .copied()
                .unwrap_or(input.active_session_id)
        })
        .filter(|selected| {
            sections
                .iter()
                .any(|section| section.rows.iter().any(|row| row.session_id == *selected))
        });
    ProjectListModel { sections, selected }
}

/// Build one project row from its root session and the agents it holds.
fn project_list_row(
    session: &AgentSessionSnapshot,
    agents: Vec<ProjectAgentRow>,
    active_sessions: &BTreeSet<AgentSessionId>,
    source_labels: &BTreeMap<AgentSessionId, String>,
    now_millis: u64,
) -> ProjectListRow {
    // A project shows activity for its delegated agents too, so a sub agent
    // that is working keeps its project's row alive. Session state is
    // authoritative; persisted task records can lag behind and would keep a
    // finished project animating.
    let active = active_sessions.contains(&session.id)
        || agents
            .iter()
            .any(|agent| active_sessions.contains(&agent.session_id));
    ProjectListRow {
        session_id: session.id,
        name: session.name.clone(),
        detail: source_labels.get(&session.id).cloned().unwrap_or_else(|| {
            format!(
                "updated {}",
                relative_time(session.updated_at.as_unix_millis(), now_millis)
            )
        }),
        summary: (!agents.is_empty()).then(|| agent_summary_line(&agents)),
        agents,
        active,
        status: session_status_pill(session.state, None),
        attention: project_attention_pill(session.state),
    }
}

fn agent_group_label(count: usize) -> String {
    if count == 1 {
        "1 agent".to_owned()
    } else {
        format!("{count} agents")
    }
}

/// A one-line summary of a project's delegated agents: the total, then how many
/// are active and how many are complete.
fn agent_summary_line(agents: &[ProjectAgentRow]) -> String {
    let running = agents
        .iter()
        .filter(|agent| session_is_active(agent.state))
        .count();
    let done = agents
        .iter()
        .filter(|agent| agent.state == AgentSessionState::Completed)
        .count();
    let mut parts = vec![agent_group_label(agents.len())];
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if done > 0 {
        parts.push(format!("{done} done"));
    }
    parts.join(" · ")
}

/// A compact status label with theme-resolved foreground and background colors
/// for a project row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionStatusPill {
    label: &'static str,
    foreground: u32,
    background: u32,
}

impl SessionStatusPill {
    const fn new(label: &'static str, foreground: u32, background: u32) -> Self {
        Self {
            label,
            foreground,
            background,
        }
    }
}

/// The status pill shown beside a project's name: only the states that ask
/// something of the user or report a problem earn one, so a healthy project
/// row stays quiet.
fn project_attention_pill(state: AgentSessionState) -> Option<SessionStatusPill> {
    match state {
        AgentSessionState::AwaitingApproval
        | AgentSessionState::NeedsInput
        | AgentSessionState::Failed
        | AgentSessionState::Paused => Some(session_status_pill(state, None)),
        _ => None,
    }
}

/// Map a session state, or a delegated task's lifecycle state when it has one,
/// to a row pill. The task status wins because it is the more specific record.
fn session_status_pill(
    state: AgentSessionState,
    task: Option<loom_core::DelegatedTaskStatus>,
) -> SessionStatusPill {
    use loom_core::DelegatedTaskStatus as Task;
    if let Some(task) = task {
        return match task {
            Task::Queued => SessionStatusPill::new("Queued", 0xb7c0d0, 0x20242c),
            Task::Running => SessionStatusPill::new("Running", 0x93c5fd, 0x263b58),
            Task::Blocked => SessionStatusPill::new("Blocked", 0xfcd34d, 0x493b1a),
            Task::Completed => SessionStatusPill::new("Done", 0x86efac, 0x24543d),
            Task::Failed => SessionStatusPill::new("Failed", 0xfca5a5, 0x542936),
            Task::Cancelled => SessionStatusPill::new("Cancelled", 0xb7c0d0, 0x20242c),
        };
    }
    match state {
        AgentSessionState::Idle | AgentSessionState::Archived => {
            SessionStatusPill::new("Ready", 0xb7c0d0, 0x20242c)
        }
        AgentSessionState::Queued => SessionStatusPill::new("Queued", 0xb7c0d0, 0x20242c),
        AgentSessionState::Planning => SessionStatusPill::new("Planning", 0x93c5fd, 0x263b58),
        AgentSessionState::Executing => SessionStatusPill::new("Working", 0x93c5fd, 0x263b58),
        AgentSessionState::Evaluating => SessionStatusPill::new("Reviewing", 0x93c5fd, 0x263b58),
        AgentSessionState::AwaitingApproval => {
            SessionStatusPill::new("Approval", 0xfcd34d, 0x493b1a)
        }
        AgentSessionState::NeedsInput => SessionStatusPill::new("Input", 0xfcd34d, 0x493b1a),
        AgentSessionState::Paused => SessionStatusPill::new("Paused", 0xb7c0d0, 0x20242c),
        AgentSessionState::Completed => SessionStatusPill::new("Done", 0x86efac, 0x24543d),
        AgentSessionState::Failed => SessionStatusPill::new("Failed", 0xfca5a5, 0x542936),
        AgentSessionState::Cancelled => SessionStatusPill::new("Cancelled", 0xb7c0d0, 0x20242c),
    }
}

/// A typing-dots task activity light for a project sidebar row: three dots that
/// bounce in sequence. It is only rendered while work is in flight.
fn task_activity_indicator(index: usize, status: SessionStatusPill) -> gpui_kit::AnyElement {
    let color = rgb(status.foreground);
    let dot = |dot_index: usize, offset: f32| {
        div()
            .relative()
            .w(px(4.))
            .h(px(4.))
            .rounded_full()
            .bg(color)
            .with_animation(
                ("sidebar-task-activity", index * 3 + dot_index),
                Animation::new(Duration::from_millis(1_200))
                    .repeat_synced()
                    .with_max_fps(24.),
                move |element, progress| {
                    let phase = (progress - offset).rem_euclid(1.0);
                    let bump = if phase < 0.6 {
                        (std::f32::consts::PI * phase / 0.6).sin()
                    } else {
                        0.0
                    };
                    element.opacity(0.3 + 0.7 * bump).top(px(-2.5 * bump))
                },
            )
    };
    div()
        .id(("project-activity", index))
        .test_support()
        .h(px(12.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(2.))
        .child(dot(0, 0.0))
        .child(dot(1, 0.15))
        .child(dot(2, 0.3))
        .into_any_element()
}

/// Copy for the browser's disconnected screen, which differs depending on
/// whether the client never connected or lost a connection it once had.
///
/// Compiled on native test builds so the copy is covered without a browser.
#[cfg(any(target_family = "wasm", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DisconnectedScreen {
    pub(crate) heading: &'static str,
    pub(crate) detail: String,
    /// The raw transport close reason, kept behind the screen's details
    /// affordance. The headline copy never interpolates it.
    pub(crate) details: Option<String>,
    /// The attempt indicator shown while an automatic reconnect is running.
    pub(crate) progress: Option<String>,
    pub(crate) sidebar: &'static str,
    pub(crate) empty: &'static str,
    pub(crate) footer: &'static str,
    pub(crate) reconnect: bool,
}

#[cfg(any(target_family = "wasm", test))]
impl DisconnectedScreen {
    /// Fills in the attempt indicator from the view's reconnecting state.
    pub(crate) fn with_progress(mut self, progress: Option<String>) -> Self {
        self.progress = progress;
        self
    }
}

#[cfg(any(target_family = "wasm", test))]
pub(crate) fn disconnected_screen(connection_lost: Option<&str>) -> DisconnectedScreen {
    match connection_lost {
        // The reason is a transport string, so it is not part of the copy: the
        // user reads what happened and what the client is doing about it, and
        // the raw reason stays behind the details affordance.
        Some(reason) => DisconnectedScreen {
            heading: "Connection lost",
            detail: format!("{CONNECTION_LOST_COPY} Your projects and sessions are unchanged."),
            details: Some(reason.to_owned()),
            progress: None,
            sidebar: "Reconnecting to the worker.",
            empty: "Reconnecting to reload your projects and sessions.",
            footer: "Disconnected  ·  reconnecting automatically",
            reconnect: true,
        },
        None => DisconnectedScreen {
            heading: "No worker connected",
            detail: "Connect a worker in Settings to begin.".to_owned(),
            details: None,
            progress: None,
            sidebar: "Connect a worker to load projects.",
            empty: "Connect a worker to create a project.",
            footer: "Not connected  ·  Connect a worker in Settings",
            reconnect: false,
        },
    }
}

/// Copy for the browser's startup screen, shown from the first connect until
/// the landing project is on screen.
///
/// A bootstrap in flight is progress, not a failure, so this screen has no
/// settings prompt, no reconnect action, and no details: those stay for a
/// client that never connected or lost its worker.
#[cfg(any(target_family = "wasm", test))]
pub(crate) fn startup_screen(connected: bool) -> DisconnectedScreen {
    DisconnectedScreen {
        heading: if connected {
            "Opening your most recent project"
        } else {
            "Connecting to your worker"
        },
        detail: if connected {
            "Loading the conversation and its project.".to_owned()
        } else {
            "Loading your projects and the most recent project to open.".to_owned()
        },
        details: None,
        progress: None,
        sidebar: "Loading projects…",
        empty: if connected {
            "Opening the most recent project…"
        } else {
            "Connecting to the worker…"
        },
        footer: if connected {
            "Connected  ·  opening your most recent project"
        } else {
            "Connecting to the worker"
        },
        reconnect: false,
    }
}

/// What the browser client should do once the page becomes visible again.
///
/// Kept as a pure decision so the table below can be tested on native builds,
/// where no browser window exists to report a visibility change.
#[cfg(any(target_family = "wasm", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrowserResumption {
    /// Do nothing. Either the page is a demo that never talks to a worker, or
    /// it never connected and has no saved worker settings to reconnect with,
    /// so becoming visible changes nothing. This arm never opens the settings
    /// dialog, because a page becoming visible is not a user request.
    Ignore,
    /// Bootstrap the worker connection again. The view recorded a lost
    /// connection, or it never connected but saved worker settings are
    /// available to connect with.
    Reconnect,
    /// Confirm the connection the view still believes is open. A hidden page
    /// can leave a socket that never reports its close, so the socket has to
    /// answer a probe before the view trusts it again.
    Verify,
}

/// Chooses a [`BrowserResumption`] for a page that just became visible.
///
/// A demo is ignored because it has no worker to reach. A connection the view
/// still believes is open is only verified, never re-bootstrapped, so
/// returning to a tab cannot tear down a working connection. Otherwise the
/// view reconnects when it recorded a loss or has saved worker settings to
/// reconnect with, and leaves a page without a worker alone.
#[cfg(any(target_family = "wasm", test))]
pub(crate) fn browser_resumption(
    demo: bool,
    connected: bool,
    connection_lost: bool,
    configured: bool,
) -> BrowserResumption {
    if demo {
        BrowserResumption::Ignore
    } else if connected {
        BrowserResumption::Verify
    } else if connection_lost || configured {
        BrowserResumption::Reconnect
    } else {
        BrowserResumption::Ignore
    }
}

/// A small uppercase section heading inside a settings pane.
fn settings_section_heading(label: &str) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(rgb(0x93c5fd))
        .child(label.to_owned())
}

/// The rounded surface that groups related settings rows.
fn settings_card() -> gpui_kit::Div {
    div()
        .w_full()
        .rounded_lg()
        .bg(rgb(0x171c25))
        .border_1()
        .border_color(rgb(0x293244))
        .overflow_hidden()
}

/// One settings row: title and description on the left, a single control on the
/// right. On phone layouts the control moves below the text and spans the full
/// width so wide controls never overflow the card. `first` suppresses the
/// divider on the first row of a card.
fn settings_row(
    label: &str,
    description: &str,
    control: impl IntoElement,
    first: bool,
    phone: bool,
) -> impl IntoElement {
    let control = if phone {
        div().w_full().child(control)
    } else {
        div().flex_shrink_0().child(control)
    };
    div()
        .w_full()
        .when(phone, |element| {
            element.flex().flex_col().items_start().gap_2()
        })
        .when(!phone, |element| element.flex().items_center().gap_4())
        .px_4()
        .py_3()
        .when(!first, |element| {
            element.border_t_1().border_color(rgb(0x242833))
        })
        .child(
            div()
                .min_w(px(0.))
                .when(!phone, |element| element.flex_1())
                .child(div().text_sm().child(label.to_owned()))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(description.to_owned()),
                ),
        )
        .child(control)
}

/// The approval policy a session's settings row sends.
///
/// One request replaces every kind decision, so the destructive dimension is
/// always set explicitly: toggling either control must not silently reset the
/// other one's choice.
fn session_approval_policy(
    auto_approve_actions: bool,
    ask_before_destructive_actions: bool,
) -> loom_core::ApprovalPolicy {
    let mut policy = if auto_approve_actions {
        loom_core::ApprovalPolicy::auto_approve()
    } else {
        loom_core::ApprovalPolicy::default()
    };
    policy.destructive = if ask_before_destructive_actions {
        loom_core::PolicyDecision::RequireApproval
    } else {
        loom_core::PolicyDecision::Allow
    };
    policy
}

/// Whether a stored destructive decision prompts the user.
///
/// The setting only distinguishes prompting from running automatically, so a
/// decision that denies destructive actions reads as prompting: the user still
/// has to change the policy before one can run.
fn destructive_actions_require_approval(decision: loom_core::PolicyDecision) -> bool {
    decision != loom_core::PolicyDecision::Allow
}

/// A read-only value shown on the right of an About row, with a stable
/// test-support id so render tests can find it.
fn about_value(id: &'static str, text: String, color: Rgba) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .text_sm()
        .text_color(color)
        .child(text)
}

/// A `- value +` control used by numeric settings rows.
fn settings_stepper(
    decrease: impl IntoElement,
    value: String,
    increase: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(decrease)
        .child(div().w(px(52.)).text_center().text_sm().child(value))
        .child(increase)
}

/// The pill container shared by the settings choice rows, so the theme and
/// agent output rows read as one kind of control.
fn settings_choice_container(children: Vec<gpui_kit::AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1()
        .p_1()
        .rounded_md()
        .bg(rgb(0x20242c))
        .children(children)
}

/// One choice chip of a settings row. The id carries test support so render
/// tests can click a specific choice.
fn settings_choice_chip(
    id: (&'static str, usize),
    label: &str,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .px_3()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .bg(if selected {
            rgb(0x263b58)
        } else {
            rgb(0x20242c)
        })
        .text_xs()
        .text_color(if selected {
            rgb(0xe5e7eb)
        } else {
            rgb(0xb7c0d0)
        })
        .child(label.to_owned())
        .on_click(on_click)
}

/// The settings dialog's section navigation. On phone layouts the sections
/// become a wrapping tab strip above the content; on larger screens they stay
/// a vertical sidebar.
fn settings_nav(
    section: SettingsSection,
    phone: bool,
    cx: &mut Context<LoomView>,
) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .when(phone, |element| {
            element
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .p_3()
                .border_b_1()
        })
        .when(!phone, |element| {
            element
                .w(px(180.))
                .h_full()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .border_r_1()
        })
        .border_color(rgb(0x242833))
        .children(
            SETTINGS_SECTIONS
                .into_iter()
                .enumerate()
                .map(|(index, (candidate, label))| {
                    let selected = candidate == section;
                    div()
                        .id(("settings-section", index))
                        .test_support()
                        .flex_shrink_0()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(if selected {
                            rgb(0x293244)
                        } else {
                            rgb(0x111318)
                        })
                        .text_color(if selected {
                            rgb(0xe5e7eb)
                        } else {
                            rgb(0xb7c0d0)
                        })
                        .hover(|style| style.bg(rgb(0x20242c)))
                        .child(label)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.select_settings_section(candidate, cx);
                        }))
                }),
        )
}

fn project_snapshot_has_unloaded_agent_sessions(
    project: &loom_core::ProjectSnapshot,
    sessions: &[AgentSessionSnapshot],
) -> bool {
    project.agents.iter().any(|agent| {
        !sessions
            .iter()
            .any(|session| session.id == agent.session_id)
    })
}

/// Session ids not covered by any known project snapshot, in list order.
///
/// A project snapshot covers its root and every delegated agent, so fetching
/// the project for the first uncovered session also covers its siblings.
fn uncovered_session_ids(
    sessions: &[AgentSessionSnapshot],
    projects: &[loom_core::ProjectSnapshot],
) -> Vec<AgentSessionId> {
    let covered = projects
        .iter()
        .flat_map(|project| project.agents.iter().map(|agent| agent.session_id))
        .collect::<BTreeSet<_>>();
    sessions
        .iter()
        .filter(|session| !covered.contains(&session.id))
        .map(|session| session.id)
        .collect()
}

/// The session the client opens on startup: the project that was worked on
/// most recently.
///
/// `sessions` is ordered newest activity first, as the worker lists them, and
/// `projects` holds the snapshots that cover them. A delegated sub-agent never
/// becomes the landing session: its update is its project's activity, so the
/// project's root session opens instead. A session no snapshot covers (a
/// standalone session, or one whose snapshot could not be read) lands as
/// itself.
#[cfg(any(target_family = "wasm", test))]
pub(crate) fn landing_session(
    sessions: &[AgentSessionSnapshot],
    projects: &[loom_core::ProjectSnapshot],
) -> Option<AgentSessionSnapshot> {
    let most_recent = sessions.first()?;
    let root_session_id = projects
        .iter()
        .find(|project| {
            project
                .agents
                .iter()
                .any(|agent| agent.session_id == most_recent.id)
        })
        .map_or(most_recent.id, |project| project.root_session_id);
    Some(
        sessions
            .iter()
            .find(|session| session.id == root_session_id)
            .unwrap_or(most_recent)
            .clone(),
    )
}

fn workspace_feed_event_sequence(event: &WorkspaceFeedEvent) -> EventSequence {
    match event {
        WorkspaceFeedEvent::Session(event) => event.sequence,
        WorkspaceFeedEvent::Workspace(event) => event.sequence,
    }
}

fn is_project_workspace_event(
    event: &WorkspaceFeedEvent,
    project_id: loom_core::ProjectId,
    member_ids: &BTreeSet<AgentSessionId>,
) -> bool {
    let WorkspaceFeedEvent::Session(event) = event else {
        return false;
    };
    match &event.event {
        ServerEvent::ProjectTaskUpdated { task } => task.project_id == project_id,
        ServerEvent::ProjectChildWorktreeUpdated { worktree } => worktree.project_id == project_id,
        ServerEvent::ProjectAgentMessageAccepted { message } => message.project_id == project_id,
        ServerEvent::ProjectAgentCreated { agent } | ServerEvent::ProjectAgentUpdated { agent } => {
            agent.project_id == project_id
        }
        ServerEvent::AgentSessionCreated { .. }
        | ServerEvent::AgentSessionStateChanged { .. }
        | ServerEvent::AgentSessionRenamed { .. }
        | ServerEvent::AgentSessionArchived { .. } => member_ids.contains(&event.session_id),
        _ => false,
    }
}

fn project_child_control_actions(
    state: AgentSessionState,
    status: loom_core::DelegatedTaskStatus,
) -> Vec<ProjectChildControlAction> {
    use loom_core::DelegatedTaskStatus as TaskStatus;
    match status {
        TaskStatus::Queued => vec![
            ProjectChildControlAction::Continue,
            ProjectChildControlAction::Cancel,
        ],
        TaskStatus::Running => match state {
            AgentSessionState::Planning
            | AgentSessionState::Executing
            | AgentSessionState::AwaitingApproval
            | AgentSessionState::Evaluating => vec![
                ProjectChildControlAction::Pause,
                ProjectChildControlAction::Interrupt,
                ProjectChildControlAction::Cancel,
            ],
            AgentSessionState::Paused => vec![
                ProjectChildControlAction::Continue,
                ProjectChildControlAction::Cancel,
            ],
            AgentSessionState::Failed => vec![
                ProjectChildControlAction::RetryFailedStep,
                ProjectChildControlAction::Cancel,
            ],
            _ => vec![ProjectChildControlAction::Cancel],
        },
        TaskStatus::Blocked => {
            if state == AgentSessionState::Paused {
                vec![
                    ProjectChildControlAction::Continue,
                    ProjectChildControlAction::Cancel,
                ]
            } else {
                vec![ProjectChildControlAction::Cancel]
            }
        }
        TaskStatus::Failed => vec![
            ProjectChildControlAction::RetryFailedStep,
            ProjectChildControlAction::Cancel,
        ],
        TaskStatus::Completed => vec![ProjectChildControlAction::Abandon],
        TaskStatus::Cancelled => Vec::new(),
    }
}

fn project_child_control_label(action: ProjectChildControlAction) -> &'static str {
    match action {
        ProjectChildControlAction::Continue => "Resume child",
        ProjectChildControlAction::RetryFailedStep => "Retry failed step",
        ProjectChildControlAction::Pause => "Pause child",
        ProjectChildControlAction::Interrupt => "Interrupt child",
        ProjectChildControlAction::Cancel => "Cancel child and descendants",
        ProjectChildControlAction::Abandon => "Abandon completed result",
    }
}

fn source_dialog_initial_state(
    purpose: SessionSourceDialogPurpose,
    local_directory_available: bool,
) -> SessionSourceChoice {
    match purpose {
        SessionSourceDialogPurpose::StartSession => SessionSourceChoice::Empty,
        SessionSourceDialogPurpose::AddToSession if local_directory_available => {
            SessionSourceChoice::LocalDirectory
        }
        SessionSourceDialogPurpose::AddToSession => SessionSourceChoice::GitHub,
    }
}

fn source_choice_is_allowed(
    purpose: SessionSourceDialogPurpose,
    local_directory_available: bool,
    choice: SessionSourceChoice,
) -> bool {
    match choice {
        SessionSourceChoice::Empty => purpose == SessionSourceDialogPurpose::StartSession,
        SessionSourceChoice::LocalDirectory => local_directory_available,
        SessionSourceChoice::GitHub => true,
    }
}

/// Local folders can only be attached by the worker that runs on this machine,
/// so the choice is offered only when the dialog targets the local node.
fn local_source_available(configured: bool, target_node_id: &str, default_node_id: &str) -> bool {
    configured && target_node_id == default_node_id
}

pub(crate) struct GitHubSource {
    pub full_name: String,
    pub clone_url: String,
    /// Reuse the worker node's cached clone instead of cloning again.
    pub reuse_local: bool,
}

pub(crate) enum SessionCreationSource {
    LocalDirectory(String),
    GitHub(GitHubSource),
}

fn session_name_for_source(source: &SessionCreationSource) -> String {
    let name = match source {
        SessionCreationSource::LocalDirectory(path) => Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        SessionCreationSource::GitHub(source) => {
            source.full_name.rsplit('/').next().map(str::to_owned)
        }
    };
    name.filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "New session".to_owned())
}

fn session_name_for_path(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.trim().is_empty())
}

/// Resolves the directory to attach for the local-folder source. An explicit
/// entry wins; otherwise the native worker's current directory is used. The
/// result must be an absolute path.
fn resolve_local_source_path(entered: &str, current: Option<&Path>) -> Option<String> {
    let entered = entered.trim();
    let candidate = if entered.is_empty() {
        current.map(|path| path.display().to_string())
    } else {
        Some(entered.to_owned())
    };
    candidate.filter(|path| PathBuf::from(path).is_absolute())
}

struct SessionSourceDialog {
    purpose: SessionSourceDialogPurpose,
    /// The worker node the dialog creates the project on, chosen with the
    /// "Run on" picker. Resolved when the dialog opens and updated by
    /// `choose_source_node`.
    target_node_id: String,
    choice: SessionSourceChoice,
    local_directory_available: bool,
    filter_subscription: Option<Subscription>,
    repositories: Vec<GitHubRepository>,
    selected_repository: Option<String>,
    repositories_loading: bool,
    error: Option<String>,
}

/// What the virtual transcript list keys its measurements on: the row count and
/// the last row's content, plus the presentation settings that change a row's
/// height without changing its text.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TimelineRevision {
    item_count: usize,
    last_item_len: usize,
    output_detail: OutputDetail,
    show_reasoning: bool,
}

pub(crate) struct TimelineView {
    parent: Entity<LoomView>,
    scroller: Entity<MessageScrollerState>,
    parent_subscription: Option<Subscription>,
    scroller_subscription: Option<Subscription>,
    session_id: Option<AgentSessionId>,
    timeline_revision: TimelineRevision,
}

impl LoomView {
    #[cfg(test)]
    fn new_for_test(focus_handle: FocusHandle) -> Self {
        let connection = ClientConnection::InProcess(Box::new(
            crate::local_backend::OwnedBackend::new().connect(),
        ));
        let backend = BackendWorker::spawn(connection.clone());
        let workspace_id = WorkspaceId::new();
        let active_session = empty_session_snapshot(workspace_id);
        let model = ModelId::new("deterministic/demo");
        let node_id = "test-node".to_owned();
        let node_backends = BTreeMap::from([(node_id.clone(), backend.clone())]);
        Self {
            backend,
            connection,
            #[cfg(not(target_family = "wasm"))]
            owned_backend: None,
            server_protocol_version: None,
            backend_endpoint: None,
            #[cfg(not(target_family = "wasm"))]
            remote_options: RemoteConnectionOptions::default(),
            browser_client: false,
            default_backend_node_id: node_id.clone(),
            node_backends,
            node_names: BTreeMap::new(),
            session_node_ids: BTreeMap::new(),
            workspace_id,
            workspaces: Vec::new(),
            node_workspaces: BTreeMap::new(),
            local_directory_sources_available: true,
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
            node_model_catalogs: BTreeMap::from([(node_id.clone(), vec![model])]),
            node_model_provider_names: BTreeMap::new(),
            model_catalog_node_id: Some(node_id),
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
            theme_choice: ThemeChoice::System,
            font_scale_percent: DEFAULT_FONT_SCALE_PERCENT,
            show_reasoning: false,
            output_detail: OutputDetail::default(),
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
            collaboration: crate::collaboration::CollaborationState::default(),
            unsigned_nodes: BTreeSet::new(),
            awaiting_sign_in_nodes: BTreeSet::new(),
            github_sign_in_available: false,
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
            node_input_initial: String::new(),
            node_input_state: None,
            run_poll_scheduled: false,
            browser_startup_error: None,
            browser_starting: false,
            browser_connection_lost: None,
        }
    }

    fn is_connected(&self) -> bool {
        // A recorded lost browser connection always means "not connected", on
        // any platform, and reading the field keeps it live on native builds
        // where the reconnect screen is compiled out.
        if self.browser_connection_lost.is_some() {
            return false;
        }
        #[cfg(target_family = "wasm")]
        {
            self.connected
        }
        #[cfg(not(target_family = "wasm"))]
        {
            true
        }
    }
}

struct LoomTooltip {
    text: String,
}

impl Render for LoomTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("loom-header-tooltip")
            .test_support()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(0x20242c))
            .border_1()
            .border_color(rgb(0x3b4555))
            .text_sm()
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(self.text.split('\n').map(str::to_owned)),
            )
    }
}

impl Focusable for LoomView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.composer_focus_handle.clone()
    }
}

#[cfg(not(target_family = "wasm"))]
impl Drop for LoomView {
    fn drop(&mut self) {
        self.shutdown_owned_backend();
    }
}

#[cfg(not(target_family = "wasm"))]
impl LoomView {
    /// The managed local worker this view started, if any.
    pub(crate) fn managed_worker(&self) -> Option<std::sync::Arc<loom_local::ManagedWorker>> {
        self.owned_backend
            .as_ref()
            .and_then(crate::local_backend::OwnedBackend::managed_worker)
    }

    fn shutdown_owned_backend(&self) {
        if let Some(backend) = &self.owned_backend
            && let Err(error) = backend.shutdown()
        {
            log::warn!("could not drain local backend during UI teardown: {error}");
        }
    }
}

mod archive;
mod collaboration;
mod composer;
mod connectivity;
mod helpers;
mod lifecycle;
mod project;
mod providers;
mod render;
mod review;
mod runs;
mod sessions;
mod source;
mod surfaces;
mod timeline;
mod workers;

#[cfg(test)]
mod tests;
