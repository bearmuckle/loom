//! The GPUI view: session navigator, run canvas, composer, and review drawer.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gpui_kit::TestSupportExt as _;
use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::base::{Disableable, SelectableText, TextSelectionLayer};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::Dialog;
use gpui_kit::component::input::{
    Input as KitInput, InputEvent, InputState, Textarea, TextareaState,
};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    Icon, IconName, IndexPath, Sizable, h_resizable,
    menu::{DropdownMenu, PopupMenu, PopupMenuItem},
    resizable_panel,
    select::{SearchableVec, Select, SelectEvent, SelectState},
    switch::Switch,
    text::TextView,
    tree::{Tree as KitTree, TreeItem, TreeState},
};
use gpui_kit::{
    Animation, AnimationExt, App, ClickEvent, ClipboardItem, Context, Element, Entity, FocusHandle,
    Focusable, FontWeight, HighlightStyle, MouseButton, Pixels, Render, StyledText, Subscription,
    Window, WindowAppearance, WindowControlArea, div, list, prelude::*, px,
};
use loom_core::{
    ActivityId, AgentMessageRecord, AgentSessionId, AgentSessionSnapshot, AgentSessionState,
    CapabilitySet, ErrorCode, EventSequence, LoomError, RepositoryId, RunId, Timestamp, ToolCallId,
    WorkspaceId, WorkspaceRecord,
};
#[cfg(not(target_family = "wasm"))]
use loom_local::GitHubDeviceCode;
#[cfg(not(target_family = "wasm"))]
use loom_model::GITHUB_COPILOT_DEFAULT_MODEL;
use loom_model::{MessageRole, ModelId, ModelMessage, ProviderKind, ProviderSummary, ToolCall};
#[cfg(target_family = "wasm")]
use loom_protocol::GitHubCopilotLoginStatus;
use loom_protocol::{
    AgentActivityData, AgentActivityRecord, AgentActivityStatus, AgentEvent, AgentRunSnapshot,
    AgentRunSnapshotProjection, AgentRunState, ClientRequest, FileActivityOperation,
    GitDiffLineKind, GitFileStatusKind, GitHubRepository, MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE,
    ProjectChildControlAction, RequestEnvelope, ResponseEnvelope, ServerEvent, ServerResponse,
    SessionDirectory, SessionRepository, WorkerNodeConfig, WorkerNodeResources, WorkerNodeStatus,
    WorkspaceConfig, WorkspaceFeedEvent,
};
#[cfg(not(target_family = "wasm"))]
#[cfg(not(target_family = "wasm"))]
#[cfg(not(target_family = "wasm"))]
use std::fs;

use crate::{
    MAX_REVIEW_CHANGES, MAX_REVIEW_DIFF,
    connection::{BackendWorker, ClientConnection, ConnectionCleanupGuard},
    state::{
        AgentMode, AssistantPart, AssistantTurn, EvidenceText, GitHubLoginState, RenameDialogState,
        ReviewPanel, ReviewRow, ReviewState, SystemNote, SystemTone, ThemeChoice, TimelineItem,
        ToolPart, ToolPartStatus, bounded, bounded_to, finish_assistant_turn,
        push_assistant_evidence, push_assistant_reasoning, push_assistant_text,
        session_state_for_run, session_title_from_task, upsert_tool_part,
    },
    syntax::{self, Language},
    theme::{
        ERROR_CARD_ACCENT, ERROR_CARD_FOREGROUND, ERROR_CARD_SURFACE, change_color, mono_font,
        mono_size, rgb,
    },
};

#[cfg(not(target_family = "wasm"))]
use crate::connection::{
    attach_session_repository, create_session_in_workspace, create_workspace,
    list_models_from_backend, list_workspace_sessions, list_workspaces, redact_secret,
    register_workspace, unexpected_response,
};
#[cfg(target_family = "wasm")]
use crate::connection::{
    attach_session_repository_async, create_session_in_workspace_async, create_workspace_async,
    list_models_from_backend, list_workspace_sessions_async, list_workspaces_async,
    register_workspace_async,
};
#[cfg(target_family = "wasm")]
use crate::connection::{redact_secret, unexpected_response};

const COMPACT_LAYOUT_WIDTH: Pixels = px(960.);
const PHONE_LAYOUT_WIDTH: Pixels = px(700.);
const COMPACT_SIDEBAR_WIDTH: Pixels = px(200.);
const FULL_SIDEBAR_WIDTH: Pixels = px(250.);
const PHONE_SIDEBAR_WIDTH: Pixels = px(300.);
const COMPACT_REVIEW_WIDTH: Pixels = px(440.);
const FULL_REVIEW_WIDTH: Pixels = px(600.);
const TIMELINE_CONTENT_MAX_WIDTH: Pixels = px(760.);
// GPUI's text utilities use rems; native display scaling and browser zoom
// are applied when the window converts them to pixels.
const BASE_FONT_SIZE: f32 = 17.;
const CONVERSATION_FONT_SIZE: f32 = 15.;
const DEFAULT_FONT_SCALE_PERCENT: u16 = 100;
const MIN_FONT_SCALE_PERCENT: u16 = 75;
const MAX_FONT_SCALE_PERCENT: u16 = 150;
const FONT_SCALE_STEP_PERCENT: i16 = 5;
/// Consecutive tool calls of the same kind collapse into one summary row once
/// they reach this count.
const TOOL_GROUP_THRESHOLD: usize = 3;

#[derive(Clone, Copy, Debug)]
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
}

/// The active pane in the settings dialog's section navigation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsSection {
    Agents,
    Providers,
    Workers,
    Appearance,
}

const SETTINGS_SECTIONS: [(SettingsSection, &str); 4] = [
    (SettingsSection::Agents, "Agents"),
    (SettingsSection::Providers, "Providers"),
    (SettingsSection::Workers, "Workers"),
    (SettingsSection::Appearance, "Appearance"),
];

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
        shortcut: Some("⌘B"),
    },
    CommandSpec {
        name: "stop",
        title: "Stop run",
        description: "Interrupt the active run",
        shortcut: Some("esc"),
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
        shortcut: Some("⌘,"),
    },
    CommandSpec {
        name: "help",
        title: "Help",
        description: "List the available commands",
        shortcut: None,
    },
];

#[cfg(not(target_family = "wasm"))]
use crate::connection::{
    list_models, list_provider_ids, negotiate, set_workspace_config, start_run, worker_node_status,
    workspace_config,
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

use helpers::*;
#[cfg(not(target_family = "wasm"))]
use loom_local::{PeerCredentialStore, UiOptions, backend_persistence_path, prepare_workspace};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionNodeIndicatorState {
    Offline,
    Online,
    Severe,
}

type TranscriptMessage = (u64, u64, ModelMessage);
type TranscriptPage = (Vec<TranscriptMessage>, Option<u64>, bool);

async fn load_transcript_page(
    backend: BackendWorker,
    run_id: RunId,
    before_ordinal: Option<u64>,
) -> Result<TranscriptPage, LoomError> {
    let response = backend
        .submit(RequestEnvelope::new(
            ClientRequest::GetAgentRunTranscriptPage {
                run_id,
                before_ordinal,
                limit: MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE,
            },
        ))
        .wait()
        .await;
    let (messages, next_before, has_older) = match response.result? {
        ServerResponse::AgentRunTranscriptPage {
            run_id: response_run_id,
            messages,
            next_before,
            has_older,
        } if response_run_id == run_id => (messages, next_before, has_older),
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
    owned_backend: Option<loom_local::OwnedBackend>,
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
    local_directory_sources_available: bool,
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
    project_messages: Vec<AgentMessageRecord>,
    project_message_cursors: BTreeMap<AgentSessionId, u64>,
    project_messages_stale: bool,
    project_messages_loading: bool,
    project_message_generation: u64,
    project_feed_after_sequence: Option<EventSequence>,
    project_feed_epoch: Option<String>,
    project_poll_scheduled: bool,
    session_tree: Option<Entity<TreeState>>,
    session_tree_entries: Vec<SessionTreeNode>,
    pub(crate) active_session: AgentSessionSnapshot,
    pub(crate) active_run: Option<AgentRunSnapshot>,
    pub(crate) active_run_id: Option<RunId>,
    context_inspection: Option<loom_protocol::ContextInspection>,
    pub(crate) model: ModelId,
    pub(crate) default_model: ModelId,
    pub(crate) session_models: BTreeMap<AgentSessionId, ModelId>,
    pub(crate) agent_mode: AgentMode,
    pub(crate) auto_approve_actions: bool,
    session_auto_approve_actions: BTreeMap<AgentSessionId, bool>,
    pub(crate) session_task_cache: BTreeMap<AgentSessionId, String>,
    pub(crate) optimistic_messages: Vec<String>,
    pub(crate) sending_message: bool,
    pub(crate) models: Vec<ModelId>,
    default_models: Vec<ModelId>,
    node_model_catalogs: BTreeMap<String, Vec<ModelId>>,
    node_model_provider_names: BTreeMap<String, BTreeMap<ModelId, String>>,
    model_catalog_node_id: Option<String>,
    model_refreshes_in_flight: BTreeSet<String>,
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
    agent_mode_select: Option<Entity<ModelSelectState>>,
    agent_mode_select_subscription: Option<Subscription>,
    pub(crate) settings_open: bool,
    settings_section: SettingsSection,
    pub(crate) providers_open: bool,
    pub(crate) about_open: bool,
    pub(crate) providers: Vec<ProviderSummary>,
    providers_node_id: Option<String>,
    provider_api_key_inputs: BTreeMap<loom_model::ProviderId, Entity<InputState>>,
    provider_setup_status: BTreeMap<loom_model::ProviderId, String>,
    pub(crate) theme_choice: ThemeChoice,
    font_scale_percent: u16,
    appearance_subscription: Option<Subscription>,
    pub(crate) after_sequence: Option<EventSequence>,
    event_stream_epoch: Option<String>,
    pub(crate) timeline: Vec<TimelineItem>,
    transcript_before_ordinal: Option<u64>,
    transcript_loaded_ordinals: BTreeSet<u64>,
    transcript_messages: BTreeMap<u64, (u64, ModelMessage)>,
    transcript_has_older: bool,
    transcript_loading: bool,
    transcript_generation: u64,
    timeline_view: Option<Entity<TimelineView>>,
    pub(crate) activity_records: BTreeMap<ActivityId, AgentActivityRecord>,
    pub(crate) expanded_tools: BTreeSet<ToolCallId>,
    pub(crate) expanded_tool_groups: BTreeSet<u64>,
    pub(crate) expanded_reasoning: BTreeSet<u64>,
    pub(crate) approval_request_in_flight: bool,
    approval_settings_request_in_flight: bool,
    pub(crate) archive_request_in_flight: bool,
    pub(crate) pending_approval: Option<ToolCall>,
    pub(crate) pending_input: Option<String>,
    composer_input: Option<Entity<TextareaState>>,
    composer_placeholder: Option<String>,
    composer_completion: Option<ComposerCompletion>,
    pending_completion_accept: bool,
    suppress_completion_once: bool,
    command_palette_open: bool,
    command_palette_input: Option<Entity<InputState>>,
    command_palette_selection: usize,
    session_filter_input: Option<Entity<InputState>>,
    input_subscriptions: Vec<Subscription>,
    clear_composer_on_render: bool,
    clear_node_on_render: bool,
    rename_input_state: Option<Entity<InputState>>,
    source_path_input: Option<Entity<InputState>>,
    repository_filter_input: Option<Entity<InputState>>,
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
    source_dialog: Option<SessionSourceDialog>,
    #[cfg(target_family = "wasm")]
    welcome_dialog_dismissed: bool,
    pub(crate) demo_workspace: bool,
    pub(crate) login_enabled: bool,
    pub(crate) github_connected: bool,
    pub(crate) github_login: Option<GitHubLoginState>,
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
    browser_startup_error: Option<String>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionTreeNode {
    session_id: AgentSessionId,
    label: String,
    children: Vec<SessionTreeNode>,
}

#[derive(Debug, Eq, PartialEq)]
struct SessionListProjection {
    entries: Vec<(AgentSessionId, String)>,
    selected_index: Option<usize>,
    tree: Vec<SessionTreeNode>,
}

fn session_list_projection(
    sessions: &[AgentSessionSnapshot],
    active_session_id: AgentSessionId,
) -> SessionListProjection {
    session_list_projection_from_tree(
        sessions
            .iter()
            .map(|session| SessionTreeNode {
                session_id: session.id,
                label: session.name.clone(),
                children: Vec::new(),
            })
            .collect(),
        active_session_id,
    )
}

fn session_list_projection_from_tree(
    tree: Vec<SessionTreeNode>,
    active_session_id: AgentSessionId,
) -> SessionListProjection {
    fn flatten(node: &SessionTreeNode, entries: &mut Vec<(AgentSessionId, String)>) {
        entries.push((node.session_id, node.label.clone()));
        for child in &node.children {
            flatten(child, entries);
        }
    }

    let mut entries = Vec::new();
    for node in &tree {
        flatten(node, &mut entries);
    }
    let selected_index = entries
        .iter()
        .position(|(session_id, _)| *session_id == active_session_id);
    SessionListProjection {
        entries,
        selected_index,
        tree,
    }
}

fn session_tree_item(node: &SessionTreeNode) -> TreeItem {
    let children = node
        .children
        .iter()
        .map(session_tree_item)
        .collect::<Vec<_>>();
    TreeItem::new(node.session_id.to_string(), node.label.clone())
        .children(children)
        .expanded(!node.children.is_empty())
}

fn find_session_tree_item<'a>(items: &'a [TreeItem], session_id: &str) -> Option<&'a TreeItem> {
    for item in items {
        if item.id.as_ref() == session_id {
            return Some(item);
        }
        if let Some(child) = find_session_tree_item(&item.children, session_id) {
            return Some(child);
        }
    }
    None
}

/// The total number of descendant sessions below a project tree node.
fn session_tree_descendant_count(node: &SessionTreeNode) -> usize {
    node.children.len()
        + node
            .children
            .iter()
            .map(session_tree_descendant_count)
            .sum::<usize>()
}

/// Whether a project node or any of its descendants matches the lowercased
/// filter. Projects are kept whole, so a child match keeps its root.
fn session_tree_matches(node: &SessionTreeNode, filter: &str) -> bool {
    node.label.to_lowercase().contains(filter)
        || node
            .children
            .iter()
            .any(|child| session_tree_matches(child, filter))
}

/// Keep every project whose root or any descendant matches the filter.
fn filter_session_tree(tree: Vec<SessionTreeNode>, filter: &str) -> Vec<SessionTreeNode> {
    tree.into_iter()
        .filter(|node| session_tree_matches(node, filter))
        .collect()
}

/// A compact status label with theme-resolved foreground and background colors
/// for a project child row.
struct SessionStatusPill {
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

/// Map a delegated task's lifecycle state to a child row pill. Falls back to
/// the session state for agents that have no durable task record.
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
/// right. `first` suppresses the divider on the first row of a card.
fn settings_row(
    label: &str,
    description: &str,
    control: impl IntoElement,
    first: bool,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_4()
        .px_4()
        .py_3()
        .when(!first, |element| {
            element.border_t_1().border_color(rgb(0x242833))
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(div().text_sm().child(label.to_owned()))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(description.to_owned()),
                ),
        )
        .child(div().flex_shrink_0().child(control))
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

/// The settings dialog's section navigation.
fn settings_nav(section: SettingsSection, cx: &mut Context<LoomView>) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .w(px(180.))
        .h_full()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .border_r_1()
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
                            view.settings_section = candidate;
                            cx.notify();
                        }))
                }),
        )
}

#[cfg(test)]
fn project_session_list_projection(
    sessions: &[AgentSessionSnapshot],
    active_session_id: AgentSessionId,
    project: Option<&loom_core::ProjectSnapshot>,
) -> SessionListProjection {
    project_session_list_projection_for_projects(
        sessions,
        active_session_id,
        project.into_iter().collect(),
    )
}

fn project_session_list_projection_for_projects(
    sessions: &[AgentSessionSnapshot],
    active_session_id: AgentSessionId,
    projects: Vec<&loom_core::ProjectSnapshot>,
) -> SessionListProjection {
    if projects.is_empty() {
        return session_list_projection(sessions, active_session_id);
    }
    let mut tree = Vec::new();
    let mut included = BTreeSet::new();
    for project in projects {
        let projection =
            project_session_list_projection_for_project(sessions, active_session_id, project);
        if let Some(root) = projection
            .tree
            .into_iter()
            .find(|node| node.session_id == project.root_session_id)
            && included.insert(root.session_id)
        {
            fn include_descendants(
                node: &SessionTreeNode,
                included: &mut BTreeSet<AgentSessionId>,
            ) {
                included.insert(node.session_id);
                for child in &node.children {
                    include_descendants(child, included);
                }
            }
            include_descendants(&root, &mut included);
            tree.push(root);
        }
    }
    for session in sessions {
        if included.insert(session.id) {
            tree.push(SessionTreeNode {
                session_id: session.id,
                label: session.name.clone(),
                children: Vec::new(),
            });
        }
    }
    tree.sort_by_key(|node| {
        sessions
            .iter()
            .position(|session| session.id == node.session_id)
            .unwrap_or(usize::MAX)
    });
    session_list_projection_from_tree(tree, active_session_id)
}

fn project_session_list_projection_for_project(
    sessions: &[AgentSessionSnapshot],
    active_session_id: AgentSessionId,
    project: &loom_core::ProjectSnapshot,
) -> SessionListProjection {
    let sessions_by_id = sessions
        .iter()
        .map(|session| (session.id, session))
        .collect::<BTreeMap<_, _>>();
    let agents_by_id = project
        .agents
        .iter()
        .filter(|agent| agent.session_id != project.root_session_id)
        .map(|agent| (agent.session_id, agent))
        .collect::<BTreeMap<_, _>>();
    let mut agents_by_parent =
        BTreeMap::<AgentSessionId, Vec<&loom_core::ProjectAgentRecord>>::new();
    for agent in project
        .agents
        .iter()
        .filter(|agent| agent.session_id != project.root_session_id)
    {
        let parent_id = agent
            .parent_session_id
            .filter(|parent_id| {
                *parent_id == project.root_session_id
                    || (agents_by_id.contains_key(parent_id)
                        && sessions_by_id.contains_key(parent_id))
            })
            .unwrap_or(project.root_session_id);
        agents_by_parent.entry(parent_id).or_default().push(agent);
    }

    fn build_agent_node(
        session_id: AgentSessionId,
        sessions_by_id: &BTreeMap<AgentSessionId, &AgentSessionSnapshot>,
        agents_by_parent: &BTreeMap<AgentSessionId, Vec<&loom_core::ProjectAgentRecord>>,
        visited: &mut BTreeSet<AgentSessionId>,
    ) -> Option<SessionTreeNode> {
        if !visited.insert(session_id) {
            return None;
        }
        let session = sessions_by_id.get(&session_id)?;
        let children = agents_by_parent
            .get(&session_id)
            .into_iter()
            .flatten()
            .filter_map(|agent| {
                build_agent_node(agent.session_id, sessions_by_id, agents_by_parent, visited)
            })
            .collect();
        Some(SessionTreeNode {
            session_id,
            label: session.name.clone(),
            children,
        })
    }

    let Some(root_session) = sessions_by_id.get(&project.root_session_id) else {
        return session_list_projection(sessions, active_session_id);
    };
    let mut visited = BTreeSet::new();
    let mut root_node = SessionTreeNode {
        session_id: project.root_session_id,
        label: root_session.name.clone(),
        children: Vec::new(),
    };
    visited.insert(project.root_session_id);
    root_node.children = agents_by_parent
        .get(&project.root_session_id)
        .into_iter()
        .flatten()
        .filter_map(|agent| {
            build_agent_node(
                agent.session_id,
                &sessions_by_id,
                &agents_by_parent,
                &mut visited,
            )
        })
        .collect();

    // Keep agents with missing parents and parent cycles in the project group too.
    for agent in agents_by_id.values() {
        if let Some(node) = build_agent_node(
            agent.session_id,
            &sessions_by_id,
            &agents_by_parent,
            &mut visited,
        ) {
            root_node.children.push(node);
        }
    }

    let project_session_ids = agents_by_id.keys().copied().collect::<BTreeSet<_>>();
    let mut tree = Vec::new();
    for session in sessions {
        if session.id == project.root_session_id {
            tree.push(root_node.clone());
        } else if !project_session_ids.contains(&session.id) {
            tree.push(SessionTreeNode {
                session_id: session.id,
                label: session.name.clone(),
                children: Vec::new(),
            });
        }
    }
    session_list_projection_from_tree(tree, active_session_id)
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
        TaskStatus::Completed | TaskStatus::Cancelled => Vec::new(),
    }
}

fn project_child_control_label(action: ProjectChildControlAction) -> &'static str {
    match action {
        ProjectChildControlAction::Continue => "Resume child",
        ProjectChildControlAction::RetryFailedStep => "Retry failed step",
        ProjectChildControlAction::Pause => "Pause child",
        ProjectChildControlAction::Interrupt => "Interrupt child",
        ProjectChildControlAction::Cancel => "Cancel child and descendants",
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

fn local_source_available(
    purpose: SessionSourceDialogPurpose,
    configured: bool,
    active_node_id: Option<&str>,
    default_node_id: &str,
) -> bool {
    configured
        && match purpose {
            SessionSourceDialogPurpose::StartSession => true,
            SessionSourceDialogPurpose::AddToSession => {
                active_node_id.is_none_or(|node_id| node_id == default_node_id)
            }
        }
}

pub(crate) enum SessionCreationSource {
    LocalDirectory(String),
    GitHub(GitHubRepository),
}

fn session_name_for_source(source: &SessionCreationSource) -> String {
    let name = match source {
        SessionCreationSource::LocalDirectory(path) => Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        SessionCreationSource::GitHub(repository) => {
            repository.full_name.rsplit('/').next().map(str::to_owned)
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

struct SessionSourceDialog {
    purpose: SessionSourceDialogPurpose,
    choice: SessionSourceChoice,
    local_directory_available: bool,
    filter_subscription: Option<Subscription>,
    repositories: Vec<GitHubRepository>,
    selected_repository: Option<String>,
    repositories_loading: bool,
    error: Option<String>,
}

pub(crate) struct TimelineView {
    parent: Entity<LoomView>,
    scroller: Entity<MessageScrollerState>,
    parent_subscription: Option<Subscription>,
    scroller_subscription: Option<Subscription>,
    session_id: Option<AgentSessionId>,
    timeline_revision: (usize, usize),
}

impl LoomView {
    #[cfg(test)]
    fn new_for_test(focus_handle: FocusHandle) -> Self {
        let connection =
            ClientConnection::InProcess(Box::new(loom_local::OwnedBackend::new().connect()));
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
            default_backend_node_id: node_id.clone(),
            node_backends,
            node_names: BTreeMap::new(),
            session_node_ids: BTreeMap::new(),
            workspace_id,
            workspaces: Vec::new(),
            local_directory_sources_available: true,
            sessions: Vec::new(),
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
            active_session: active_session.clone(),
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
            model: model.clone(),
            models: vec![model.clone()],
            default_models: vec![model.clone()],
            node_model_catalogs: BTreeMap::from([(node_id.clone(), vec![model])]),
            node_model_provider_names: BTreeMap::new(),
            model_catalog_node_id: Some(node_id),
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
            session_state: active_session.state,
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
            login_enabled: false,
            github_connected: false,
            github_login: None,
            next_worker_node_id: 0,
            worker_nodes: Vec::new(),
            worker_node_polls_scheduled: BTreeSet::new(),
            workspace_config: WorkspaceConfig::default(),
            node_input_initial: String::new(),
            node_input_state: None,
            run_poll_scheduled: false,
            browser_startup_error: None,
        }
    }

    fn is_connected(&self) -> bool {
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
    fn shutdown_owned_backend(&self) {
        if let Some(backend) = &self.owned_backend
            && let Err(error) = backend.shutdown()
        {
            log::warn!("could not drain local backend during UI teardown: {error}");
        }
    }
}

mod composer;
mod helpers;
mod lifecycle;
mod project;
mod providers;
mod render;
mod review;
mod runs;
mod sessions;
mod source;
mod timeline;
mod workers;

#[cfg(test)]
mod tests;
