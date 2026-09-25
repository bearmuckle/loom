//! The GPUI view: session navigator, run canvas, composer, and review drawer.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::PathBuf,
    time::Duration,
};

use gpui_kit::base::{Disableable, SelectableText, TextSelectionLayer};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{
    Icon, IconName, IndexPath, Sizable,
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    select::{SearchableVec, Select, SelectEvent, SelectState},
    text::TextView,
};
use gpui_kit::{
    Animation, AnimationExt, App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle,
    Decorations, Element, Entity, EntityInputHandler, FocusHandle, Focusable, HitboxBehavior,
    ListAlignment, ListState, MouseButton, MouseDownEvent, Pixels, Point, Render, ResizeEdge,
    Subscription, Tiling, UTF16Selection, Window, WindowAppearance, WindowControlArea, canvas, div,
    list, point, prelude::*, px, transparent_black,
};
use loom_core::{
    ActivityId, AgentSessionId, AgentSessionSnapshot, AgentSessionState, CapabilitySet, ErrorCode,
    EventSequence, LoomError, ProjectId, RunId,
};
use loom_model::{MessageRole, ModelId, ProviderKind, ProviderSummary, ToolCall};
#[cfg(target_family = "wasm")]
use loom_protocol::GitHubCopilotLoginStatus;
use loom_protocol::{
    AgentActivityData, AgentActivityRecord, AgentActivityStatus, AgentEvent, AgentRunSnapshot,
    AgentRunSnapshotProjection, AgentRunState, ClientRequest, FileActivityOperation,
    ProjectSnapshot, RequestEnvelope, ResponseEnvelope, ServerEvent, ServerResponse, TaskSnapshot,
    TaskStatus, WorkerNodeConfig, WorkerNodeResources, WorkerNodeStatus, WorkspaceConfig,
};
#[cfg(not(target_family = "wasm"))]
use loom_providers::{GITHUB_COPILOT_DEFAULT_MODEL, GitHubCopilotAuthenticator, GitHubDeviceCode};
#[cfg(not(target_family = "wasm"))]
use loom_server::InProcessBackend;

use crate::{
    MAX_REVIEW_CHANGES, MAX_REVIEW_DIFF,
    connection::{
        BackendWorker, ClientConnection, ConnectionCleanupGuard, list_models_from_backend,
        redact_secret, select_remote_project, unexpected_response,
    },
    state::{
        AgentMode, GitHubLoginState, RenameDialogState, ReviewPanel, ReviewState, ThemeChoice,
        TimelineItem, activity_status_label, bounded, bounded_to, session_state_for_run,
        session_title_from_task, upsert_activity,
    },
    text_input::{
        Backspace, Copy, Delete, End, Home, InputField, Left, LoomTooltip, Paste, Right, SelectAll,
        Submit, TextBufferState, TextInputElement,
    },
    theme::{
        CLIENT_DECORATION_SHADOW, ClientCorners, ERROR_CARD_ACCENT, ERROR_CARD_FOREGROUND,
        ERROR_CARD_SURFACE, change_color, resize_edge, rgb, state_color,
    },
};

const COMPACT_LAYOUT_WIDTH: Pixels = px(960.);
const PHONE_LAYOUT_WIDTH: Pixels = px(640.);
const COMPACT_SIDEBAR_WIDTH: Pixels = px(200.);
const FULL_SIDEBAR_WIDTH: Pixels = px(250.);
const PHONE_SIDEBAR_WIDTH: Pixels = px(300.);
const COMPACT_REVIEW_WIDTH: Pixels = px(280.);
const FULL_REVIEW_WIDTH: Pixels = px(340.);

#[derive(Clone, Copy, Debug)]
struct ResponsiveLayout {
    phone: bool,
    sidebar_width: Pixels,
    review_width: Pixels,
}

fn responsive_layout(width: Pixels) -> ResponsiveLayout {
    if width < PHONE_LAYOUT_WIDTH {
        ResponsiveLayout {
            phone: true,
            sidebar_width: if width < PHONE_SIDEBAR_WIDTH {
                width
            } else {
                PHONE_SIDEBAR_WIDTH
            },
            review_width: width,
        }
    } else if width < COMPACT_LAYOUT_WIDTH {
        ResponsiveLayout {
            phone: false,
            sidebar_width: COMPACT_SIDEBAR_WIDTH,
            review_width: COMPACT_REVIEW_WIDTH,
        }
    } else {
        ResponsiveLayout {
            phone: false,
            sidebar_width: FULL_SIDEBAR_WIDTH,
            review_width: FULL_REVIEW_WIDTH,
        }
    }
}

#[cfg(target_family = "wasm")]
use crate::{
    browser::BrowserOptions,
    connection::{
        create_session_async, list_models_async, list_projects_async, list_sessions_async,
        negotiate_async, open_workspace_async, set_workspace_config_async,
        worker_node_status_async, workspace_config_async,
    },
};
#[cfg(not(target_family = "wasm"))]
use crate::{
    connection::{
        create_session, list_models, list_projects, list_provider_ids, list_sessions, negotiate,
        open_workspace, set_workspace_config, start_run, worker_node_status, workspace_config,
    },
    platform::{
        PeerCredentialStore, UiOptions, backend_persistence_path, prepare_workspace,
        stable_project_id,
    },
};
#[cfg(not(target_family = "wasm"))]
use log::info;

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

fn format_bytes(value: Option<u64>) -> String {
    let Some(value) = value else {
        return "n/a".to_owned();
    };
    let (value, suffix) = if value >= 1 << 30 {
        (value as f64 / (1 << 30) as f64, "GiB")
    } else if value >= 1 << 20 {
        (value as f64 / (1 << 20) as f64, "MiB")
    } else {
        (value as f64 / (1 << 10) as f64, "KiB")
    };
    format!("{value:.1} {suffix}")
}

fn format_percentage(value: Option<u8>) -> String {
    value
        .filter(|value| *value <= 100)
        .map_or_else(|| "n/a".to_owned(), |value| format!("{value}%"))
}

fn format_worker_node_resources(resources: &WorkerNodeResources) -> String {
    let cpu_usage = if resources.cpu_count > 0 {
        format!(
            "CPU {} of {} cores",
            format_percentage(resources.cpu_usage_percent),
            resources.cpu_count
        )
    } else {
        "CPU n/a".to_owned()
    };
    format!(
        "{cpu_usage} · RAM {} of {} · disk {} available",
        format_percentage(resources.memory_usage_percent),
        format_bytes(resources.memory_total_bytes),
        format_bytes(resources.disk_available_bytes),
    )
}

fn worker_node_for_id<'a>(
    nodes: &'a [WorkerNodeEntry],
    node_id: Option<&str>,
) -> Option<&'a WorkerNodeEntry> {
    let node_id = node_id?;
    nodes.iter().find(|node| node.status.node_id == node_id)
}

fn worker_node_name_for_id(
    nodes: &[WorkerNodeEntry],
    node_names: &BTreeMap<String, String>,
    node_id: Option<&str>,
) -> String {
    worker_node_for_id(nodes, node_id)
        .map(worker_node_display_name)
        .or_else(|| node_id.and_then(|node_id| node_names.get(node_id).cloned()))
        .unwrap_or_else(|| "Worker node unavailable".to_owned())
}

fn worker_node_display_name(node: &WorkerNodeEntry) -> String {
    let role = if node.is_local {
        "Local backend"
    } else {
        "External worker"
    };
    format!("{role} · {}", node.status.name)
}

fn format_session_resource_percentages(status: Option<&WorkerNodeStatus>) -> String {
    let resources = status.map(|status| &status.resources);
    format!(
        "CPU {} · RAM {}",
        format_percentage(resources.and_then(|resources| resources.cpu_usage_percent)),
        format_percentage(resources.and_then(|resources| resources.memory_usage_percent)),
    )
}

fn session_owner_status<'a>(
    nodes: &'a [WorkerNodeEntry],
    session_node_ids: &BTreeMap<AgentSessionId, String>,
    session_id: AgentSessionId,
) -> Option<&'a WorkerNodeEntry> {
    worker_node_for_id(nodes, session_node_ids.get(&session_id).map(String::as_str))
}

fn session_node_pulse(
    status: Option<&WorkerNodeStatus>,
    threshold_percent: u8,
) -> Option<(Duration, f32)> {
    let cpu_percent = status
        .filter(|status| status.online)
        .and_then(|status| status.resources.cpu_usage_percent)
        .filter(|percent| *percent <= 100)?;
    let threshold_percent = threshold_percent.min(100);
    if cpu_percent <= threshold_percent {
        return None;
    }
    let load_above_threshold = f32::from(cpu_percent - threshold_percent)
        / f32::from(100_u8.saturating_sub(threshold_percent).max(1));
    let period_ms = 2_600_u64 - (load_above_threshold * 800.) as u64;
    let amplitude = 0.35 + load_above_threshold * 0.8;
    Some((Duration::from_millis(period_ms), amplitude))
}

fn next_severe_load_streak(current: u8, resources: &WorkerNodeResources) -> u8 {
    match (resources.cpu_usage_percent, resources.memory_usage_percent) {
        (Some(cpu), Some(memory)) if cpu > 90 && cpu <= 100 && memory > 90 && memory <= 100 => {
            current.saturating_add(1)
        }
        _ => 0,
    }
}

fn adjusted_cpu_pulse_threshold(current: u8, delta: i8) -> u8 {
    (i16::from(current.min(100)) + i16::from(delta)).clamp(0, 100) as u8
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionNodeIndicatorState {
    Offline,
    Online,
    Severe,
}

fn session_node_indicator_state(
    status: Option<&WorkerNodeStatus>,
    severe_load_streak: u8,
) -> SessionNodeIndicatorState {
    match status {
        Some(status) if status.online && severe_load_streak >= 3 => {
            SessionNodeIndicatorState::Severe
        }
        Some(status) if status.online => SessionNodeIndicatorState::Online,
        _ => SessionNodeIndicatorState::Offline,
    }
}

fn order_session_nodes(
    mut nodes: Vec<(String, String)>,
    default_node_id: &str,
) -> Vec<(String, String)> {
    nodes.sort_by_key(|(node_id, _)| node_id != default_node_id);
    nodes
}

fn merge_node_sessions(
    current: &[AgentSessionSnapshot],
    current_owners: &BTreeMap<AgentSessionId, String>,
    node_results: Vec<(String, Vec<AgentSessionSnapshot>)>,
) -> (Vec<AgentSessionSnapshot>, BTreeMap<AgentSessionId, String>) {
    let queried_nodes = node_results
        .iter()
        .map(|(node_id, _)| node_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut sessions = current
        .iter()
        .filter(|session| {
            current_owners
                .get(&session.id)
                .is_none_or(|owner| !queried_nodes.contains(owner.as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut owners = current_owners.clone();
    for (node_id, node_sessions) in node_results {
        for session in node_sessions {
            if let Some(existing) = sessions
                .iter_mut()
                .find(|existing| existing.id == session.id)
            {
                *existing = session.clone();
            } else {
                sessions.push(session.clone());
            }
            owners.insert(session.id, node_id.clone());
        }
    }
    owners.retain(|session_id, _| sessions.iter().any(|session| session.id == *session_id));
    (sessions, owners)
}

fn session_id_for_request(
    request: &ClientRequest,
    active_session_id: AgentSessionId,
) -> Option<AgentSessionId> {
    match request {
        ClientRequest::SetApprovalPolicy {
            session_id: Some(session_id),
            ..
        } => Some(*session_id),
        ClientRequest::GetAgentSession { session_id }
        | ClientRequest::GetAgentSessionSnapshot { session_id }
        | ClientRequest::RenameAgentSession { session_id, .. }
        | ClientRequest::ArchiveAgentSession { session_id }
        | ClientRequest::GetRecentSessionEvents { session_id, .. }
        | ClientRequest::StartAgentRun { session_id, .. }
        | ClientRequest::StartAgentRunWithOptions { session_id, .. }
        | ClientRequest::ForkAgentSession { session_id, .. }
        | ClientRequest::GetSessionUsage { session_id } => Some(*session_id),
        ClientRequest::CreateCheckpoint {
            session_id: Some(session_id),
            ..
        } => Some(*session_id),
        ClientRequest::GetSessionEvents { session_id, .. } => {
            Some(session_id.unwrap_or(active_session_id))
        }
        ClientRequest::GetAgentRun { .. }
        | ClientRequest::GetAgentRunSnapshot { .. }
        | ClientRequest::GetRunCheckpoint { .. }
        | ClientRequest::ApproveAgentAction { .. }
        | ClientRequest::RejectAgentAction { .. }
        | ClientRequest::SendAgentMessage { .. }
        | ClientRequest::InterruptAgentRun { .. }
        | ClientRequest::RetryAgentStep { .. }
        | ClientRequest::PauseAgentRun { .. }
        | ClientRequest::ResumeAgentRun { .. }
        | ClientRequest::RetryAgentFromCheckpoint { .. }
        | ClientRequest::GetRunUsage { .. }
        | ClientRequest::InspectAgentContext { .. }
        | ClientRequest::AttachRunEvidence { .. } => Some(active_session_id),
        _ => None,
    }
}

fn assigned_node_id(
    session_node_ids: &BTreeMap<AgentSessionId, String>,
    session_id: AgentSessionId,
) -> Result<&str, LoomError> {
    session_node_ids
        .get(&session_id)
        .map(String::as_str)
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::NotFound,
                format!("session {session_id} has no assigned worker node"),
                false,
            )
        })
}

fn validate_model_for_node(
    node_model_catalogs: &BTreeMap<String, Vec<ModelId>>,
    node_id: &str,
    model: &ModelId,
) -> Result<(), String> {
    let Some(models) = node_model_catalogs.get(node_id) else {
        return Err("model availability has not been checked".to_owned());
    };
    if models.contains(model) {
        Ok(())
    } else {
        Err(format!("model '{}' is not configured", model.as_str()))
    }
}

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

fn format_duration(elapsed_ms: u64) -> String {
    if elapsed_ms < 1_000 {
        format!("{elapsed_ms}ms")
    } else if elapsed_ms < 60_000 {
        format!("{:.1}s", elapsed_ms as f64 / 1_000.0)
    } else {
        format!(
            "{}m {}s",
            elapsed_ms / 60_000,
            (elapsed_ms % 60_000) / 1_000
        )
    }
}

fn session_state_label(state: AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Idle => "Ready",
        AgentSessionState::Queued => "Queued",
        AgentSessionState::Planning => "Planning",
        AgentSessionState::AwaitingApproval => "Needs approval",
        AgentSessionState::Paused => "Paused",
        AgentSessionState::Executing => "Working",
        AgentSessionState::Evaluating => "Reviewing",
        AgentSessionState::NeedsInput => "Needs your input",
        AgentSessionState::Completed => "Complete",
        AgentSessionState::Failed => "Something went wrong",
        AgentSessionState::Cancelled => "Cancelled",
        AgentSessionState::Archived => "Archived",
    }
}

fn run_state_label(state: Option<AgentRunState>) -> &'static str {
    match state {
        None => "Ready",
        Some(AgentRunState::Planning) => "Planning",
        Some(AgentRunState::Executing) => "Working",
        Some(AgentRunState::AwaitingApproval) => "Needs approval",
        Some(AgentRunState::Paused) => "Paused",
        Some(AgentRunState::NeedsInput) => "Needs your input",
        Some(AgentRunState::Evaluating) => "Reviewing",
        Some(AgentRunState::Completed) => "Complete",
        Some(AgentRunState::Failed) => "Something went wrong",
        Some(AgentRunState::Cancelled) => "Cancelled",
    }
}

fn change_kind_label(kind: loom_workspace::WorkspaceChangeKind) -> &'static str {
    match kind {
        loom_workspace::WorkspaceChangeKind::Created => "New",
        loom_workspace::WorkspaceChangeKind::Deleted => "Removed",
        loom_workspace::WorkspaceChangeKind::Modified => "Updated",
    }
}

fn activity_marker(status: AgentActivityStatus) -> &'static str {
    match status {
        AgentActivityStatus::Started => "›",
        AgentActivityStatus::Completed => "✓",
        AgentActivityStatus::Failed => "×",
        AgentActivityStatus::AwaitingApproval => "!",
        AgentActivityStatus::AwaitingInput => "?",
        AgentActivityStatus::Cancelled => "–",
    }
}

fn render_timeline_text(id: String, text: String, color: u32) -> gpui_kit::AnyElement {
    let has_markdown = text.lines().any(|line| {
        let line = line.trim_start();
        (line.starts_with('#')
            && line
                .strip_prefix('#')
                .is_some_and(|rest| rest.trim_start_matches('#').starts_with(' ')))
            || line.starts_with("- ")
            || line.starts_with("* ")
            || line.starts_with("+ ")
            || line.starts_with("> ")
            || line.starts_with("```")
            || line.starts_with("~~~")
            || line.starts_with("1. ")
    }) || text.contains("**")
        || text.contains("__")
        || text.contains('`')
        || text.contains("~~")
        || text.contains("![")
        || text.contains("](");

    if !has_markdown {
        div()
            .w_full()
            .text_color(rgb(color))
            .child(SelectableText::new(id, text))
            .into_any()
    } else {
        TextView::markdown(id, text)
            .selectable(true)
            .w_full()
            .text_color(rgb(color))
            .into_any()
    }
}

fn activity_label(activity: &AgentActivityRecord) -> (String, Option<String>) {
    let compact_arguments = |call: &loom_model::ToolCall| {
        bounded_to(
            &serde_json::to_string(&call.arguments).unwrap_or_default(),
            180,
        )
    };
    match &activity.data {
        AgentActivityData::ModelTurn { model } => {
            (format!("Agent turn · {}", model.as_str()), None)
        }
        AgentActivityData::ToolCall { call, .. } => {
            (call.name.clone(), Some(compact_arguments(call)))
        }
        AgentActivityData::File {
            call,
            operation,
            path,
            ..
        } => {
            let operation = match operation {
                FileActivityOperation::List => "List files",
                FileActivityOperation::Read => "Read file",
                FileActivityOperation::Write => "Write file",
            };
            (
                operation.to_owned(),
                Some(format!(
                    "{} · {}",
                    path.as_deref().unwrap_or("."),
                    compact_arguments(call)
                )),
            )
        }
        AgentActivityData::Search {
            call, query, path, ..
        } => (
            "Search".to_owned(),
            Some(format!(
                "\"{}\"{} · {}",
                bounded_to(query, 120),
                path.as_deref()
                    .map_or_else(String::new, |path| format!(" in {path}")),
                compact_arguments(call)
            )),
        ),
        AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            ..
        } => {
            let command_line = std::iter::once(command.as_str())
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            (
                "Run command".to_owned(),
                Some(format!(
                    "{}{} · {}",
                    bounded_to(&command_line, 180),
                    cwd.as_deref()
                        .map_or_else(String::new, |cwd| format!(" in {cwd}")),
                    compact_arguments(call)
                )),
            )
        }
    }
}

fn activity_output(activity: &AgentActivityRecord) -> Option<&str> {
    match &activity.data {
        AgentActivityData::ModelTurn { .. } => None,
        AgentActivityData::ToolCall { result, .. }
        | AgentActivityData::File { result, .. }
        | AgentActivityData::Search { result, .. }
        | AgentActivityData::Command { result, .. } => result
            .as_ref()
            .map(|result| result.output.as_str())
            .filter(|output| !output.is_empty()),
    }
}

fn activity_turn_title(activities: &[AgentActivityRecord]) -> &'static str {
    if activities.iter().any(|activity| {
        matches!(
            &activity.data,
            AgentActivityData::File {
                operation: FileActivityOperation::Write,
                ..
            }
        ) || matches!(
            &activity.data,
            AgentActivityData::ToolCall { call, .. } if call.name == "apply_patch"
        )
    }) {
        "Making changes"
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::Command { .. }))
    {
        "Running commands"
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::Search { .. }))
    {
        "Searching the codebase"
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::File { .. }))
    {
        "Inspecting the workspace"
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::ToolCall { .. }))
    {
        "Using tools"
    } else {
        "Working on the task"
    }
}

fn is_redundant_completion_summary(summary: &str) -> bool {
    summary.starts_with("Completed task:")
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
enum WorkerConnectionStage {
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

fn worker_connection_failure_detail(
    stage: WorkerConnectionStage,
    error: &LoomError,
    secret: Option<&str>,
) -> String {
    let lower_message = error.message.to_ascii_lowercase();
    if stage == WorkerConnectionStage::CredentialSave {
        let detail = redact_secret(&error.message, secret.unwrap_or_default());
        return format!(
            "Connected for this session, but could not save reconnect credentials securely: {}. Re-enter the URL and token after restart.",
            detail
                .chars()
                .filter(|character| !character.is_control())
                .take(240)
                .collect::<String>()
        );
    }
    if stage == WorkerConnectionStage::BootstrapSave {
        let detail = redact_secret(&error.message, secret.unwrap_or_default());
        return format!(
            "Connected for this session, but the browser could not save its bootstrap connection: {}. The worker must be opened again manually after restart.",
            detail
                .chars()
                .filter(|character| !character.is_control())
                .take(240)
                .collect::<String>()
        );
    }
    if stage == WorkerConnectionStage::CredentialRead {
        return "Could not read this worker's saved credential from the OS credential store. Re-enter its URL and access token.".to_owned();
    }
    if stage == WorkerConnectionStage::InputValidation {
        return "Enter a worker URL followed by its access token.".to_owned();
    }
    if matches!(
        error.code,
        ErrorCode::AuthenticationFailed
            | ErrorCode::AuthenticationRequired
            | ErrorCode::AuthorizationDenied
    ) {
        return "The worker denied authentication. Check the access token and the worker's authentication configuration.".to_owned();
    }
    if error.code == ErrorCode::RequestCancelled {
        return match stage {
            WorkerConnectionStage::Transport | WorkerConnectionStage::Negotiation => {
                "The worker closed the connection before negotiation completed. Check that it is running, the URL is correct, and the access token is valid."
                    .to_owned()
            }
            _ => "The worker closed the connection before returning status. Check that it is running and reachable, then retry.".to_owned(),
        };
    }
    if stage == WorkerConnectionStage::Transport
        && error.code == ErrorCode::InvalidRequest
        && lower_message.contains("bearer token")
    {
        return "The access token contains unsupported characters. Verify the token value and retry."
            .to_owned();
    }
    if stage == WorkerConnectionStage::Transport && error.code == ErrorCode::InvalidRequest {
        return "Invalid worker URL. Use a ws:// or wss:// WebSocket URL with the worker's WebSocket path.".to_owned();
    }
    if error.code == ErrorCode::DeadlineExceeded
        || lower_message.contains("timed out")
        || lower_message.contains("timeout")
    {
        return "The connection timed out. Check that the worker is reachable and retry."
            .to_owned();
    }
    if lower_message.contains("connection refused")
        || lower_message.contains("actively refused")
        || lower_message.contains("os error 111")
    {
        return "The worker refused the connection. Check that its server is running and the URL and port are correct.".to_owned();
    }
    match stage {
        #[cfg(target_family = "wasm")]
        WorkerConnectionStage::Bootstrap => {
            let detail = redact_secret(&error.message, secret.unwrap_or_default());
            format!(
                "Connected to the worker, but could not open its workspace: {}",
                detail
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(240)
                    .collect::<String>()
            )
        }
        WorkerConnectionStage::Transport => {
            "Could not connect to the worker. Check the WebSocket URL, network access, and firewall, then retry.".to_owned()
        }
        WorkerConnectionStage::Negotiation => format!(
            "The WebSocket opened, but protocol negotiation failed ({}). Update the worker and UI to compatible versions.",
            error.code
        ),
        WorkerConnectionStage::Status => format!(
            "Protocol negotiation succeeded, but the worker status request failed ({}). Check that a compatible Loom worker is running.",
            error.code
        ),
        WorkerConnectionStage::CredentialRead | WorkerConnectionStage::InputValidation => unreachable!(),
        WorkerConnectionStage::CredentialSave | WorkerConnectionStage::BootstrapSave => {
            unreachable!()
        }
    }
}

fn connection_placeholder(
    id: u64,
    url: String,
    connection_state: WorkerConnectionState,
    connection_detail: Option<String>,
) -> WorkerNodeEntry {
    WorkerNodeEntry {
        id,
        status: WorkerNodeStatus {
            node_id: url.clone(),
            name: safe_worker_url_label(&url),
            online: false,
            capabilities: CapabilitySet::default(),
            resources: WorkerNodeResources {
                cpu_count: 0,
                cpu_usage_percent: None,
                memory_usage_percent: None,
                memory_total_bytes: None,
                memory_available_bytes: None,
                disk_total_bytes: None,
                disk_available_bytes: None,
            },
        },
        is_local: false,
        url: Some(url),
        connection: None,
        connection_state,
        connection_detail,
        severe_load_streak: 0,
    }
}

fn transition_worker_connection_to_connecting(
    state: &mut WorkerConnectionState,
    has_connection: bool,
) -> Result<(), &'static str> {
    match (*state, has_connection) {
        (WorkerConnectionState::Connecting, _) => {
            Err("A connection attempt for this worker is already in progress.")
        }
        (WorkerConnectionState::Connected, true) => Err("This worker is already connected."),
        _ => {
            *state = WorkerConnectionState::Connecting;
            Ok(())
        }
    }
}

fn mark_worker_connection_failed(node: &mut WorkerNodeEntry, detail: String) -> bool {
    let cleanup_failed = node
        .connection
        .take()
        .is_some_and(|connection| connection.close().is_err());
    node.status.online = false;
    node.connection_state = WorkerConnectionState::Failed;
    node.connection_detail = Some(detail);
    cleanup_failed
}

fn safe_worker_url_label(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or_default();
    if let Some((scheme, remainder)) = without_query.split_once("://") {
        if let Some((authority, path)) = remainder.split_once('/') {
            let authority = authority.rsplit('@').next().unwrap_or(authority);
            return format!("{scheme}://{authority}/{path}");
        }
        let authority = remainder.rsplit('@').next().unwrap_or(remainder);
        return format!("{scheme}://{authority}");
    }
    without_query
        .rsplit('@')
        .next()
        .unwrap_or(without_query)
        .to_owned()
}

fn worker_url_embeds_credential(url: &str) -> bool {
    let Some((_, remainder)) = url.split_once("://") else {
        return false;
    };
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return true;
    }
    let Some(query) = url.split_once('?').map(|(_, query)| query) else {
        return false;
    };
    query.split('&').any(|parameter| {
        let key = parameter.split('=').next().unwrap_or_default();
        matches!(
            key.to_ascii_lowercase().as_str(),
            "token"
                | "access_token"
                | "auth"
                | "authorization"
                | "bearer"
                | "key"
                | "api_key"
                | "password"
                | "secret"
                | "client_secret"
        )
    })
}

fn remove_worker_node_entry(nodes: &mut Vec<WorkerNodeEntry>, id: u64) -> Option<WorkerNodeEntry> {
    let index = nodes.iter().position(|node| node.id == id)?;
    if nodes[index].is_local() {
        return None;
    }
    Some(nodes.remove(index))
}

fn update_worker_node_status(
    nodes: &mut [WorkerNodeEntry],
    id: u64,
    status: WorkerNodeStatus,
) -> Option<String> {
    let node = nodes.iter_mut().find(|node| node.id == id)?;
    let recovered = !node.status.online && status.online;
    let message = recovered.then(|| format!("Worker node {} is online", status.name));
    node.status = status;
    message
}

fn initial_worker_nodes(
    local_status: WorkerNodeStatus,
    local_connection: ClientConnection,
    config: &WorkspaceConfig,
    local_url: Option<&str>,
) -> Vec<WorkerNodeEntry> {
    let mut nodes = vec![WorkerNodeEntry {
        id: ACTIVE_BACKEND_NODE_ENTRY_ID,
        status: local_status,
        is_local: true,
        url: None,
        connection: Some(local_connection),
        connection_state: WorkerConnectionState::Connected,
        connection_detail: None,
        severe_load_streak: 0,
    }];
    for configured in &config.worker_nodes {
        if local_url == Some(configured.url.as_str()) {
            continue;
        }
        nodes.push(connection_placeholder(
            nodes.len() as u64,
            configured.url.clone(),
            WorkerConnectionState::Disconnected,
            None,
        ));
    }
    nodes
}

pub(crate) struct LoomView {
    #[cfg(target_family = "wasm")]
    connected: bool,
    /// Used for the synchronous bootstrap before the window exists.
    pub(crate) connection: ClientConnection,
    /// Used for every request made once the view is interactive.
    pub(crate) backend: BackendWorker,
    /// The startup backend remains the default for project-wide requests and new sessions.
    default_backend_node_id: String,
    /// Connections are keyed by the backend's stable node identity.
    node_backends: BTreeMap<String, BackendWorker>,
    /// Last known display names remain available for sessions after node removal.
    node_names: BTreeMap<String, String>,
    /// Sessions stay pinned to the node that created them.
    session_node_ids: BTreeMap<AgentSessionId, String>,
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
    pub(crate) auto_approve_actions: bool,
    session_auto_approve_actions: BTreeMap<AgentSessionId, bool>,
    pub(crate) session_task_cache: BTreeMap<AgentSessionId, String>,
    pub(crate) optimistic_messages: Vec<String>,
    pub(crate) sending_message: bool,
    pub(crate) models: Vec<ModelId>,
    default_models: Vec<ModelId>,
    node_model_catalogs: BTreeMap<String, Vec<ModelId>>,
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
    agent_mode_select: Option<Entity<ModelSelectState>>,
    agent_mode_select_subscription: Option<Subscription>,
    pub(crate) settings_open: bool,
    pub(crate) providers_open: bool,
    pub(crate) about_open: bool,
    pub(crate) providers: Vec<ProviderSummary>,
    providers_node_id: Option<String>,
    pub(crate) theme_choice: ThemeChoice,
    appearance_subscription: Option<Subscription>,
    pub(crate) after_sequence: Option<EventSequence>,
    pub(crate) timeline: Vec<TimelineItem>,
    timeline_view: Option<Entity<TimelineView>>,
    pub(crate) activity_records_seen: bool,
    pub(crate) expanded_activities: BTreeSet<ActivityId>,
    pub(crate) approval_request_in_flight: bool,
    approval_settings_request_in_flight: bool,
    pub(crate) archive_request_in_flight: bool,
    pub(crate) pending_approval: Option<ToolCall>,
    pub(crate) pending_input: Option<String>,
    pub(crate) composer: TextBufferState,
    pub(crate) composer_focus_handle: FocusHandle,
    pub(crate) node_focus_handle: FocusHandle,
    pub(crate) input_field: InputField,
    pub(crate) session_state: AgentSessionState,
    pub(crate) run_state: Option<AgentRunState>,
    pub(crate) summary: Option<String>,
    pub(crate) review: ReviewState,
    session_drawer_open: bool,
    pub(crate) tasks: Vec<TaskSnapshot>,
    pub(crate) rename_dialog: Option<RenameDialogState>,
    pub(crate) rename_focus_handle: FocusHandle,
    pub(crate) demo_workspace: bool,
    pub(crate) login_enabled: bool,
    pub(crate) github_connected: bool,
    pub(crate) github_login: Option<GitHubLoginState>,
    worker_nodes: Vec<WorkerNodeEntry>,
    next_worker_node_id: u64,
    worker_node_polls_scheduled: BTreeSet<u64>,
    workspace_config: WorkspaceConfig,
    pub(crate) node_input: TextBufferState,
    pub(crate) run_poll_scheduled: bool,
    #[cfg(target_family = "wasm")]
    browser_workspace: Option<String>,
    #[cfg(target_family = "wasm")]
    browser_model: Option<ModelId>,
    #[cfg(target_family = "wasm")]
    browser_window_initialized: bool,
    browser_startup_error: Option<String>,
}

struct TimelineView {
    parent: Entity<LoomView>,
    list_state: ListState,
    parent_subscription: Option<Subscription>,
    session_id: Option<AgentSessionId>,
    timeline_revision: (usize, usize),
}

impl TimelineView {
    fn new(parent: Entity<LoomView>) -> Self {
        Self {
            parent,
            list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            parent_subscription: None,
            session_id: None,
            timeline_revision: (0, 0),
        }
    }

    fn sync_list(
        &mut self,
        item_count: usize,
        timeline_revision: (usize, usize),
        session_changed: bool,
    ) {
        let content_changed = self.timeline_revision != timeline_revision;
        self.timeline_revision = timeline_revision;
        if self.list_state.item_count() == item_count {
            if session_changed || content_changed {
                self.list_state.scroll_to_end();
            }
            return;
        }

        self.list_state.reset(item_count);
        self.list_state.scroll_to_end();
    }
}

impl Render for TimelineView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.parent_subscription.is_none() {
            let parent = self.parent.clone();
            self.parent_subscription = Some(cx.observe(&parent, |_, _, cx| cx.notify()));
        }

        let parent_state = self.parent.read(cx);
        let item_count = parent_state.timeline.len();
        let session_id = parent_state.active_session.id;
        let session_changed = self.session_id != Some(session_id);
        if session_changed {
            self.session_id = Some(session_id);
        }
        let timeline_revision = (
            item_count,
            parent_state
                .timeline
                .last()
                .map(|item| format!("{item:?}").len())
                .unwrap_or_default(),
        );
        self.sync_list(item_count, timeline_revision, session_changed);
        if item_count == 0 {
            return div()
                .size_full()
                .p_6()
                .child(
                    div()
                        .w_full()
                        .p_5()
                        .rounded_lg()
                        .bg(rgb(0x171c25))
                        .border_1()
                        .border_color(rgb(0x293244))
                        .text_sm()
                        .text_color(rgb(0xb7c0d0))
                        .child(
                            div()
                                .text_base()
                                .text_color(rgb(0xf3f4f6))
                                .child("Ready when you are"),
                        )
                        .child(
                            div()
                                .mt_1()
                                .text_sm()
                                .text_color(rgb(0x8f98a6))
                                .child("Describe a task below and Loom will keep the work, decisions, and results together."),
                        ),
                );
        }

        let parent = self.parent.clone();
        let timeline = list(self.list_state.clone(), move |index, _window, cx| {
            let view = parent.read(cx);
            let item = &view.timeline[index];
            view.render_timeline_item(item, index, &parent)
        })
        .size_full();

        div().size_full().p_3().child(timeline)
    }
}

impl LoomView {
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

    pub(crate) fn input_state(&self, field: InputField) -> Option<&TextBufferState> {
        match field {
            InputField::Composer => Some(&self.composer),
            InputField::Rename => self.rename_dialog.as_ref().map(|dialog| &dialog.input),
            InputField::Node => Some(&self.node_input),
        }
    }

    pub(crate) fn input_state_mut(&mut self, field: InputField) -> Option<&mut TextBufferState> {
        match field {
            InputField::Composer => Some(&mut self.composer),
            InputField::Rename => self.rename_dialog.as_mut().map(|dialog| &mut dialog.input),
            InputField::Node => Some(&mut self.node_input),
        }
    }

    pub(crate) fn input_focus_handle(&self, field: InputField) -> FocusHandle {
        match field {
            InputField::Composer => self.composer_focus_handle.clone(),
            InputField::Rename => self.rename_focus_handle.clone(),
            InputField::Node => self.node_focus_handle.clone(),
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
        node_focus_handle: FocusHandle,
        rename_focus_handle: FocusHandle,
    ) -> Result<Self, LoomError> {
        info!("bootstrapping backend connection");
        let mut remote_cleanup_guard = None;
        let (connection, workspace_root, project_id, demo_workspace) = if let Some(remote_url) =
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
            let projects = list_projects(&connection)?;
            info!("remote backend returned {} project(s)", projects.len());
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
            info!("selected remote project {}", project.id);
            (connection, workspace_root, project.id, false)
        } else {
            let (workspace_root, demo_workspace) = prepare_workspace(options)?;
            info!(
                "using workspace '{}'{}",
                workspace_root.display(),
                if demo_workspace { " (demo)" } else { "" }
            );
            let project_id = if demo_workspace {
                ProjectId::new()
            } else {
                stable_project_id(&workspace_root)
            };
            let backend = if demo_workspace {
                info!("starting demo backend");
                InProcessBackend::demo_with_github_copilot()?
            } else if let Some(endpoint) = &options.endpoint {
                let persistence_path = backend_persistence_path(&workspace_root)?;
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
                let persistence_path = backend_persistence_path(&workspace_root)?;
                info!(
                    "starting local backend with GitHub Copilot; state '{}'",
                    persistence_path.display()
                );
                InProcessBackend::new_persistent_with_github_copilot(persistence_path)?
            };
            (
                ClientConnection::InProcess(backend.connect()),
                workspace_root,
                project_id,
                demo_workspace,
            )
        };
        if options.remote.is_none() {
            info!("negotiating protocol and opening workspace");
            negotiate(&connection)?;
            open_workspace(&connection, project_id, &workspace_root)?;
        }
        let node_status = worker_node_status(&connection)?;
        let default_backend_node_id = node_status.node_id.clone();
        let workspace_config = workspace_config(&connection, project_id)?;
        let worker_nodes = initial_worker_nodes(
            node_status,
            connection.clone(),
            &workspace_config,
            options.remote.as_deref(),
        );
        let sessions = list_sessions(&connection, project_id)?;
        info!("loaded {} session(s)", sessions.len());
        let session = sessions.into_iter().next().map_or_else(
            || {
                info!("creating a new session");
                create_session(&connection, project_id, "New session")
            },
            |session| {
                info!("resuming session {}", session.id);
                Ok(session)
            },
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
        let node_model_catalogs =
            BTreeMap::from([(default_backend_node_id.clone(), models.clone())]);
        info!(
            "loaded {} model(s); selected '{}'",
            models.len(),
            model.as_str()
        );
        let run = if demo_workspace {
            info!("starting demo agent run");
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
        let backend = BackendWorker::spawn(connection.clone());
        let node_backends = BTreeMap::from([(default_backend_node_id.clone(), backend.clone())]);
        let session_node_ids = BTreeMap::from([(session.id, default_backend_node_id.clone())]);
        let node_names = worker_nodes
            .iter()
            .map(|node| (node.status.node_id.clone(), worker_node_display_name(node)))
            .collect();
        let mut view = Self {
            backend,
            connection: connection.clone(),
            default_backend_node_id: default_backend_node_id.clone(),
            node_backends,
            node_names,
            session_node_ids,
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
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model,
            models: models.clone(),
            default_models: models,
            node_model_catalogs,
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
            agent_mode_select: None,
            agent_mode_select_subscription: None,
            settings_open: false,
            providers_open: false,
            about_open: false,
            providers: Vec::new(),
            providers_node_id: None,
            theme_choice: ThemeChoice::System,
            appearance_subscription: None,
            after_sequence: None,
            timeline: Vec::new(),
            timeline_view: None,
            activity_records_seen: false,
            expanded_activities: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer: TextBufferState::new(""),
            composer_focus_handle: focus_handle,
            node_focus_handle,
            input_field: InputField::Composer,
            session_state: session.state,
            run_state: run.as_ref().map(|run| run.state),
            summary: run.as_ref().and_then(|run| run.summary.clone()),
            review: ReviewState::default(),
            session_drawer_open: false,
            tasks: Vec::new(),
            rename_dialog: None,
            rename_focus_handle,
            demo_workspace,
            login_enabled: true,
            github_connected: false,
            github_login: None,
            next_worker_node_id: worker_nodes.len() as u64,
            worker_nodes,
            worker_node_polls_scheduled: BTreeSet::new(),
            workspace_config,
            node_input: TextBufferState::new(""),
            run_poll_scheduled: false,
            browser_startup_error: None,
        };
        view.refresh_models();
        view.refresh_sessions()?;
        let active_session = view.active_session.clone();
        view.load_session(active_session);
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
        node_focus_handle: FocusHandle,
        rename_focus_handle: FocusHandle,
    ) -> Self {
        let connection = ClientConnection::Disconnected;
        let backend = BackendWorker::spawn(connection.clone());
        let project_id = ProjectId::new();
        let timestamp = loom_core::Timestamp::from_unix_millis(0);
        let active_session = AgentSessionSnapshot {
            id: AgentSessionId::new(),
            project_id,
            name: "No worker connected".to_owned(),
            state: AgentSessionState::Idle,
            created_at: timestamp,
            updated_at: timestamp,
        };
        let node_input = TextBufferState::new(
            format!("{} {}", options.remote(), options.token())
                .trim()
                .to_owned(),
        );

        Self {
            connected: false,
            backend,
            connection,
            default_backend_node_id: String::new(),
            node_backends: BTreeMap::new(),
            node_names: BTreeMap::new(),
            session_node_ids: BTreeMap::new(),
            project_id,
            project: None,
            workspace_root: PathBuf::new(),
            projects: Vec::new(),
            sessions: Vec::new(),
            active_session,
            active_run: None,
            active_run_id: None,
            default_model: ModelId::new("default"),
            session_models: BTreeMap::new(),
            agent_mode: AgentMode::Agent,
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model: ModelId::new("default"),
            models: Vec::new(),
            default_models: Vec::new(),
            node_model_catalogs: BTreeMap::new(),
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
            agent_mode_select: None,
            agent_mode_select_subscription: None,
            settings_open: options.is_configured(),
            providers_open: false,
            about_open: false,
            providers: Vec::new(),
            providers_node_id: None,
            theme_choice: ThemeChoice::System,
            appearance_subscription: None,
            after_sequence: None,
            timeline: Vec::new(),
            timeline_view: None,
            activity_records_seen: false,
            expanded_activities: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer: TextBufferState::new(""),
            composer_focus_handle: focus_handle,
            node_focus_handle,
            input_field: InputField::Composer,
            session_state: AgentSessionState::Idle,
            run_state: None,
            summary: None,
            review: ReviewState::default(),
            session_drawer_open: false,
            tasks: Vec::new(),
            rename_dialog: None,
            rename_focus_handle,
            demo_workspace: false,
            login_enabled: false,
            github_connected: false,
            github_login: None,
            next_worker_node_id: 0,
            worker_nodes: Vec::new(),
            worker_node_polls_scheduled: BTreeSet::new(),
            workspace_config: WorkspaceConfig::default(),
            node_input,
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
        node_focus_handle: FocusHandle,
        rename_focus_handle: FocusHandle,
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
        let workspace_config = workspace_config_async(&connection, project_id).await?;
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
        let node_model_catalogs =
            BTreeMap::from([(default_backend_node_id.clone(), models.clone())]);
        let backend = BackendWorker::spawn(connection.clone());
        let node_backends = BTreeMap::from([(default_backend_node_id.clone(), backend.clone())]);
        let session_node_ids = BTreeMap::from([(session.id, default_backend_node_id.clone())]);
        let node_names = worker_nodes
            .iter()
            .map(|node| (node.status.node_id.clone(), worker_node_display_name(node)))
            .collect();

        let view = Self {
            connected: true,
            backend,
            connection: connection.clone(),
            default_backend_node_id: default_backend_node_id.clone(),
            node_backends,
            node_names,
            session_node_ids,
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
            auto_approve_actions: true,
            session_auto_approve_actions: BTreeMap::new(),
            session_task_cache: BTreeMap::new(),
            optimistic_messages: Vec::new(),
            sending_message: false,
            model,
            models: models.clone(),
            default_models: models,
            node_model_catalogs,
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
            agent_mode_select: None,
            agent_mode_select_subscription: None,
            settings_open: false,
            providers_open: false,
            about_open: false,
            providers: Vec::new(),
            providers_node_id: None,
            theme_choice: ThemeChoice::System,
            appearance_subscription: None,
            after_sequence: None,
            timeline: Vec::new(),
            timeline_view: None,
            activity_records_seen: false,
            expanded_activities: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer: TextBufferState::new(""),
            composer_focus_handle: focus_handle,
            node_focus_handle,
            input_field: InputField::Composer,
            session_state: session.state,
            run_state: None,
            summary: None,
            review: ReviewState::default(),
            session_drawer_open: false,
            tasks: Vec::new(),
            rename_dialog: None,
            rename_focus_handle,
            demo_workspace: false,
            login_enabled: true,
            github_connected: false,
            github_login: None,
            next_worker_node_id: worker_nodes.len() as u64,
            worker_nodes,
            worker_node_polls_scheduled: BTreeSet::new(),
            workspace_config,
            node_input: TextBufferState::new(""),
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

    fn dispatch_to_node(
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

    fn backend_for_request(&self, request: &ClientRequest) -> Result<BackendWorker, LoomError> {
        let Some(session_id) = session_id_for_request(request, self.active_session.id) else {
            return Ok(self.backend.clone());
        };
        self.backend_for_session(session_id)
    }

    fn backend_for_session(&self, session_id: AgentSessionId) -> Result<BackendWorker, LoomError> {
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
        self.timeline.push(TimelineItem::Status(status.into()));
    }

    pub(crate) fn record_backend_error(&mut self, operation: &str, error: LoomError) {
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
        let mut models = match list_models(&self.connection) {
            Ok(models) => models,
            Err(error) => {
                self.record_status(format!("Could not load available models: {error}"));
                return;
            }
        };
        for provider_id in provider_ids {
            let response = self.connection.request(RequestEnvelope::new(
                ClientRequest::DiscoverProviderModels {
                    provider_id: provider_id.clone(),
                },
            ));
            match response.result {
                Ok(ServerResponse::Models { models: discovered }) => {
                    models.extend(discovered.into_iter().map(|model| model.id));
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
        self.apply_models(models);
    }

    fn refresh_models_for_node_async(&mut self, node_id: String, cx: &mut Context<Self>) {
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
            view.update(cx, |view, cx| {
                view.model_refreshes_in_flight.remove(&node_id);
                match result {
                    Ok(catalog) => {
                        view.record_model_discovery_errors(catalog.discovery_errors);
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

    fn record_model_discovery_errors(
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
    fn apply_models(&mut self, models: Vec<ModelId>) {
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
        Ok(())
    }

    /// Reloads sessions from every connected node while keeping their owners.
    pub(crate) fn reload_sessions(&mut self, cx: &mut Context<Self>) {
        let project_id = self.project_id;
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
                        backend.submit(RequestEnvelope::new(ClientRequest::ListAgentSessions {
                            project_id: Some(project_id),
                            include_archived: false,
                        }))
                    });
            }
        }
        let project_request = self
            .backend
            .submit(RequestEnvelope::new(ClientRequest::ListProjects));
        cx.spawn(async move |view, cx| {
            let (node_responses, project_response) = cx
                .background_spawn(async move {
                    let mut node_responses = Vec::with_capacity(node_requests.len());
                    for (node_id, pending) in node_requests {
                        node_responses.push((node_id, pending.wait().await));
                    }
                    (node_responses, project_request.wait().await)
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
                match project_response.result {
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
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn reset_projection(&mut self) {
        self.timeline.clear();
        self.activity_records_seen = false;
        self.expanded_activities.clear();
        self.approval_request_in_flight = false;
        self.pending_approval = None;
        self.pending_input = None;
        self.active_run = None;
        self.active_run_id = None;
        self.run_state = None;
        self.summary = None;
        self.after_sequence = None;
    }

    fn enable_activity_projection(&mut self) {
        if self.activity_records_seen {
            return;
        }
        self.activity_records_seen = true;
        self.timeline.retain(|item| {
            !matches!(
                item,
                TimelineItem::ToolRequested { .. }
                    | TimelineItem::ToolStarted(_)
                    | TimelineItem::ToolOutput(_)
                    | TimelineItem::ToolCompleted { .. }
                    | TimelineItem::Approval { .. }
            )
        });
    }

    pub(crate) fn activate_session(&mut self, session: AgentSessionSnapshot) {
        self.active_session = session;
        self.session_state = self.active_session.state;
        self.auto_approve_actions = self
            .session_auto_approve_actions
            .get(&self.active_session.id)
            .copied()
            .unwrap_or(true);
        self.approval_settings_request_in_flight = false;
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
        let fallback_projection = match self
            .connection
            .request(RequestEnvelope::new(
                ClientRequest::GetAgentSessionSnapshot {
                    session_id: self.active_session.id,
                },
            ))
            .result
        {
            Ok(ServerResponse::AgentSessionSnapshot(projection)) => {
                self.active_session = projection.session.clone();
                self.session_state = self.active_session.state;
                self.auto_approve_actions = projection.auto_approve_actions;
                self.session_auto_approve_actions
                    .insert(self.active_session.id, projection.auto_approve_actions);
                if let Some(run) = &projection.active_run {
                    self.model = run.run.model.clone();
                    self.session_task_cache
                        .insert(self.active_session.id, run.run.task.clone());
                }
                Some(projection)
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
        if let Err(error) = self.collect_events_since(
            None,
            fallback_projection.and_then(|projection| projection.active_run),
        ) {
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
        fallback: Option<AgentRunSnapshotProjection>,
    ) -> Result<(), LoomError> {
        let response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(self.active_session.id),
                    after_sequence,
                }));
        match response.result? {
            ServerResponse::SessionEvents { events } => {
                self.reset_projection();
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
                if self.timeline.is_empty()
                    && let Some(projection) = fallback
                {
                    self.apply_run_projection(projection);
                }
            }
            response => return Err(unexpected_response("session event stream", response)),
        }
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
            #[cfg(target_family = "wasm")]
            {
                let promise = js_sys::Promise::new(&mut |resolve, _reject| {
                    if let Some(window) = web_sys::window() {
                        let _ = window
                            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 250);
                    }
                });
                let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
            }
            #[cfg(not(target_family = "wasm"))]
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

    fn update_session_list(&mut self) {
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
            }
            AgentEvent::PlanProposed { plan, .. } => self.timeline.push(TimelineItem::Plan {
                steps: plan
                    .steps
                    .iter()
                    .map(|step| step.description.clone())
                    .collect(),
                completed: BTreeSet::new(),
                active: None,
            }),
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
            AgentEvent::ContextInspected { .. } => {}
            AgentEvent::ProviderError { error, .. } | AgentEvent::ContextError { error, .. } => {
                self.timeline.push(TimelineItem::Error {
                    operation: "agent".to_owned(),
                    error: error.clone(),
                });
            }
            AgentEvent::ToolCallRequested { call, .. } => {
                if !self.activity_records_seen {
                    self.timeline.push(TimelineItem::ToolRequested {
                        name: call.name.clone(),
                        arguments: bounded(
                            &serde_json::to_string(&call.arguments).unwrap_or_default(),
                        ),
                    });
                }
            }
            AgentEvent::ToolApprovalRequired { call, .. } => {
                for item in &mut self.timeline {
                    if let TimelineItem::Approval { active, .. } = item {
                        *active = false;
                    }
                }
                self.pending_approval = Some(call.clone());
                if !self.activity_records_seen {
                    self.timeline.push(TimelineItem::Approval {
                        name: call.name.clone(),
                        active: true,
                    });
                }
            }
            AgentEvent::ToolPolicyEvaluated { .. } => {}
            AgentEvent::ToolApprovalDecided { .. } => {
                self.pending_approval = None;
                self.approval_request_in_flight = false;
                for item in &mut self.timeline {
                    if let TimelineItem::Approval { active, .. } = item {
                        *active = false;
                    }
                }
            }
            AgentEvent::ToolCallStarted { call, .. } => {
                if !self.activity_records_seen {
                    self.timeline
                        .push(TimelineItem::ToolStarted(call.name.clone()));
                }
            }
            AgentEvent::ToolOutputChunk { chunk, .. } => {
                if !self.activity_records_seen {
                    if let Some(TimelineItem::ToolOutput(output)) = self.timeline.last_mut() {
                        output.push_str(chunk);
                        *output = bounded(output);
                    } else {
                        self.timeline.push(TimelineItem::ToolOutput(bounded(chunk)));
                    }
                }
            }
            AgentEvent::ToolCallCompleted { result, .. } => {
                if !self.activity_records_seen {
                    self.timeline.push(TimelineItem::ToolCompleted {
                        name: result.name.clone(),
                        success: result.success,
                    });
                }
            }
            AgentEvent::ActivityRecorded { activity, .. } => {
                self.enable_activity_projection();
                upsert_activity(&mut self.timeline, activity.clone());
            }
            AgentEvent::NeedsInput { prompt, .. } => {
                self.pending_input = Some(prompt.clone());
                self.timeline.push(TimelineItem::NeedsInput(prompt.clone()));
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
            }
            AgentEvent::RunCompleted { snapshot } => {
                self.active_run = Some(snapshot.clone());
                self.active_run_id = Some(snapshot.id);
                self.run_state = Some(snapshot.state);
                self.session_state = session_state_for_run(snapshot.state);
                self.active_session.state = self.session_state;
                self.update_session_list();
                self.summary = snapshot.summary.clone();
                if let Some(summary) = &snapshot.summary
                    && !is_redundant_completion_summary(summary)
                {
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
        let activity_records = projection.activities;
        if !activity_records.is_empty() {
            self.enable_activity_projection();
        }
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
                    MessageRole::Tool if activity_records.is_empty() => {
                        timeline.push(TimelineItem::ToolOutput(bounded(&message.content)))
                    }
                    MessageRole::Tool => {}
                    MessageRole::System => {}
                }
            }
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
        }
        for activity in activity_records {
            upsert_activity(&mut self.timeline, activity);
        }
        if let Some(summary) = &projection.run.summary
            && !is_redundant_completion_summary(summary)
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
        if self.archive_request_in_flight {
            return;
        }
        self.archive_request_in_flight = true;
        self.record_status("Archiving session...");
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::ArchiveAgentSession { session_id },
            move |view, response, cx| {
                view.archive_request_in_flight = false;
                match response.result {
                    Ok(ServerResponse::AgentSessionArchived(snapshot)) => {
                        view.sessions.retain(|session| session.id != snapshot.id);
                        view.session_node_ids.remove(&snapshot.id);
                        if let Some(session) = view.sessions.first().cloned() {
                            view.select_session(session, cx);
                        } else {
                            view.reset_projection();
                            view.create_session_async("New session".to_owned(), cx);
                        }
                    }
                    Err(error) => view.record_backend_error("archive session", error),
                    Ok(response) => view.record_backend_error(
                        "archive session",
                        unexpected_response("session archive", response),
                    ),
                }
            },
        );
    }

    pub(crate) fn send_message(&mut self, message: String, cx: &mut Context<Self>) {
        let backend = match self.backend_for_session(self.active_session.id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("send message", error);
                cx.notify();
                return;
            }
        };
        if self.active_run_id.is_none()
            && !self.demo_workspace
            && self.model.as_str() != "deterministic/demo"
        {
            let node_id = self
                .session_node_ids
                .get(&self.active_session.id)
                .map(String::as_str)
                .unwrap_or_default();
            let validation = if self.model_catalog_node_id.as_deref() != Some(node_id) {
                Err("model availability has not been refreshed for this worker".to_owned())
            } else {
                validate_model_for_node(&self.node_model_catalogs, node_id, &self.model)
            };
            if let Err(reason) = validation {
                let node_name = self
                    .node_names
                    .get(node_id)
                    .map(String::as_str)
                    .unwrap_or(node_id);
                self.record_backend_error(
                    "start run",
                    LoomError::invalid_state(format!(
                        "Cannot start a run on {node_name}: {reason}. Choose a model configured on this worker before sending."
                    )),
                );
                cx.notify();
                return;
            }
        }
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
            backend.submit(RequestEnvelope::new(ClientRequest::RenameAgentSession {
                session_id,
                name: title,
            }))
        });
        let run_request = backend.submit(RequestEnvelope::new(request));
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

    fn approve_pending_action(&mut self, cx: &mut Context<Self>) {
        if self.approval_request_in_flight {
            return;
        }
        let (Some(run_id), Some(call)) = (self.active_run_id, self.pending_approval.clone()) else {
            return;
        };
        self.approval_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::ApproveAgentAction {
                run_id,
                tool_call_id: call.id,
            },
            |view, response, cx| view.finish_approval_response(response, cx),
        );
    }

    fn reject_pending_action(&mut self, cx: &mut Context<Self>) {
        if self.approval_request_in_flight {
            return;
        }
        let (Some(run_id), Some(call)) = (self.active_run_id, self.pending_approval.clone()) else {
            return;
        };
        self.approval_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::RejectAgentAction {
                run_id,
                tool_call_id: call.id,
                reason: None,
            },
            |view, response, cx| view.finish_approval_response(response, cx),
        );
    }

    fn finish_approval_response(&mut self, response: ResponseEnvelope, cx: &mut Context<Self>) {
        match response.result {
            Ok(ServerResponse::AgentRun(run)) | Ok(ServerResponse::AgentRunStarted(run)) => {
                self.active_run = Some(run.clone());
                self.active_run_id = Some(run.id);
                self.run_state = Some(run.state);
                self.session_state = session_state_for_run(run.state);
                self.active_session.state = self.session_state;
                self.start_run_polling(cx);
            }
            Err(error) => {
                self.approval_request_in_flight = false;
                self.record_backend_error("approval", error);
            }
            Ok(response) => {
                self.approval_request_in_flight = false;
                self.record_backend_error("approval", unexpected_response("approval", response))
            }
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

    pub(crate) fn focus_node(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.node_focus_handle.focus(window, cx);
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
        } else if self.input_field == InputField::Node {
            self.connect_worker_node(cx);
        } else {
            self.submit_composer(cx);
        }
        cx.notify();
    }

    pub(crate) fn select_session(&mut self, session: AgentSessionSnapshot, cx: &mut Context<Self>) {
        let backend = match self.backend_for_session(session.id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("select session", error);
                cx.notify();
                return;
            }
        };
        self.github_login = None;
        self.settings_open = false;
        self.providers_open = false;
        self.about_open = false;
        self.review.open = false;
        self.activate_session(session.clone());
        if let Some(node_id) = self.session_node_ids.get(&session.id).cloned()
            && self.model_catalog_node_id.as_deref() != Some(node_id.as_str())
        {
            self.refresh_models_for_node_async(node_id, cx);
        }
        self.ensure_session_task_message(session.id);
        let session_id = session.id;
        let snapshot_request = backend.submit(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot { session_id },
        ));
        cx.spawn(async move |view, cx| {
            let snapshot = cx
                .background_spawn(async move { snapshot_request.wait().await })
                .await;
            let events_request =
                backend.submit(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    after_sequence: None,
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
        let fallback_projection = match snapshot_response.result {
            Ok(ServerResponse::AgentSessionSnapshot(projection)) => {
                self.active_session = projection.session.clone();
                self.auto_approve_actions = projection.auto_approve_actions;
                self.session_auto_approve_actions
                    .insert(session_id, projection.auto_approve_actions);
                if let Some(run) = &projection.active_run {
                    self.session_task_cache
                        .insert(session_id, run.run.task.clone());
                    self.model = run.run.model.clone();
                }
                Some(projection)
            }
            Err(error) => {
                self.record_backend_error("load session snapshot", error);
                self.reset_projection();
                None
            }
            Ok(response) => {
                self.record_backend_error(
                    "load session snapshot",
                    unexpected_response("session snapshot", response),
                );
                self.reset_projection();
                None
            }
        };
        self.reset_projection();
        match events_response.result {
            Ok(ServerResponse::SessionEvents { events }) => {
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
                if self.timeline.is_empty()
                    && let Some(projection) = fallback_projection
                        .as_ref()
                        .and_then(|projection| projection.active_run.clone())
                {
                    self.apply_run_projection(projection);
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
                if self.timeline.is_empty()
                    && let Some(projection) = fallback_projection
                        .as_ref()
                        .and_then(|projection| projection.active_run.clone())
                {
                    self.apply_run_projection(projection);
                }
            }
            Err(error) => {
                if let Some(projection) = fallback_projection
                    .as_ref()
                    .and_then(|projection| projection.active_run.clone())
                {
                    self.apply_run_projection(projection);
                }
                self.record_backend_error("load session events", error);
            }
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
    fn start_github_login_flow(&mut self, cx: &mut Context<Self>) {
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
    fn start_github_login_flow(&mut self, cx: &mut Context<Self>) {
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

    fn handle_github_provider_configured(&mut self, node_id: String, cx: &mut Context<Self>) {
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

    fn sync_model_select_states(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self
            .models
            .iter()
            .map(|model| model.as_str().to_owned())
            .collect::<Vec<_>>();
        let default_items = self
            .default_models
            .iter()
            .map(|model| model.as_str().to_owned())
            .collect::<Vec<_>>();
        let model_value = self.model.as_str().to_owned();
        let default_model_value = self.default_model.as_str().to_owned();
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
                    if let SelectEvent::Confirm(Some(model)) = event {
                        view.select_model(ModelId::new(model.clone()), cx);
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
                    if let SelectEvent::Confirm(Some(model)) = event {
                        view.select_default_model(ModelId::new(model.clone()), cx);
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

    fn sync_agent_mode_select_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            ClientRequest::SetApprovalPolicy {
                project_id: self.active_session.project_id,
                session_id: Some(session_id),
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
            ClientRequest::SetApprovalPolicy {
                project_id: self.active_session.project_id,
                session_id: Some(session_id),
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
        cx.notify();
    }

    fn add_worker_node(
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

    fn begin_worker_node_connection(&mut self, url: &str) -> Result<u64, String> {
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

    fn fail_worker_node_connection(
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

    fn set_worker_node_connection_failure(&mut self, id: u64, url: &str, detail: String) {
        if let Some(node) = self
            .worker_nodes
            .iter_mut()
            .find(|node| node.id == id && !node.is_local && node.url.as_deref() == Some(url))
        {
            let _ = mark_worker_connection_failed(node, detail);
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn attach_reconnected_worker_node(
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
            let project_id = self.project_id;
            cx.spawn(async move |view, cx| {
                let result = cx
                    .background_spawn(
                        async move { PeerCredentialStore::new().delete(project_id, &url) },
                    )
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

    fn adjust_cpu_pulse_threshold(&mut self, delta: i8, cx: &mut Context<Self>) {
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

    fn persist_and_distribute_workspace_config(
        &self,
        retiring: Option<(String, ClientConnection)>,
        cx: &mut Context<Self>,
    ) {
        let project_id = self.project_id;
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
                    if let Err(error) = set_workspace_config(&source, project_id, config.clone()) {
                        errors.push(("save workspace config".to_owned(), error));
                    }
                    for (name, connection) in peers {
                        if let Err(error) =
                            set_workspace_config(&connection, project_id, config.clone())
                        {
                            errors.push((format!("distribute workspace config to {name}"), error));
                        }
                    }
                    if let Some((name, connection)) = retiring {
                        if let Err(error) =
                            set_workspace_config(&connection, project_id, config.clone())
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
                set_workspace_config_async(&source, project_id, config.clone()).await
            {
                errors.push(("save workspace config".to_owned(), error));
            } else {
                for (name, connection) in peers {
                    if let Err(error) =
                        set_workspace_config_async(&connection, project_id, config.clone()).await
                    {
                        errors.push((format!("distribute workspace config to {name}"), error));
                    }
                }
            }
            if let Some((name, connection)) = retiring {
                if let Err(error) =
                    set_workspace_config_async(&connection, project_id, config.clone()).await
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

    fn schedule_worker_node_poll(&mut self, cx: &mut Context<Self>) {
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
        let project_id = self.project_id;
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
                        let token = match credentials.get(project_id, &candidate_url) {
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

    fn poll_worker_node_once(&mut self, id: u64, cx: &mut Context<Self>) {
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
        let value = self.node_input.text.trim().to_owned();
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
        let project_id = self.project_id;
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
                        .set(project_id, &node_url, &token)
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
                        if view.node_input.text.trim() == submitted_value {
                            view.node_input.set_text("");
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
        let value = self.node_input.text.trim().to_owned();
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
                        if view.node_input.text.trim() == submitted_value {
                            view.node_input.set_text("");
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
    fn connect_browser_bootstrap(
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
        let node_focus_handle = self.node_focus_handle.clone();
        let rename_focus_handle = self.rename_focus_handle.clone();
        let view = cx.entity();
        cx.notify();
        cx.spawn(async move |_, cx| {
            let result = LoomView::try_new_browser(
                &options,
                focus_handle,
                node_focus_handle,
                rename_focus_handle,
            )
            .await;
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
                        view.select_session(active_session, cx);
                        if view.node_input.text.trim() == submitted_value {
                            view.node_input.set_text("");
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

    fn open_providers_for_node(&mut self, node_id: String, cx: &mut Context<Self>) {
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

    pub(crate) fn close_providers(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.providers_open = false;
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

    fn apply_appearance(
        &mut self,
        appearance: WindowAppearance,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::theme::apply_theme(appearance, cx);
        cx.notify();
    }

    pub(crate) fn new_session(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = format!("Session {}", self.sessions.len().saturating_add(1));
        let node_id = self.available_session_nodes().first().map_or_else(
            || self.default_backend_node_id.clone(),
            |(id, _)| id.clone(),
        );
        self.create_session_on_node(node_id, name, cx);
    }

    fn available_session_nodes(&self) -> Vec<(String, String)> {
        let nodes = self
            .worker_nodes
            .iter()
            .filter(|node| {
                node.status.online && self.node_backends.contains_key(&node.status.node_id)
            })
            .map(|node| (node.status.node_id.clone(), worker_node_display_name(node)))
            .collect::<Vec<_>>();
        order_session_nodes(nodes, &self.default_backend_node_id)
    }

    fn render_new_session_button(
        &self,
        view: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let targets = self.available_session_nodes();
        let button = Button::new("new-session")
            .icon(Icon::new(IconName::Plus))
            .ghost()
            .small()
            .tooltip("Create a new session");
        if targets.len() <= 1 {
            button
                .on_click(cx.listener(Self::new_session))
                .into_any_element()
        } else {
            let default_node_id = self.default_backend_node_id.clone();
            let view = view.clone();
            button
                .dropdown_menu(move |mut menu, _, _| {
                    for (node_id, node_name) in targets.clone() {
                        let target_view = view.clone();
                        let is_default = node_id == default_node_id;
                        let label = if is_default {
                            format!("{node_name} (default)")
                        } else {
                            node_name.clone()
                        };
                        let target_node_id = node_id.clone();
                        menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                            target_view.update(cx, |view, cx| {
                                let name =
                                    format!("Session {}", view.sessions.len().saturating_add(1));
                                view.create_session_on_node(target_node_id.clone(), name, cx);
                            });
                        }));
                    }
                    menu
                })
                .into_any_element()
        }
    }

    /// Creates a session through the selected node and pins it to that node.
    pub(crate) fn create_session_async(&mut self, name: String, cx: &mut Context<Self>) {
        self.create_session_on_node(self.default_backend_node_id.clone(), name, cx);
    }

    fn create_session_on_node(&mut self, node_id: String, name: String, cx: &mut Context<Self>) {
        let Some(backend) = self.node_backends.get(&node_id).cloned() else {
            self.record_backend_error(
                "create session",
                LoomError::new(
                    ErrorCode::NotFound,
                    format!("worker node {node_id} is not connected"),
                    false,
                ),
            );
            cx.notify();
            return;
        };
        let project_id = self.project_id;
        let workspace_root = self.workspace_root.display().to_string();
        let open_workspace = node_id != self.default_backend_node_id;
        let model = self.default_model.clone();
        let node_name = self
            .node_names
            .get(&node_id)
            .cloned()
            .unwrap_or_else(|| node_id.clone());
        cx.spawn(async move |view, cx| {
            let catalog = match list_models_from_backend(&backend).await {
                Ok(catalog) => catalog,
                Err(error) => {
                    view.update(cx, |view, cx| {
                        view.record_backend_error("check worker models", error);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let models = catalog.models;
            view.update(cx, |view, cx| {
                view.record_model_discovery_errors(catalog.discovery_errors);
                view.node_model_catalogs
                    .insert(node_id.clone(), models.clone());
                view.default_models = models.clone();
                cx.notify();
            })
            .ok();
            let node_models = BTreeMap::from([(node_id.clone(), models.clone())]);
            if let Err(reason) = validate_model_for_node(&node_models, &node_id, &model) {
                view.update(cx, |view, cx| {
                    view.record_backend_error(
                        "create session",
                        LoomError::invalid_state(format!(
                            "Cannot create a session on {node_name}: {reason}. Choose a model configured on this worker in Settings, then try again."
                        )),
                    );
                    cx.notify();
                })
                .ok();
                return;
            }
            let result = async {
                if open_workspace {
                    let response = backend
                        .submit(RequestEnvelope::new(ClientRequest::OpenWorkspace {
                            project_id,
                            root: workspace_root,
                        }))
                        .wait()
                        .await;
                    match response.result? {
                        ServerResponse::WorkspaceOpened(_) => {}
                        response => {
                            return Err(unexpected_response("workspace open", response));
                        }
                    }
                }
                let response = backend
                    .submit(RequestEnvelope::new(ClientRequest::CreateAgentSession {
                        project_id,
                        name,
                    }))
                    .wait()
                    .await;
                match response.result? {
                    ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
                    response => Err(unexpected_response("session creation", response)),
                }
            }
            .await;
            view.update(cx, |view, cx| match result {
                Ok(snapshot) => {
                    view.node_model_catalogs
                        .insert(node_id.clone(), models);
                    view.session_models.insert(snapshot.id, model);
                    view.session_node_ids
                        .insert(snapshot.id, node_id.clone());
                    view.sessions.push(snapshot.clone());
                    view.select_session(snapshot, cx);
                }
                Err(error) => view.record_backend_error("create session", error),
            })
            .ok();
        })
        .detach();
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
        self.providers_open = false;
        self.about_open = false;
        self.github_login = None;
        self.session_drawer_open = false;
        self.toggle_changes_sidebar(event, window, cx);
    }

    pub(crate) fn close_review(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.review.open = false;
        self.session_drawer_open = false;
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
            let node_indicator = self.render_session_node_indicator(session.id, index);
            let card =
                div()
                    .id(("session", index))
                    .relative()
                    .w_full()
                    .px_2()
                    .py_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(if active { rgb(0x3b5d85) } else { rgb(0x242833) })
                    .bg(if active { rgb(0x25334a) } else { rgb(0x1b1d24) })
                    .text_color(if active { rgb(0xf3f4f6) } else { rgb(0xb7c0d0) })
                    .cursor_pointer()
                    .hover(|style| style.bg(if active { rgb(0x293b56) } else { rgb(0x20242c) }))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div().text_sm().child(session.name.clone()).when(
                                    session.state == AgentSessionState::Archived,
                                    |element| element.text_color(rgb(0x64748b)),
                                ),
                            )
                            .child(node_indicator),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(state_color(session.state))
                            .child(session_state_label(session.state)),
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

    fn render_session_node_indicator(
        &self,
        session_id: AgentSessionId,
        index: usize,
    ) -> gpui_kit::AnyElement {
        let node_id = self.session_node_ids.get(&session_id).map(String::as_str);
        let node = session_owner_status(&self.worker_nodes, &self.session_node_ids, session_id);
        let status = node.map(|node| &node.status);
        let indicator_state =
            session_node_indicator_state(status, node.map_or(0, |node| node.severe_load_streak));
        let online = indicator_state != SessionNodeIndicatorState::Offline;
        let pulse = session_node_pulse(status, self.workspace_config.cpu_pulse_threshold_percent);
        let color = match indicator_state {
            SessionNodeIndicatorState::Offline => rgb(0x64748b),
            SessionNodeIndicatorState::Online => rgb(0x4ade80),
            SessionNodeIndicatorState::Severe => rgb(0xef4444),
        };
        let name = worker_node_name_for_id(&self.worker_nodes, &self.node_names, node_id);
        let metrics = status.filter(|status| status.online).map_or_else(
            || "CPU n/a · RAM n/a".to_owned(),
            |status| format_session_resource_percentages(Some(status)),
        );
        let tooltip_text = format!(
            "{}{}\n{}",
            name,
            if online { "" } else { " · Offline" },
            metrics
        );
        let dot = if let Some((period, amplitude)) = pulse {
            div()
                .w(px(6.))
                .h(px(6.))
                .rounded_full()
                .bg(color)
                .with_animation(
                    ("session-node-status-pulse", index),
                    Animation::new(period).repeat_synced().with_max_fps(24.),
                    move |element, progress| {
                        let eased_progress = progress * progress * (3. - 2. * progress);
                        let pulse = 0.5 - 0.5 * (eased_progress * std::f32::consts::TAU).cos();
                        let size = 6. + amplitude * pulse;
                        element.w(px(size)).h(px(size))
                    },
                )
                .into_any_element()
        } else {
            div()
                .w(px(6.))
                .h(px(6.))
                .rounded_full()
                .bg(color)
                .into_any_element()
        };
        div()
            .id(("session-node-indicator", index))
            .w(px(12.))
            .h(px(12.))
            .flex()
            .items_center()
            .justify_center()
            .tooltip(move |_, cx| {
                cx.new(|_| LoomTooltip {
                    text: tooltip_text.clone().into(),
                })
                .into()
            })
            .child(dot)
            .into_any_element()
    }

    pub(crate) fn render_model_picker(&self, phone: bool) -> impl IntoElement {
        let mut picker = div()
            .flex_1()
            .min_w(if phone { px(100.) } else { px(120.) })
            .w_full();
        if let Some(state) = &self.model_select {
            picker = picker.child(
                Select::new(state)
                    .id("session-model-select")
                    .w_full()
                    .small()
                    .accessibility_label("Model for this session")
                    .placeholder("No model is configured")
                    .search_placeholder("Search models"),
            );
        }
        picker
    }

    pub(crate) fn render_agent_mode_picker(&self, phone: bool) -> impl IntoElement {
        let mut picker = div().w(if phone { px(110.) } else { px(140.) });
        if let Some(state) = &self.agent_mode_select {
            picker = picker.child(
                Select::new(state)
                    .id("agent-mode-select")
                    .w_full()
                    .small()
                    .accessibility_label("Agent mode")
                    .placeholder("Select agent mode"),
            );
        }
        picker
    }

    fn render_activity_section(
        &self,
        activities: &[AgentActivityRecord],
        index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let model = activities.iter().find_map(|activity| match &activity.data {
            AgentActivityData::ModelTurn { model } => Some(model.as_str().to_owned()),
            _ => None,
        });
        let turn = activities
            .iter()
            .find(|activity| matches!(activity.data, AgentActivityData::ModelTurn { .. }));
        let title = activity_turn_title(activities);
        let turn_status = turn.map(|activity| {
            let duration = activity
                .elapsed_ms
                .map_or_else(String::new, format_duration);
            if duration.is_empty() {
                activity_status_label(activity.status).to_owned()
            } else {
                format!("{} · {duration}", activity_status_label(activity.status))
            }
        });
        let mut section = div()
            .id(("activity-section", index))
            .mx_2()
            .my_1()
            .pl_3()
            .border_l_1()
            .border_color(rgb(0x3b4555))
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("●")
                    .child(title)
                    .when_some(model, |element, model| {
                        element
                            .child("·")
                            .child(div().text_color(rgb(0x64748b)).child(model))
                    })
                    .when_some(turn_status, |element, status| {
                        element
                            .child("·")
                            .child(div().text_color(rgb(0x94a3b8)).child(status))
                    }),
            );
        for (activity_index, activity) in activities.iter().enumerate() {
            if matches!(activity.data, AgentActivityData::ModelTurn { .. }) {
                continue;
            }
            let (label, detail) = activity_label(activity);
            let duration = activity.elapsed_ms.map(format_duration);
            let output = activity_output(activity);
            let expanded = self.expanded_activities.contains(&activity.id);
            let status_color = match activity.status {
                AgentActivityStatus::Failed => rgb(0xfca5a5),
                AgentActivityStatus::Completed => rgb(0x9ad7bd),
                AgentActivityStatus::AwaitingApproval | AgentActivityStatus::AwaitingInput => {
                    rgb(0xfef3c7)
                }
                AgentActivityStatus::Started => rgb(0x93c5fd),
                AgentActivityStatus::Cancelled => rgb(0x94a3b8),
            };
            let activity_id = activity.id;
            let parent_for_toggle = parent.clone();
            let mut row = div()
                .id(("activity", ((index as u64) << 32) | activity_index as u64))
                .flex()
                .flex_col()
                .cursor_pointer()
                .text_xs()
                .text_color(rgb(0xcbd5e1))
                .on_click(move |_, _, cx| {
                    parent_for_toggle.update(cx, |this, cx| this.toggle_activity(activity_id, cx));
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(12.))
                                .text_color(status_color)
                                .child(activity_marker(activity.status)),
                        )
                        .child(if expanded { "⌄" } else { "›" })
                        .child(label)
                        .flex_1()
                        .child(
                            div()
                                .text_color(status_color)
                                .child(activity_status_label(activity.status)),
                        )
                        .when_some(duration, |element, duration| {
                            element
                                .text_color(rgb(0x64748b))
                                .child(format!(" · {duration}"))
                        }),
                );
            if expanded {
                if let Some(detail) = detail {
                    row = row.child(
                        div()
                            .ml(px(26.))
                            .max_w(px(560.))
                            .text_color(rgb(0x94a3b8))
                            .child(detail),
                    );
                }
                if let Some(output) = output {
                    row = row.child(
                        div()
                            .ml(px(26.))
                            .max_w(px(560.))
                            .border_l_1()
                            .border_color(rgb(0x30343f))
                            .pl_2()
                            .text_color(rgb(0x8f98a6))
                            .child(bounded_to(output, 420)),
                    );
                }
            }
            if activity.status == AgentActivityStatus::AwaitingApproval
                && self.pending_approval.is_some()
                && !self.approval_request_in_flight
            {
                let parent_for_approve = parent.clone();
                let parent_for_reject = parent.clone();
                row = row.child(
                    div()
                        .ml(px(26.))
                        .mt_1()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new((
                                "approve-activity",
                                ((index as u64) << 32) | activity_index as u64,
                            ))
                            .label("Approve")
                            .success()
                            .small()
                            .on_click(move |_, _, cx| {
                                parent_for_approve
                                    .update(cx, |this, cx| this.approve_pending_action(cx));
                            }),
                        )
                        .child(
                            Button::new((
                                "reject-activity",
                                ((index as u64) << 32) | activity_index as u64,
                            ))
                            .label("Reject")
                            .danger()
                            .small()
                            .on_click(move |_, _, cx| {
                                parent_for_reject
                                    .update(cx, |this, cx| this.reject_pending_action(cx));
                            }),
                        ),
                );
            } else if activity.status == AgentActivityStatus::AwaitingApproval
                && self.approval_request_in_flight
            {
                row = row.child(
                    div()
                        .ml(px(26.))
                        .mt_1()
                        .text_color(rgb(0x94a3b8))
                        .child("Submitting approval..."),
                );
            }
            section = section.child(row);
        }
        section.into_any()
    }

    fn toggle_activity(&mut self, activity_id: ActivityId, cx: &mut Context<Self>) {
        if !self.expanded_activities.remove(&activity_id) {
            self.expanded_activities.insert(activity_id);
        }
        cx.notify();
    }

    pub(crate) fn render_timeline_item(
        &self,
        item: &TimelineItem,
        index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let user_background = rgb(0x20242c);
        let user_foreground = rgb(0xdbeafe);
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
                        .child(render_timeline_text(
                            format!("transcript-user-{index}"),
                            text.clone(),
                            0xdbeafe,
                        )),
                )
                .into_any(),
            TimelineItem::Assistant(text) => div()
                .px_3()
                .py_2()
                .text_color(rgb(0xf3f4f6))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("GitHub Copilot"),
                )
                .child(render_timeline_text(
                    format!("transcript-assistant-{index}"),
                    text.clone(),
                    0xf3f4f6,
                ))
                .into_any(),
            TimelineItem::ActivitySection { activities } => {
                self.render_activity_section(activities, index, parent)
            }
            TimelineItem::Plan {
                steps,
                completed,
                active,
            } => {
                let mut card = div()
                    .px_3()
                    .py_2()
                    .text_color(rgb(0xb7c0d0))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Plan"));
                for (index, step) in steps.iter().enumerate() {
                    let index = index as u32;
                    let marker = if completed.contains(&index) {
                        "✓"
                    } else if active == &Some(index) {
                        ">"
                    } else {
                        "○"
                    };
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(if completed.contains(&index) {
                                rgb(0x9ad7bd)
                            } else {
                                rgb(0xb7c0d0)
                            })
                            .child(format!("{marker} {}", step)),
                    );
                }
                card.into_any()
            }
            TimelineItem::ToolRequested { name, arguments } => div()
                .px_3()
                .py_1()
                .text_color(rgb(0xcbd5e1))
                .child(format!(">_  {name}"))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child(arguments.clone()),
                )
                .into_any(),
            TimelineItem::Approval { name, active } => {
                let mut row = div()
                    .px_3()
                    .py_1()
                    .text_color(if *active {
                        rgb(0xfef3c7)
                    } else {
                        rgb(0x94a3b8)
                    })
                    .child(if *active {
                        format!("Approval required  >_ {name}")
                    } else {
                        format!("Approval resolved  >_ {name}")
                    });
                if *active && self.pending_approval.is_some() && !self.approval_request_in_flight {
                    let parent_for_approve = parent.clone();
                    let parent_for_reject = parent.clone();
                    row = row.child(
                        div()
                            .mt_1()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new(("approve-legacy", index))
                                    .label("Approve")
                                    .success()
                                    .small()
                                    .on_click(move |_, _, cx| {
                                        parent_for_approve
                                            .update(cx, |this, cx| this.approve_pending_action(cx));
                                    }),
                            )
                            .child(
                                Button::new(("reject-legacy", index))
                                    .label("Reject")
                                    .danger()
                                    .small()
                                    .on_click(move |_, _, cx| {
                                        parent_for_reject
                                            .update(cx, |this, cx| this.reject_pending_action(cx));
                                    }),
                            ),
                    );
                } else if *active && self.approval_request_in_flight {
                    row = row.child(
                        div()
                            .mt_1()
                            .text_color(rgb(0x94a3b8))
                            .child("Submitting approval..."),
                    );
                }
                row.into_any()
            }
            TimelineItem::ToolStarted(name) => div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(rgb(0xcbd5e1))
                .child(format!(">_  {name}"))
                .into_any(),
            TimelineItem::ToolOutput(output) => div()
                .mx_3()
                .px_2()
                .py_1()
                .border_l_1()
                .border_color(rgb(0x30343f))
                .text_xs()
                .text_color(rgb(0x94a3b8))
                .child(output.clone())
                .into_any(),
            TimelineItem::ToolCompleted { name, success } => div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(if *success {
                    rgb(0x9ad7bd)
                } else {
                    rgb(0xfca5a5)
                })
                .child(format!(
                    ">_  {name} [{}]",
                    if *success { "ok" } else { "failed" }
                ))
                .into_any(),
            TimelineItem::Status(status) => div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(rgb(0x94a3b8))
                .child(status.clone())
                .into_any(),
            TimelineItem::Error { operation, error } => div()
                .p_2()
                .rounded_lg()
                .bg(rgb(ERROR_CARD_SURFACE))
                .border_1()
                .border_color(rgb(ERROR_CARD_ACCENT))
                .text_sm()
                .text_color(rgb(ERROR_CARD_FOREGROUND))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(ERROR_CARD_ACCENT))
                        .child(format!("{} · {}", operation, error.code)),
                )
                .child(div().mt_1().child(render_timeline_text(
                    format!("timeline-error-{index}"),
                    error.message.clone(),
                    ERROR_CARD_FOREGROUND,
                )))
                .when(error.retryable, |element| {
                    element.child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(ERROR_CARD_ACCENT))
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
                .child(render_timeline_text(
                    format!("timeline-input-{index}"),
                    prompt.clone(),
                    0xe9d5ff,
                ))
                .into_any(),
            TimelineItem::Summary { text, evidence } => {
                let mut card = div()
                    .px_3()
                    .py_2()
                    .text_color(rgb(0xf3f4f6))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child("GitHub Copilot"),
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

    fn timeline_entity(&mut self, cx: &mut Context<Self>) -> Entity<TimelineView> {
        if let Some(timeline_view) = &self.timeline_view {
            return timeline_view.clone();
        }

        let parent = cx.entity();
        let timeline_view = cx.new(|_| TimelineView::new(parent));
        self.timeline_view = Some(timeline_view.clone());
        timeline_view
    }

    pub(crate) fn render_review(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let layout = responsive_layout(window.bounds().size.width);
        let title = match self.review.panel {
            ReviewPanel::Changes => "Changed files",
            ReviewPanel::Diff => "Diff",
            ReviewPanel::Evidence => "Evidence",
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
                            .child(format!(
                                "{}  {}",
                                change_kind_label(change.kind),
                                change.path
                            ))
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
                                .child(format!("Git · {:?}  {}", file.worktree, file.path))
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
                        .child("A read-only view of the changes in this session."),
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
                        .child("What was completed and how it was checked."),
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
            .when(layout.phone, |element| {
                element.size_full().absolute().top(px(0.)).left(px(0.))
            })
            .when(!layout.phone, |element| {
                element.w(layout.review_width).h_full()
            })
            .flex()
            .flex_col()
            .bg(rgb(0x17191f))
            .border_l_1()
            .border_color(rgb(0x30343f))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(0x293244))
                    .child(div().text_xs().text_color(rgb(0x93c5fd)).child(title))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(!layout.phone, |element| {
                                element.child(
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
                            })
                            .when(!layout.phone, |element| {
                                element.child(
                                    Button::new("close-review")
                                        .icon(Icon::new(IconName::FileText))
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.review.panel = ReviewPanel::Changes;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new("toggle-review-sidebar-close")
                                    .label("Close")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(Self::close_review)),
                            ),
                    ),
            )
            .child(body)
    }

    fn render_composer(
        &self,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
            .p_3()
            .bg(rgb(0x17191f))
            .border_t_1()
            .border_color(rgb(0x30343f))
            .child(
                div()
                    .w_full()
                    .min_h(px(46.))
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x10141b))
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
                div()
                    .mt_2()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .relative()
                            .items_center()
                            .gap_1()
                            .child(self.render_agent_mode_picker(layout.phone))
                            .child(self.render_model_picker(layout.phone))
                            .when(self.sending_message, |element| {
                                element.child(
                                    div().text_xs().text_color(rgb(0x64748b)).child("Working…"),
                                )
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
            #[cfg(not(target_family = "wasm"))]
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
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child("Connect GitHub Copilot"),
                    )
                    .child(
                        div()
                            .id("close-github-login")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close GitHub connection".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.github_login = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(div().mt_4().child(body))
            .into_any()
    }

    pub(crate) fn render_settings_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.default_model_select.as_ref().map_or_else(
            || div().into_any_element(),
            |state| {
                Select::new(state)
                    .id("default-model-select")
                    .w_full()
                    .small()
                    .accessibility_label("Default model for new sessions")
                    .placeholder("No configured models are available")
                    .search_placeholder("Search models")
                    .into_any_element()
            },
        );
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
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close settings".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_settings)),
                    ),
            )
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("SESSION SETTINGS"),
            )
            .child(
                div()
                    .mt_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .child("Auto-approve non-destructive actions")
                            .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                                "In Agent and Edit, writes, commands, and network won't prompt",
                            )),
                    )
                    .child(
                        Button::new("session-auto-approve-toggle")
                            .label(if self.auto_approve_actions {
                                "On"
                            } else {
                                "Off"
                            })
                            .small()
                            .disabled(
                                !self.is_connected() || self.approval_settings_request_in_flight,
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.toggle_auto_approve_actions(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("DEFAULT MODEL FOR NEW SESSIONS"),
            )
            .child(div().mt_2().child(body))
            .child(
                div()
                    .mt_4()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("WORKER NODES"),
            )
            .when_some(self.browser_startup_error.as_deref(), |element, error| {
                element.child(
                    div()
                        .mt_2()
                        .text_xs()
                        .text_color(rgb(0xfca5a5))
                        .child(error.to_owned()),
                )
            })
            .child(
                div()
                    .mt_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .child("Session indicator pulse threshold")
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child("Pulse when CPU usage is above this value"),
                            ),
                    )
                    .child(
                        Button::new("cpu-pulse-threshold-decrease")
                            .label("-")
                            .small()
                            .disabled(
                                !self.is_connected()
                                    || self.workspace_config.cpu_pulse_threshold_percent == 0,
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.adjust_cpu_pulse_threshold(-1, cx);
                            })),
                    )
                    .child(div().w(px(44.)).text_center().text_sm().child(format!(
                        "{}%",
                        self.workspace_config.cpu_pulse_threshold_percent.min(100)
                    )))
                    .child(
                        Button::new("cpu-pulse-threshold-increase")
                            .label("+")
                            .small()
                            .disabled(
                                !self.is_connected()
                                    || self.workspace_config.cpu_pulse_threshold_percent >= 100,
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.adjust_cpu_pulse_threshold(1, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .mt_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when(self.worker_nodes.is_empty(), |element| {
                        element.child(
                            div()
                                .p_2()
                                .rounded_lg()
                                .bg(rgb(0x171c25))
                                .border_1()
                                .border_color(rgb(0x293244))
                                .text_xs()
                                .text_color(rgb(0x8f98a6))
                                .child(
                                    "No worker connected. Add one below to load your workspace.",
                                ),
                        )
                    })
                    .children(self.worker_nodes.iter().map(|node| {
                        let id = node.id;
                        let status = &node.status;
                        let node_id = status.node_id.clone();
                        let resources = &status.resources;
                        let connection_label = match node.connection_state {
                            WorkerConnectionState::Disconnected => "not connected",
                            WorkerConnectionState::Connecting => "connecting",
                            WorkerConnectionState::Connected if status.online => {
                                "connected · online"
                            }
                            WorkerConnectionState::Connected => "connected · offline",
                            WorkerConnectionState::Failed => "connection failed",
                        };
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .p_2()
                                    .rounded_lg()
                                    .bg(rgb(0x171c25))
                                    .border_1()
                                    .border_color(rgb(0x293244))
                                    .text_xs()
                                    .child(div().text_color(rgb(0xe5e7eb)).child(format!(
                                        "{} · {} · {}",
                                        worker_node_display_name(node),
                                        connection_label,
                                        format_worker_node_resources(resources),
                                    )))
                                    .when_some(
                                        node.connection_detail.as_deref(),
                                        |element, detail| {
                                            element.child(
                                                div()
                                                    .mt_1()
                                                    .text_xs()
                                                    .text_color(
                                                        if node.connection_state
                                                            == WorkerConnectionState::Failed
                                                        {
                                                            rgb(0xfca5a5)
                                                        } else {
                                                            rgb(0xfcd34d)
                                                        },
                                                    )
                                                    .child(detail.to_owned()),
                                            )
                                        },
                                    ),
                            )
                            .when(!node.is_local, |element| {
                                element.child(
                                    Button::new(format!("remove-worker-node-{id}"))
                                        .label("Remove")
                                        .small()
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.remove_worker_node(id, cx)
                                        })),
                                )
                            })
                            .when(
                                !node.is_local && self.node_backends.contains_key(&node_id),
                                |element| {
                                    element.child(
                                        Button::new(format!("worker-node-providers-{id}"))
                                            .label("Providers")
                                            .small()
                                            .on_click(cx.listener(move |view, _, _, cx| {
                                                view.open_providers_for_node(node_id.clone(), cx)
                                            })),
                                    )
                                },
                            )
                    })),
            )
            .child(
                div()
                    .mt_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(30.))
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(0x171c25))
                            .border_1()
                            .border_color(rgb(0x293244))
                            .text_xs()
                            .text_color(rgb(0xb7c0d0))
                            .key_context("Composer")
                            .track_focus(&self.node_focus_handle)
                            .cursor(CursorStyle::IBeam)
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::focus_node))
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
                                field: InputField::Node,
                            }),
                    )
                    .child(
                        Button::new("connect-worker-node")
                            .label("Connect")
                            .small()
                            .on_click(cx.listener(|view, _, _, cx| view.connect_worker_node(cx))),
                    ),
            )
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(rgb(0x64748b))
                    .child("Use: ws://host:port/ws token · URLs are shared; access tokens are not"),
            )
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("THEME"),
            )
            .child(
                div().mt_2().flex().flex_wrap().gap_1().children(
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
            .into_any()
    }

    pub(crate) fn render_about_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("about-dialog")
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
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child("About Loom"),
                    )
                    .child(
                        div()
                            .id("close-about")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close about".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_about)),
                    ),
            )
            .child(
                div()
                    .mt_8()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(div().text_lg().text_color(rgb(0xf3f4f6)).child("Loom"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("Local agent workspace"),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(0x64748b))
                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    ),
            )
            .into_any()
    }

    pub(crate) fn render_providers_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let node_name = self
            .providers_node_id
            .as_ref()
            .and_then(|node_id| self.node_names.get(node_id))
            .map_or("worker", String::as_str);
        let github_provider = self
            .providers
            .iter()
            .find(|provider| provider.kind == ProviderKind::GitHubCopilot);
        let local_providers = self
            .providers
            .iter()
            .filter(|provider| provider.kind != ProviderKind::GitHubCopilot)
            .collect::<Vec<_>>();
        let github_status = if self.github_connected {
            "Connected"
        } else {
            "Not connected"
        };
        let github_models = github_provider.map_or(0, |provider| provider.models.len());

        let mut local_body = div().flex().flex_col().gap_1();
        if local_providers.is_empty() {
            local_body = local_body.child(
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x171c25))
                    .border_1()
                    .border_color(rgb(0x293244))
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("No other providers are configured."),
            );
        } else {
            for provider in local_providers {
                local_body =
                    local_body.child(
                        div()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(0x171c25))
                            .border_1()
                            .border_color(rgb(0x293244))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0xf3f4f6))
                                    .child(provider.display_name.clone()),
                            )
                            .child(div().mt_1().text_xs().text_color(rgb(0x8f98a6)).child(
                                format!(
                                    "{} model{} available",
                                    provider.models.len(),
                                    if provider.models.len() == 1 { "" } else { "s" }
                                ),
                            )),
                    );
            }
        }

        let mut github_card =
            div()
                .p_3()
                .rounded_lg()
                .bg(rgb(0x171c25))
                .border_1()
                .border_color(rgb(0x293244))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(0xf3f4f6))
                                .child("GitHub Copilot"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(if self.github_connected {
                                    rgb(0x9ad7bd)
                                } else {
                                    rgb(0xfef3c7)
                                })
                                .child(github_status),
                        ),
                )
                .child(div().mt_1().text_xs().text_color(rgb(0x8f98a6)).child(
                    if github_models == 0 {
                        "Connect your GitHub account to use Copilot models.".to_owned()
                    } else {
                        format!(
                            "{} model{} available",
                            github_models,
                            if github_models == 1 { "" } else { "s" }
                        )
                    },
                ));
        if self.login_enabled && !self.github_connected {
            github_card = github_card.child(
                div()
                    .id("connect-github-provider")
                    .mt_3()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(0x2563eb))
                    .hover(|style| style.bg(rgb(0x1d4ed8)))
                    .text_xs()
                    .text_color(rgb(0xffffff))
                    .cursor_pointer()
                    .child("Connect GitHub Copilot")
                    .on_click(cx.listener(Self::toggle_github_login)),
            );
        }

        div()
            .id("providers-dialog")
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
                    .child(
                        div()
                            .child(div().text_sm().text_color(rgb(0xf3f4f6)).child("Providers"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child(format!("Configured on {node_name}")),
                            ),
                    )
                    .child(
                        div()
                            .id("close-providers")
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(rgb(0x20242c))
                            .hover(|style| style.bg(rgb(0x293244)))
                            .text_color(rgb(0xb7c0d0))
                            .cursor_pointer()
                            .tooltip(|_, cx| {
                                cx.new(|_| LoomTooltip {
                                    text: "Close providers".into(),
                                })
                                .into()
                            })
                            .child(Icon::new(IconName::Close).size_4())
                            .on_click(cx.listener(Self::close_providers)),
                    ),
            )
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("GITHUB COPILOT"),
            )
            .child(div().mt_2().child(github_card))
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("LOCAL PROVIDER"),
            )
            .child(div().mt_2().child(local_body))
            .into_any()
    }

    fn render_session_sidebar(
        &self,
        view: &Entity<Self>,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .w(layout.sidebar_width)
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
                    .child(div().text_sm().text_color(rgb(0xf3f4f6)).child("Workspace"))
                    .when(layout.phone, |element| {
                        element.child(
                            Button::new("close-session-drawer")
                                .label("Close")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.session_drawer_open = false;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Sessions"))
                    .child(self.render_new_session_button(view, cx)),
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
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(0x30343f))
                    .pt_2()
                    .child(
                        Button::new("account-menu")
                            .icon(Icon::new(IconName::User))
                            .ghost()
                            .small()
                            .dropdown_menu({
                                let view = view.clone();
                                move |menu, _, _| {
                                    let providers_view = view.clone();
                                    let about_view = view.clone();
                                    menu.item(PopupMenuItem::new("Providers").on_click(
                                        move |_, _, cx| {
                                            providers_view.update(cx, |view, cx| {
                                                view.open_providers_from_menu(cx);
                                            });
                                        },
                                    ))
                                    .item(
                                        PopupMenuItem::new("About Loom").on_click(
                                            move |_, _, cx| {
                                                about_view.update(cx, |view, cx| {
                                                    view.open_about_from_menu(cx);
                                                });
                                            },
                                        ),
                                    )
                                }
                            }),
                    )
                    .child(
                        Button::new("settings-button")
                            .icon(Icon::new(IconName::Settings))
                            .ghost()
                            .small()
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    view.update(cx, |view, cx| {
                                        view.open_settings_from_menu(cx);
                                    });
                                }
                            }),
                    ),
            )
    }

    #[cfg(target_family = "wasm")]
    fn render_disconnected(&self, window: &Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let layout = responsive_layout(window.bounds().size.width);
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .h(px(30.))
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .bg(rgb(0x1b1d24))
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Loom")),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .relative()
                    .overflow_hidden()
                    .when(!layout.phone, |row| row.child(
                        div()
                            .w(layout.sidebar_width)
                            .h_full()
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
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0xf3f4f6))
                                            .child("Workspace"),
                                    ),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child("Sessions"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .p_3()
                                    .rounded_lg()
                                    .bg(rgb(0x111318))
                                    .border_1()
                                    .border_color(rgb(0x293244))
                                    .text_sm()
                                    .text_color(rgb(0x64748b))
                                    .child("Connect a worker to load sessions."),
                            )
                            .child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .border_t_1()
                                    .border_color(rgb(0x30343f))
                                    .pt_2()
                                    .child(
                                        Button::new("disconnected-settings")
                                            .icon(Icon::new(IconName::Settings))
                                            .ghost()
                                            .xsmall()
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                view.open_settings_from_menu(cx);
                                            })),
                                    ),
                            ),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .h_full()
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
                                            .child("No worker connected")
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(0x8f98a6))
                                                    .child("Connect a worker in Settings to begin."),
                                            ),
                                    )
                                    .child(
                                        Button::new("disconnected-open-settings")
                                            .icon(Icon::new(IconName::Settings))
                                            .ghost()
                                            .xsmall()
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                view.open_settings_from_menu(cx);
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .max_w(px(460.))
                                            .p_6()
                                            .rounded_lg()
                                            .bg(rgb(0x171c25))
                                            .border_1()
                                            .border_color(rgb(0x293244))
                                            .child(
                                                div()
                                                    .text_base()
                                                    .text_color(rgb(0xf3f4f6))
                                                    .child("Your workspace is ready"),
                                            )
                                            .child(
                                                div()
                                                    .mt_2()
                                                    .text_sm()
                                                    .text_color(rgb(0x8f98a6))
                                                    .child("Connect a Loom worker from Settings to load your sessions, models, and workspace."),
                                            )
                                            .child(
                                                Button::new("disconnected-connect-worker")
                                                    .label("Open Settings")
                                                    .small()
                                                    .on_click(cx.listener(
                                                        |view, _, _, cx| {
                                                            view.open_settings_from_menu(cx);
                                                        },
                                                    )),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .px_4()
                                    .py_3()
                                    .border_t_1()
                                    .border_color(rgb(0x30343f))
                                    .child(
                                        div()
                                            .p_3()
                                            .rounded_lg()
                                            .bg(rgb(0x171c25))
                                            .border_1()
                                            .border_color(rgb(0x293244))
                                            .text_sm()
                                            .text_color(rgb(0x64748b))
                                            .child("Connect a worker to start a session."),
                                    ),
                            )
                            .when(self.settings_open, |element| {
                                element.child(self.render_settings_dialog(cx))
                            }),
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
                    .text_xs()
                    .text_color(rgb(0x8f98a6))
                    .child("Not connected  ·  Connect a worker in Settings"),
            )
            .into_any()
    }
}

impl Render for LoomView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(target_family = "wasm")]
        if !self.connected {
            return self.render_disconnected(window, cx);
        }
        #[cfg(target_family = "wasm")]
        if !self.browser_window_initialized {
            self.browser_window_initialized = true;
            self.observe_system_appearance(window, cx);
            self.composer_focus_handle.focus(window, cx);
            self.select_theme(ThemeChoice::System, window, cx);
        }
        self.schedule_worker_node_poll(cx);
        self.sync_model_select_states(window, cx);
        self.sync_agent_mode_select_state(window, cx);
        self.schedule_run_poll(cx);
        let view = cx.entity();
        let layout = responsive_layout(window.bounds().size.width);
        let project_name = self
            .project
            .as_ref()
            .map(|project| project.name.as_str())
            .unwrap_or("Project");
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
                    .h(px(30.))
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
                            .child(div().text_xs().text_color(rgb(0x8f98a6)).child(format!(
                                "{}  ·  {}",
                                project_name, self.active_session.name
                            ))),
                    )
                    .when(!cfg!(target_family = "wasm"), |element| {
                        element.child(
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
                                    .text_color(rgb(0xb7c0d0))
                                    .hover(|style| {
                                        style.bg(rgb(0x7f1d1d)).text_color(rgb(0xffffff))
                                    })
                                    .cursor_pointer()
                                    .tooltip(|_, cx| {
                                        cx.new(|_| LoomTooltip {
                                            text: "Close window".into(),
                                        })
                                        .into()
                                    })
                                    .window_control_area(WindowControlArea::Close)
                                    .child(Icon::new(IconName::Close).size_4())
                                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    })
                                    .on_click(|_, window, cx| {
                                        cx.stop_propagation();
                                        window.remove_window();
                                    }),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .relative()
                    .overflow_hidden()
                    .when(!layout.phone, |row| {
                        row.child(self.render_session_sidebar(&view, layout, cx))
                    })
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
                                            .items_center()
                                            .gap_2()
                                            .when(layout.phone, |element| {
                                                element.child(
                                                    Button::new("open-session-drawer")
                                                        .label("Sessions")
                                                        .small()
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.session_drawer_open = true;
                                                            cx.notify();
                                                        })),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .flex_col()
                                                    .child(self.active_session.name.clone())
                                                    .when(!layout.phone, |element| {
                                                        element.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(rgb(0x8f98a6))
                                                                .child(format!(
                                                                    "{}  ·  {}  ·  {} model{}",
                                                                    run_state_label(self.run_state),
                                                                    self.model.as_str(),
                                                                    self.models.len(),
                                                                    if self.models.len() == 1 {
                                                                        ""
                                                                    } else {
                                                                        "s"
                                                                    }
                                                                )),
                                                        )
                                                    }),
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
                                                .label("Review")
                                                .icon(Icon::new(IconName::PanelRight))
                                                .ghost()
                                                .small()
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
                                    .overflow_hidden()
                                    .child(self.timeline_entity(cx)),
                            )
                            .child(self.render_composer(layout, cx))
                            .when(self.rename_dialog.is_some(), |element| {
                                element.child(self.render_rename_dialog(cx))
                            })
                            .when(self.settings_open, |element| {
                                element.child(self.render_settings_dialog(cx))
                            })
                            .when(self.about_open, |element| {
                                element.child(self.render_about_dialog(cx))
                            })
                            .when(
                                self.providers_open && self.github_login.is_none(),
                                |element| element.child(self.render_providers_dialog(cx)),
                            )
                            .when(self.github_login.is_some(), |element| {
                                element.child(self.render_github_login_dialog(cx))
                            }),
                    )
                    .when(layout.phone && self.session_drawer_open, |row| {
                        row.child(
                            div()
                                .id("mobile-session-backdrop")
                                .size_full()
                                .absolute()
                                .top(px(0.))
                                .left(px(0.))
                                .bg(gpui_kit::hsla(0., 0., 0., 0.55))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.session_drawer_open = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("mobile-session-drawer")
                                .absolute()
                                .top(px(0.))
                                .bottom(px(0.))
                                .left(px(0.))
                                .shadow_lg()
                                .child(self.render_session_sidebar(&view, layout, cx)),
                        )
                    })
                    .when(
                        self.review.open
                            && !self.settings_open
                            && !self.about_open
                            && !self.providers_open
                            && self.github_login.is_none(),
                        |element| element.child(self.render_review(window, cx)),
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
                        "{}  ·  {} updates  ·  {}",
                        session_state_label(self.session_state),
                        self.timeline.len(),
                        if self.demo_workspace {
                            "Demo workspace"
                        } else {
                            "Workspace"
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
                            element.border_1().border_color(rgb(0x30343f)).shadow(vec![
                                gpui_kit::BoxShadow {
                                    color: gpui_kit::hsla(0., 0., 0., 0.4),
                                    blur_radius: shadow_size / 2.,
                                    spread_radius: px(0.),
                                    offset: point(px(0.), px(0.)),
                                    inset: false,
                                },
                            ])
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
        .into_any()
    }
}

#[cfg(test)]
mod responsive_layout_tests {
    use super::{
        COMPACT_REVIEW_WIDTH, COMPACT_SIDEBAR_WIDTH, FULL_REVIEW_WIDTH, FULL_SIDEBAR_WIDTH,
        PHONE_SIDEBAR_WIDTH, responsive_layout,
    };
    use gpui_kit::px;

    #[test]
    fn compact_windows_use_narrower_navigation_panels() {
        let layout = responsive_layout(px(959.));
        assert_eq!(layout.sidebar_width, COMPACT_SIDEBAR_WIDTH);
        assert_eq!(layout.review_width, COMPACT_REVIEW_WIDTH);
    }

    #[test]
    fn wide_windows_keep_full_navigation_panels() {
        let layout = responsive_layout(px(960.));
        assert!(!layout.phone);
        assert_eq!(layout.sidebar_width, FULL_SIDEBAR_WIDTH);
        assert_eq!(layout.review_width, FULL_REVIEW_WIDTH);
    }

    #[test]
    fn phone_windows_show_single_column_and_full_width_review() {
        let layout = responsive_layout(px(390.));
        assert!(layout.phone);
        assert_eq!(layout.sidebar_width, PHONE_SIDEBAR_WIDTH);
        assert_eq!(layout.review_width, px(390.));
    }

    #[test]
    fn very_narrow_phones_keep_the_session_drawer_in_view() {
        let layout = responsive_layout(px(280.));
        assert!(layout.phone);
        assert_eq!(layout.sidebar_width, px(280.));
        assert_eq!(layout.review_width, px(280.));
    }
}

#[cfg(test)]
mod worker_node_tests {
    use super::{
        ACTIVE_BACKEND_NODE_ENTRY_ID, SessionNodeIndicatorState, WorkerConnectionStage,
        WorkerConnectionState, WorkerNodeEntry, adjusted_cpu_pulse_threshold, assigned_node_id,
        format_percentage, format_session_resource_percentages, format_worker_node_resources,
        mark_worker_connection_failed, merge_node_sessions, next_severe_load_streak,
        order_session_nodes, remove_worker_node_entry, safe_worker_url_label,
        session_id_for_request, session_node_indicator_state, session_node_pulse,
        session_owner_status, transition_worker_connection_to_connecting,
        update_worker_node_status, validate_model_for_node, worker_connection_failure_detail,
        worker_node_display_name, worker_node_name_for_id, worker_url_embeds_credential,
    };
    use loom_core::{
        AgentSessionId, AgentSessionSnapshot, AgentSessionState, CapabilitySet, ProjectId, RunId,
        Timestamp,
    };
    use loom_core::{ErrorCode, LoomError};
    use loom_model::ModelId;
    use loom_protocol::{ClientRequest, WorkerNodeResources, WorkerNodeStatus};
    use std::collections::BTreeMap;

    fn node(id: u64, is_local: bool) -> WorkerNodeEntry {
        WorkerNodeEntry {
            id,
            status: WorkerNodeStatus {
                node_id: format!("node-{id}"),
                name: format!("Node {id}"),
                online: true,
                capabilities: CapabilitySet::default(),
                resources: WorkerNodeResources {
                    cpu_count: 0,
                    cpu_usage_percent: None,
                    memory_usage_percent: None,
                    memory_total_bytes: None,
                    memory_available_bytes: None,
                    disk_total_bytes: None,
                    disk_available_bytes: None,
                },
            },
            is_local,
            url: (!is_local).then(|| format!("ws://worker-{id}/ws")),
            connection: None,
            connection_state: if is_local {
                WorkerConnectionState::Connected
            } else {
                WorkerConnectionState::Disconnected
            },
            connection_detail: None,
            severe_load_streak: 0,
        }
    }

    #[test]
    fn connection_error_details_are_actionable_and_do_not_echo_tokens() {
        let invalid_url = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::invalid_request("invalid url with secret-token"),
            Some("secret-token"),
        );
        assert!(invalid_url.contains("Invalid worker URL"));
        assert!(!invalid_url.contains("secret-token"));

        let refused = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::new(ErrorCode::Internal, "connection refused", true),
            None,
        );
        assert!(refused.contains("server is running"));

        let timeout = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::new(ErrorCode::DeadlineExceeded, "timeout", true),
            None,
        );
        assert!(timeout.contains("timed out"));

        let authentication = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::new(ErrorCode::AuthenticationFailed, "denied", false),
            None,
        );
        assert!(authentication.contains("access token"));

        let negotiation = worker_connection_failure_detail(
            WorkerConnectionStage::Negotiation,
            &LoomError::new(ErrorCode::UnsupportedProtocol, "mismatch", false),
            None,
        );
        assert!(negotiation.contains("protocol negotiation failed"));

        let status = worker_connection_failure_detail(
            WorkerConnectionStage::Status,
            &LoomError::new(ErrorCode::Internal, "bad status", false),
            None,
        );
        assert!(status.contains("status request failed"));

        let credential = worker_connection_failure_detail(
            WorkerConnectionStage::CredentialSave,
            &LoomError::new(
                ErrorCode::Persistence,
                "could not persist secret%2Fvalue",
                false,
            ),
            Some("secret/value"),
        );
        assert!(credential.contains("could not save reconnect credentials"));
        assert!(!credential.contains("secret/value"));
        assert!(!credential.contains("secret%2Fvalue"));

        let credential_read = worker_connection_failure_detail(
            WorkerConnectionStage::CredentialRead,
            &LoomError::new(ErrorCode::AuthenticationRequired, "missing", false),
            None,
        );
        assert!(credential_read.contains("OS credential store"));

        let bootstrap_save = worker_connection_failure_detail(
            WorkerConnectionStage::BootstrapSave,
            &LoomError::new(ErrorCode::Persistence, "failed with secret", false),
            Some("secret"),
        );
        assert!(bootstrap_save.contains("browser could not save"));
        assert!(!bootstrap_save.contains("secret"));
    }

    #[test]
    fn duplicate_connection_attempts_are_blocked_and_url_labels_hide_credentials() {
        let mut state = WorkerConnectionState::Connecting;
        assert!(transition_worker_connection_to_connecting(&mut state, false).is_err());
        assert_eq!(state, WorkerConnectionState::Connecting);

        state = WorkerConnectionState::Failed;
        assert!(transition_worker_connection_to_connecting(&mut state, false).is_ok());
        assert_eq!(state, WorkerConnectionState::Connecting);

        state = WorkerConnectionState::Connected;
        assert!(transition_worker_connection_to_connecting(&mut state, true).is_err());
        assert_eq!(state, WorkerConnectionState::Connected);

        assert!(worker_url_embeds_credential(
            "wss://user:password@worker.example/ws?access_token=sample"
        ));
        assert_eq!(
            safe_worker_url_label(
                "wss://user:password@worker.example/ws?access_token=sample&keep=hidden"
            ),
            "wss://worker.example/ws"
        );
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn failed_connection_state_clears_transport_and_keeps_node_url() {
        let mut node = node(7, false);
        let backend = loom_server::InProcessBackend::new();
        node.connection = Some(super::ClientConnection::InProcess(backend.connect()));
        node.status.online = true;
        node.connection_state = WorkerConnectionState::Connected;

        let cleanup_failed =
            mark_worker_connection_failed(&mut node, "protocol negotiation failed".to_owned());

        assert!(!cleanup_failed);
        assert!(node.connection.is_none());
        assert_eq!(node.connection_state, WorkerConnectionState::Failed);
        assert!(!node.status.online);
        assert_eq!(node.url.as_deref(), Some("ws://worker-7/ws"));
        assert_eq!(
            node.connection_detail.as_deref(),
            Some("protocol negotiation failed")
        );
    }

    fn session(id: AgentSessionId, name: &str) -> AgentSessionSnapshot {
        AgentSessionSnapshot {
            id,
            project_id: ProjectId::new(),
            name: name.to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(1),
        }
    }

    #[test]
    fn local_worker_node_cannot_be_removed() {
        let mut nodes = vec![node(0, true)];

        assert!(remove_worker_node_entry(&mut nodes, 0).is_none());
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].is_local());
    }

    #[test]
    fn configured_worker_node_can_be_removed_without_removing_local_node() {
        let mut nodes = vec![node(0, true), node(1, false)];

        let removed = remove_worker_node_entry(&mut nodes, 1).unwrap();

        assert_eq!(removed.id, 1);
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].is_local());
    }

    #[test]
    fn unavailable_and_available_resource_percentages_are_formatted() {
        assert_eq!(format_percentage(None), "n/a");
        assert_eq!(format_percentage(Some(0)), "0%");
        assert_eq!(format_percentage(Some(73)), "73%");
        assert_eq!(format_percentage(Some(101)), "n/a");
    }

    #[test]
    fn resource_summary_keeps_cpu_cores_and_total_ram_across_samples() {
        let initial = WorkerNodeResources {
            cpu_count: 8,
            cpu_usage_percent: None,
            memory_usage_percent: None,
            memory_total_bytes: Some(16 << 30),
            memory_available_bytes: Some(8 << 30),
            disk_total_bytes: Some(1 << 30),
            disk_available_bytes: Some(512 << 20),
        };
        let initial_summary = format_worker_node_resources(&initial);
        assert!(initial_summary.contains("CPU n/a of 8 cores"));
        assert!(initial_summary.contains("RAM n/a of 16.0 GiB"));
        assert!(initial_summary.contains("disk 512.0 MiB available"));

        let updated = WorkerNodeResources {
            cpu_usage_percent: Some(31),
            memory_usage_percent: Some(50),
            ..initial
        };
        let updated_summary = format_worker_node_resources(&updated);
        assert!(updated_summary.contains("CPU 31% of 8 cores"));
        assert!(updated_summary.contains("RAM 50% of 16.0 GiB"));
        assert!(updated_summary.contains("disk 512.0 MiB available"));
    }

    #[test]
    fn refreshed_worker_status_replaces_initial_unavailable_percentages() {
        let mut nodes = vec![node(1, false)];
        nodes[0].status.resources = WorkerNodeResources {
            cpu_count: 4,
            cpu_usage_percent: None,
            memory_usage_percent: None,
            memory_total_bytes: Some(8 << 30),
            memory_available_bytes: Some(4 << 30),
            disk_total_bytes: Some(100 << 30),
            disk_available_bytes: Some(50 << 30),
        };
        assert!(
            format_worker_node_resources(&nodes[0].status.resources)
                .contains("CPU n/a of 4 cores · RAM n/a of 8.0 GiB")
        );

        let mut refreshed = nodes[0].status.clone();
        refreshed.resources.cpu_usage_percent = Some(25);
        refreshed.resources.memory_usage_percent = Some(50);
        assert_eq!(update_worker_node_status(&mut nodes, 1, refreshed), None);

        let summary = format_worker_node_resources(&nodes[0].status.resources);
        assert!(summary.contains("CPU 25% of 4 cores · RAM 50% of 8.0 GiB"));
        assert!(summary.contains("disk 50.0 GiB available"));
    }

    #[test]
    fn session_status_uses_the_assigned_node_not_the_active_backend() {
        let mut active_backend = node(ACTIVE_BACKEND_NODE_ENTRY_ID, true);
        active_backend.status.resources.cpu_usage_percent = Some(22);
        active_backend.status.resources.memory_usage_percent = Some(38);

        let mut peer = node(1, false);
        peer.status.resources.cpu_usage_percent = Some(99);
        peer.status.resources.memory_usage_percent = Some(97);
        let nodes = vec![active_backend, peer];
        let session_id = AgentSessionId::new();
        let owners = BTreeMap::from([(session_id, "node-1".to_owned())]);

        let owner = session_owner_status(&nodes, &owners, session_id).unwrap();
        assert_eq!(worker_node_display_name(owner), "External worker · Node 1");
        assert_eq!(
            format_session_resource_percentages(Some(&owner.status)),
            "CPU 99% · RAM 97%"
        );
        let node_names =
            BTreeMap::from([("node-1".to_owned(), "External worker · Node 1".to_owned())]);
        assert_eq!(
            worker_node_name_for_id(&nodes[..1], &node_names, Some("node-1")),
            "External worker · Node 1"
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::GetAgentSessionSnapshot { session_id },
                AgentSessionId::new()
            ),
            Some(session_id)
        );
    }

    #[test]
    fn run_requests_route_to_the_active_session_owner() {
        let active_session_id = AgentSessionId::new();
        let owners = BTreeMap::from([(active_session_id, "peer-node".to_owned())]);
        assert_eq!(
            session_id_for_request(
                &ClientRequest::SendAgentMessage {
                    run_id: RunId::new(),
                    message: "hello".to_owned(),
                },
                active_session_id
            ),
            Some(active_session_id)
        );
        assert_eq!(
            assigned_node_id(&owners, active_session_id),
            Ok("peer-node")
        );
        assert!(assigned_node_id(&BTreeMap::new(), active_session_id).is_err());
        assert_eq!(
            session_id_for_request(&ClientRequest::ListProjects, active_session_id),
            None
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::CreateCheckpoint {
                    project_id: ProjectId::new(),
                    session_id: Some(active_session_id),
                    label: "checkpoint".to_owned(),
                },
                AgentSessionId::new()
            ),
            Some(active_session_id)
        );
    }

    #[test]
    fn session_aggregation_tracks_node_owners_and_preserves_disconnected_sessions() {
        let primary_id = AgentSessionId::new();
        let peer_id = AgentSessionId::new();
        let disconnected_id = AgentSessionId::new();
        let primary_session = session(primary_id, "Primary");
        let peer_session = session(peer_id, "Peer");
        let disconnected_session = session(disconnected_id, "Disconnected");
        let current = vec![primary_session.clone(), disconnected_session.clone()];
        let owners = BTreeMap::from([
            (primary_id, "node-0".to_owned()),
            (disconnected_id, "removed-node".to_owned()),
        ]);

        let (sessions, owners) = merge_node_sessions(
            &current,
            &owners,
            vec![
                ("node-0".to_owned(), vec![primary_session]),
                ("node-1".to_owned(), vec![peer_session]),
            ],
        );

        assert_eq!(sessions.len(), 3);
        assert_eq!(owners.get(&primary_id).map(String::as_str), Some("node-0"));
        assert_eq!(owners.get(&peer_id).map(String::as_str), Some("node-1"));
        assert_eq!(
            owners.get(&disconnected_id).map(String::as_str),
            Some("removed-node")
        );
    }

    #[test]
    fn session_aggregation_rebinds_returned_sessions_to_a_restarted_node_identity() {
        let session_id = AgentSessionId::new();
        let current = vec![session(session_id, "Existing")];
        let current_owners = BTreeMap::from([(session_id, "old-node-id".to_owned())]);

        let (sessions, owners) = merge_node_sessions(
            &current,
            &current_owners,
            vec![(
                "new-node-id".to_owned(),
                vec![session(session_id, "Existing")],
            )],
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            owners.get(&session_id).map(String::as_str),
            Some("new-node-id")
        );
    }

    #[test]
    fn new_session_node_choices_keep_the_default_backend_first() {
        let nodes = vec![
            ("peer".to_owned(), "External worker".to_owned()),
            ("default".to_owned(), "Local backend".to_owned()),
        ];
        let ordered = order_session_nodes(nodes, "default");

        assert_eq!(ordered[0].0, "default");
        assert_eq!(ordered[1].0, "peer");
    }

    #[test]
    fn model_selection_uses_the_chosen_workers_catalog() {
        let local_model = ModelId::new("local/provider-model");
        let worker_model = ModelId::new("worker/provider-model");
        let catalogs = BTreeMap::from([
            ("local".to_owned(), vec![local_model.clone()]),
            ("worker".to_owned(), vec![worker_model.clone()]),
        ]);

        assert!(validate_model_for_node(&catalogs, "local", &local_model).is_ok());
        assert!(validate_model_for_node(&catalogs, "worker", &local_model).is_err());
        assert!(validate_model_for_node(&catalogs, "worker", &worker_model).is_ok());
    }

    #[test]
    fn assigned_node_status_drives_dot_pulse_speed_and_intensity() {
        let mut low_load = node(1, false).status;
        low_load.resources.cpu_usage_percent = Some(6);
        low_load.resources.memory_usage_percent = Some(10);
        let mut high_load = low_load.clone();
        high_load.resources.cpu_usage_percent = Some(80);
        high_load.resources.memory_usage_percent = Some(60);
        let unknown_load = node(2, false).status;

        let (low_period, low_amplitude) = session_node_pulse(Some(&low_load), 5).unwrap();
        let (high_period, high_amplitude) = session_node_pulse(Some(&high_load), 5).unwrap();

        assert!(high_period < low_period);
        assert!(high_amplitude > low_amplitude);
        assert_eq!(session_node_pulse(Some(&unknown_load), 5), None);
        assert_eq!(session_node_pulse(None, 5), None);
        assert_eq!(session_node_pulse(Some(&low_load), 6), None);
        high_load.online = false;
        assert_eq!(session_node_pulse(Some(&high_load), 5), None);
    }

    #[test]
    fn pulse_threshold_adjustment_is_bounded_and_uses_five_percent_by_default() {
        assert_eq!(
            loom_protocol::WorkspaceConfig::default().cpu_pulse_threshold_percent,
            5
        );
        assert_eq!(adjusted_cpu_pulse_threshold(5, -1), 4);
        assert_eq!(adjusted_cpu_pulse_threshold(0, -1), 0);
        assert_eq!(adjusted_cpu_pulse_threshold(100, 1), 100);
        assert_eq!(adjusted_cpu_pulse_threshold(99, 1), 100);
    }

    #[test]
    fn severe_load_red_requires_three_consecutive_dual_threshold_samples() {
        let mut status = node(1, false).status;
        status.resources.cpu_usage_percent = Some(91);
        status.resources.memory_usage_percent = Some(91);

        let first = next_severe_load_streak(0, &status.resources);
        let second = next_severe_load_streak(first, &status.resources);
        assert_eq!(
            session_node_indicator_state(Some(&status), second),
            SessionNodeIndicatorState::Online
        );
        let third = next_severe_load_streak(second, &status.resources);
        assert_eq!(third, 3);
        assert_eq!(
            session_node_indicator_state(Some(&status), third),
            SessionNodeIndicatorState::Severe
        );

        status.resources.cpu_usage_percent = Some(90);
        assert_eq!(next_severe_load_streak(third, &status.resources), 0);
        status.resources.cpu_usage_percent = Some(91);
        status.resources.memory_usage_percent = None;
        assert_eq!(next_severe_load_streak(third, &status.resources), 0);
        status.resources.memory_usage_percent = Some(91);
        status.online = false;
        assert_eq!(
            session_node_indicator_state(Some(&status), third),
            SessionNodeIndicatorState::Offline
        );
    }

    #[test]
    fn active_backend_and_external_worker_labels_do_not_collide() {
        let mut active_backend = node(ACTIVE_BACKEND_NODE_ENTRY_ID, true);
        active_backend.status.name = "local".to_owned();
        let mut external_worker = node(1, false);
        external_worker.status.name = "local".to_owned();

        assert_eq!(
            worker_node_display_name(&active_backend),
            "Local backend · local"
        );
        assert_eq!(
            worker_node_display_name(&external_worker),
            "External worker · local"
        );
    }
}
