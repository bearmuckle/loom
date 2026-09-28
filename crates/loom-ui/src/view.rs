//! The GPUI view: session navigator, run canvas, composer, and review drawer.

#[cfg(not(target_family = "wasm"))]
use std::sync::Arc;
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
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{
    Icon, IconName, IndexPath, Sizable,
    collapsible::Collapsible,
    h_resizable,
    menu::{DropdownMenu, PopupMenu, PopupMenuItem},
    resizable_panel,
    select::{SearchableVec, Select, SelectEvent, SelectState},
    text::TextView,
    tree::{Tree as KitTree, TreeItem, TreeState},
};
use gpui_kit::{
    Animation, AnimationExt, App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle,
    Decorations, Element, Entity, FocusHandle, Focusable, HighlightStyle, HitboxBehavior,
    ListAlignment, ListState, MouseButton, Pixels, Render, ResizeEdge, Subscription, Tiling,
    Window, WindowAppearance, WindowControlArea, canvas, div, list, point, prelude::*, px,
    transparent_black,
};
use loom_core::{
    ActivityId, AgentMessageRecord, AgentSessionId, AgentSessionSnapshot, AgentSessionState,
    CapabilitySet, ErrorCode, EventSequence, LoomError, RepositoryId, RunId, WorkspaceId,
    WorkspaceRecord,
};
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
use loom_providers::{GITHUB_COPILOT_DEFAULT_MODEL, GitHubCopilotAuthenticator, GitHubDeviceCode};
#[cfg(not(target_family = "wasm"))]
use loom_server::InProcessBackend;
#[cfg(not(target_family = "wasm"))]
use std::fs;

use crate::{
    MAX_REVIEW_CHANGES, MAX_REVIEW_DIFF,
    connection::{BackendWorker, ClientConnection, ConnectionCleanupGuard},
    state::{
        AgentMode, GitHubLoginState, RenameDialogState, ReviewPanel, ReviewRow, ReviewState,
        ThemeChoice, TimelineItem, activity_status_label, bounded, bounded_to,
        session_state_for_run, session_title_from_task, upsert_activity,
    },
    theme::{
        CLIENT_DECORATION_SHADOW, ClientCorners, ERROR_CARD_ACCENT, ERROR_CARD_FOREGROUND,
        ERROR_CARD_SURFACE, change_color, resize_edge, rgb,
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
            review_width: (width - COMPACT_SIDEBAR_WIDTH - px(220.))
                .max(px(300.))
                .min(COMPACT_REVIEW_WIDTH),
        }
    } else {
        ResponsiveLayout {
            phone: false,
            sidebar_width: FULL_SIDEBAR_WIDTH,
            review_width: (width - FULL_SIDEBAR_WIDTH - px(280.))
                .max(px(360.))
                .min(FULL_REVIEW_WIDTH),
        }
    }
}

fn review_panel_is_visible(
    layout: ResponsiveLayout,
    review_open: bool,
    session_count: usize,
    settings_open: bool,
    about_open: bool,
    providers_open: bool,
    github_login_open: bool,
) -> bool {
    !layout.phone
        && review_open
        && session_count > 0
        && !settings_open
        && !about_open
        && !providers_open
        && !github_login_open
}

fn session_header_title() -> gpui_kit::Div {
    div().flex().flex_1().min_w(px(0.)).items_center().gap_2()
}

fn session_header_actions() -> gpui_kit::Div {
    div().flex().flex_shrink_0().items_center().gap_1()
}

fn empty_session_snapshot(workspace_id: WorkspaceId) -> AgentSessionSnapshot {
    let now = loom_core::Timestamp::now();
    AgentSessionSnapshot {
        id: AgentSessionId::new(),
        workspace_id,
        name: "No session selected".to_owned(),
        state: AgentSessionState::Idle,
        created_at: now,
        updated_at: now,
    }
}

fn header_tooltip(
    id: &'static str,
    text: &'static str,
    child: impl IntoElement,
) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .tooltip(move |_, cx| cx.new(|_| LoomTooltip { text: text.into() }).into())
        .child(child)
}

#[cfg(target_family = "wasm")]
use crate::{
    browser::BrowserOptions,
    connection::{
        list_models_async, negotiate_async, set_workspace_config_async, worker_node_status_async,
        workspace_config_async,
    },
};
#[cfg(not(target_family = "wasm"))]
use crate::{
    connection::{
        list_models, list_provider_ids, negotiate, set_workspace_config, start_run,
        worker_node_status, workspace_config,
    },
    platform::{PeerCredentialStore, UiOptions, backend_persistence_path, prepare_workspace},
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

fn adjusted_project_agent_concurrency(current: u8, delta: i8) -> u8 {
    (i16::from(current) + i16::from(delta)).clamp(
        i16::from(loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY),
        i16::from(loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY),
    ) as u8
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

#[cfg(test)]
fn order_session_nodes(
    mut nodes: Vec<(String, String)>,
    default_node_id: &str,
) -> Vec<(String, String)> {
    nodes.sort_by_key(|(node_id, _)| node_id != default_node_id);
    nodes
}

async fn load_transcript_page(
    backend: BackendWorker,
    run_id: RunId,
    before_ordinal: Option<u64>,
) -> Result<(Vec<ModelMessage>, Option<u64>, bool), LoomError> {
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
            .map(|message| message.message)
            .collect(),
        next_before,
        has_older,
    ))
}

#[cfg(not(target_family = "wasm"))]
fn load_transcript_page_sync(
    connection: &ClientConnection,
    run_id: RunId,
    before_ordinal: Option<u64>,
) -> Result<(Vec<ModelMessage>, Option<u64>, bool), LoomError> {
    let response = connection.request(RequestEnvelope::new(
        ClientRequest::GetAgentRunTranscriptPage {
            run_id,
            before_ordinal,
            limit: MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE,
        },
    ));
    let (messages, next_before, has_older) = match response.result? {
        ServerResponse::AgentRunTranscriptPage {
            run_id: response_run_id,
            messages,
            next_before,
            has_older,
        } if response_run_id == run_id => (messages, next_before, has_older),
        response => return Err(unexpected_response("run transcript page", response)),
    };
    Ok((
        messages
            .into_iter()
            .map(|message| message.message)
            .collect(),
        next_before,
        has_older,
    ))
}

fn timeline_items_from_messages(
    messages: Vec<ModelMessage>,
    has_activity_records: bool,
) -> Vec<TimelineItem> {
    let mut timeline = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::User => timeline.push(TimelineItem::User(message.content)),
            MessageRole::Assistant => {
                if message.content.is_empty() {
                    continue;
                }
                if let Some(TimelineItem::Assistant(previous)) = timeline.last_mut() {
                    if !previous.is_empty() {
                        previous.push_str("\n\n");
                    }
                    previous.push_str(&message.content);
                } else {
                    timeline.push(TimelineItem::Assistant(message.content));
                }
            }
            MessageRole::Tool if !has_activity_records => {
                timeline.push(TimelineItem::ToolOutput(bounded(&message.content)))
            }
            MessageRole::Tool | MessageRole::System => {}
        }
    }
    timeline
}

fn prepend_timeline_page(
    timeline: &mut Vec<TimelineItem>,
    mut older_items: Vec<TimelineItem>,
    insertion_index: usize,
) {
    let boundary_assistant = matches!(older_items.last(), Some(TimelineItem::Assistant(_)))
        && matches!(
            timeline.get(insertion_index),
            Some(TimelineItem::Assistant(_))
        );
    if boundary_assistant {
        let Some(TimelineItem::Assistant(older)) = older_items.pop() else {
            unreachable!()
        };
        let Some(TimelineItem::Assistant(newer)) = timeline.get_mut(insertion_index) else {
            unreachable!()
        };
        let separator = if older.is_empty() || newer.is_empty() {
            ""
        } else {
            "\n\n"
        };
        *newer = format!("{older}{separator}{newer}");
    }
    timeline.splice(insertion_index..insertion_index, older_items);
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
        ClientRequest::GetAgentSession { session_id }
        | ClientRequest::GetAgentSessionSnapshot { session_id }
        | ClientRequest::GetAgentSessionSnapshotMetadata { session_id }
        | ClientRequest::GetAgentSessionInitialState { session_id }
        | ClientRequest::RenameAgentSession { session_id, .. }
        | ClientRequest::ArchiveAgentSession { session_id }
        | ClientRequest::GetRecentSessionEvents { session_id, .. }
        | ClientRequest::StartSessionAgentRun { session_id, .. }
        | ClientRequest::StartSessionAgentRunWithOptions { session_id, .. }
        | ClientRequest::AttachSessionRepository { session_id, .. }
        | ClientRequest::AttachSessionDirectory { session_id, .. }
        | ClientRequest::ListSessionDirectories { session_id }
        | ClientRequest::DetachSessionDirectory { session_id, .. }
        | ClientRequest::ListSessionRepositories { session_id }
        | ClientRequest::DetachSessionRepository { session_id, .. }
        | ClientRequest::GetSessionFilesystemSnapshot { session_id }
        | ClientRequest::GetSessionFilesystemChanges { session_id, .. }
        | ClientRequest::ReadSessionFile { session_id, .. }
        | ClientRequest::ApplySessionFilesystemEdit { session_id, .. }
        | ClientRequest::TakeSessionFilesystemControl { session_id, .. }
        | ClientRequest::CreateSessionCheckpoint { session_id, .. }
        | ClientRequest::RevertSessionCheckpoint { session_id, .. }
        | ClientRequest::UndoSessionEdit { session_id }
        | ClientRequest::GetSessionContextFiles { session_id }
        | ClientRequest::GetSessionVcsStatus { session_id, .. }
        | ClientRequest::GetSessionVcsDiff { session_id, .. }
        | ClientRequest::GetSessionVcsBranches { session_id, .. }
        | ClientRequest::GetSessionVcsConflicts { session_id, .. }
        | ClientRequest::OpenSessionTerminal { session_id, .. }
        | ClientRequest::WriteSessionTerminalInput { session_id, .. }
        | ClientRequest::ResizeSessionTerminal { session_id, .. }
        | ClientRequest::GetSessionTerminalEvents { session_id, .. }
        | ClientRequest::CancelSessionTerminal { session_id, .. }
        | ClientRequest::StartSessionTask { session_id, .. }
        | ClientRequest::ListSessionTasks { session_id }
        | ClientRequest::GetSessionTask { session_id, .. }
        | ClientRequest::GetSessionTaskEvents { session_id, .. }
        | ClientRequest::CancelSessionTask { session_id, .. }
        | ClientRequest::GetSessionTaskEvidence { session_id, .. }
        | ClientRequest::SetSessionApprovalPolicy { session_id, .. }
        | ClientRequest::ForkAgentSession { session_id, .. }
        | ClientRequest::GetSessionUsage { session_id } => Some(*session_id),
        ClientRequest::ControlProjectChild {
            manager_session_id, ..
        }
        | ClientRequest::GetProjectChildReview {
            manager_session_id, ..
        }
        | ClientRequest::IntegrateProjectChild {
            manager_session_id, ..
        }
        | ClientRequest::CleanupProjectChildWorktree {
            manager_session_id, ..
        } => Some(*manager_session_id),
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

fn model_choice_labels(
    models: &[ModelId],
    provider_names: Option<&BTreeMap<ModelId, String>>,
) -> BTreeMap<String, ModelId> {
    models
        .iter()
        .map(|model| {
            let provider = provider_names
                .and_then(|names| names.get(model))
                .map(String::as_str)
                .unwrap_or("Provider");
            (format!("{provider} · {}", model.as_str()), model.clone())
        })
        .collect()
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

fn belongs_to_repository(path: &str, repositories: &[SessionRepository]) -> bool {
    repositories.iter().any(|repository| {
        path == repository.path || path.starts_with(&format!("{}/", repository.path))
    })
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
        // TextView owns the copy/select-all key handlers as well as the
        // selection participant. SelectableText alone only paints and tracks
        // the selection; it does not install clipboard actions.
        TextView::markdown(id, text)
            .selectable(true)
            .w_full()
            .text_size(gpui_kit::rems(CONVERSATION_FONT_SIZE / BASE_FONT_SIZE))
            .text_color(rgb(color))
            .into_any()
    } else {
        TextView::markdown(id, text)
            .style(
                gpui_kit::component::text::TextViewStyle::default().inline_code(HighlightStyle {
                    background_color: Some(rgb(0x1b1d24).into()),
                    ..Default::default()
                }),
            )
            .selectable(true)
            .w_full()
            .text_size(gpui_kit::rems(CONVERSATION_FONT_SIZE / BASE_FONT_SIZE))
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
            command, args, cwd, ..
        } => (
            compact_activity_text(&command_line(command, args), 180),
            cwd.as_ref().map(|cwd| format!("Directory: {cwd}")),
        ),
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

fn activity_turn_title(activities: &[AgentActivityRecord]) -> String {
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
        "Making changes".to_owned()
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::Command { .. }))
    {
        command_group_title(activities)
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::Search { .. }))
    {
        "Searching the codebase".to_owned()
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::File { .. }))
    {
        "Inspecting the workspace".to_owned()
    } else if activities
        .iter()
        .any(|activity| matches!(&activity.data, AgentActivityData::ToolCall { .. }))
    {
        "Using tools".to_owned()
    } else {
        "Working on the task".to_owned()
    }
}

fn compact_activity_text(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let mut label = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        label.push('…');
    }
    label
}

fn command_line(command: &str, args: &[String]) -> String {
    std::iter::once(command.to_owned())
        .chain(args.iter().map(|arg| {
            if !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_alphanumeric() || "-_/.:=".contains(c))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        }))
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_purpose(command: &str, args: &[String]) -> String {
    let executable = command.rsplit(['/', '\\']).next().unwrap_or(command);
    // Shell wrappers are common; use their script to recognize familiar work.
    if matches!(executable, "sh" | "bash" | "zsh")
        && let Some(script) = args
            .windows(2)
            .find_map(|pair| matches!(pair[0].as_str(), "-c" | "-lc").then_some(pair[1].as_str()))
    {
        let mut words = script.split_whitespace();
        if let Some(program) = words.next() {
            return command_purpose(program, &words.map(str::to_owned).collect::<Vec<_>>());
        }
    }
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str);
    match (executable, action) {
        ("cargo", Some("test" | "llvm-cov")) => "Run tests".to_owned(),
        ("cargo", Some("clippy")) => "Check code quality".to_owned(),
        ("cargo", Some("fmt")) if args.iter().any(|arg| arg == "--check") => {
            "Check formatting".to_owned()
        }
        ("cargo", Some("fmt")) => "Format code".to_owned(),
        ("cargo", Some("build" | "check")) => "Check the build".to_owned(),
        ("npm" | "pnpm" | "yarn", Some("test")) | ("pytest", _) => "Run tests".to_owned(),
        ("git", Some("status" | "diff" | "log" | "show")) => {
            "Inspect repository changes".to_owned()
        }
        ("rg" | "grep" | "find", _) => "Search the workspace".to_owned(),
        ("ls" | "cat" | "sed" | "head" | "tail" | "pwd", _) => "Inspect workspace files".to_owned(),
        _ => {
            let program = if executable.is_empty() {
                "command"
            } else {
                executable
            };
            let detail = action.map_or_else(String::new, |action| format!(" {action}"));
            format!(
                "Run {}",
                compact_activity_text(&format!("{program}{detail}"), 40)
            )
        }
    }
}

fn command_group_title(activities: &[AgentActivityRecord]) -> String {
    let mut purposes = Vec::new();
    for activity in activities {
        if let AgentActivityData::Command { command, args, .. } = &activity.data {
            let purpose = command_purpose(command, args);
            if !purposes.contains(&purpose) {
                purposes.push(purpose);
            }
        }
    }
    let title = match purposes.as_slice() {
        [] => "Inspect the workspace".to_owned(),
        [purpose] => purpose.clone(),
        [first, second] => format!("{first} · {second}"),
        [first, second, ..] => format!("{first} · {second} · More work"),
    };
    compact_activity_text(&title, 72)
}

fn activity_group_status(activities: &[AgentActivityRecord]) -> AgentActivityStatus {
    // Actionable and active work stays visible even after another activity fails.
    for status in [
        AgentActivityStatus::AwaitingApproval,
        AgentActivityStatus::AwaitingInput,
        AgentActivityStatus::Started,
        AgentActivityStatus::Failed,
        AgentActivityStatus::Cancelled,
    ] {
        if activities.iter().any(|activity| activity.status == status) {
            return status;
        }
    }
    AgentActivityStatus::Completed
}

fn command_output_summary(output: &str) -> String {
    let lines = output.lines().collect::<Vec<_>>();
    if lines.len() <= 8 && output.len() <= 420 {
        return output.trim().to_owned();
    }
    let head = lines.iter().take(3).copied().collect::<Vec<_>>().join("\n");
    let tail = lines
        .iter()
        .skip(lines.len().saturating_sub(4))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    // Keep the end, where build/test summaries and failure messages usually live.
    let tail_start = tail
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| tail.len() - index <= 240)
        .unwrap_or(tail.len());
    format!(
        "{}\n… output abbreviated …\n{}",
        bounded_to(&head, 140),
        &tail[tail_start..]
    )
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
    #[cfg(target_family = "wasm")]
    browser_demo_mode: bool,
    /// Used for the synchronous bootstrap before the window exists.
    pub(crate) connection: ClientConnection,
    #[cfg(not(target_family = "wasm"))]
    owned_backend: Option<Arc<InProcessBackend>>,
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
    transcript_has_older: bool,
    transcript_loading: bool,
    transcript_generation: u64,
    timeline_view: Option<Entity<TimelineView>>,
    pub(crate) activity_records_seen: bool,
    pub(crate) expanded_activities: BTreeSet<ActivityId>,
    expanded_activity_groups: BTreeSet<ActivityId>,
    pub(crate) approval_request_in_flight: bool,
    approval_settings_request_in_flight: bool,
    pub(crate) archive_request_in_flight: bool,
    pub(crate) pending_approval: Option<ToolCall>,
    pub(crate) pending_input: Option<String>,
    composer_input: Option<Entity<TextareaState>>,
    composer_placeholder: Option<String>,
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
enum SessionSourceDialogPurpose {
    StartSession,
    AddToSession,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionSourceChoice {
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

fn project_session_list_projection(
    sessions: &[AgentSessionSnapshot],
    active_session_id: AgentSessionId,
    project: Option<&loom_core::ProjectSnapshot>,
) -> SessionListProjection {
    let Some(project) = project else {
        return session_list_projection(sessions, active_session_id);
    };
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

    fn agent_label(
        session: &AgentSessionSnapshot,
        agent: &loom_core::ProjectAgentRecord,
    ) -> String {
        let task_summary = agent
            .task_summary
            .as_deref()
            .map(|summary| format!(" — {summary}"))
            .unwrap_or_default();
        format!(
            "↳ {} · {}{}",
            session.name,
            session_state_label(agent.state),
            task_summary
        )
    }

    fn build_agent_node(
        session_id: AgentSessionId,
        sessions_by_id: &BTreeMap<AgentSessionId, &AgentSessionSnapshot>,
        agents_by_id: &BTreeMap<AgentSessionId, &loom_core::ProjectAgentRecord>,
        agents_by_parent: &BTreeMap<AgentSessionId, Vec<&loom_core::ProjectAgentRecord>>,
        visited: &mut BTreeSet<AgentSessionId>,
    ) -> Option<SessionTreeNode> {
        if !visited.insert(session_id) {
            return None;
        }
        let session = sessions_by_id.get(&session_id)?;
        let label = agents_by_id
            .get(&session_id)
            .map_or_else(|| session.name.clone(), |agent| agent_label(session, agent));
        let children = agents_by_parent
            .get(&session_id)
            .into_iter()
            .flatten()
            .filter_map(|agent| {
                build_agent_node(
                    agent.session_id,
                    sessions_by_id,
                    agents_by_id,
                    agents_by_parent,
                    visited,
                )
            })
            .collect();
        Some(SessionTreeNode {
            session_id,
            label,
            children,
        })
    }

    let Some(root_session) = sessions_by_id.get(&project.root_session_id) else {
        return session_list_projection(sessions, active_session_id);
    };
    let mut visited = BTreeSet::new();
    let mut root_node = SessionTreeNode {
        session_id: project.root_session_id,
        label: format!("Project · {}", root_session.name),
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
                &agents_by_id,
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
            &agents_by_id,
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
        ProjectChildControlAction::Cancel => "Cancel child task",
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

enum SessionCreationSource {
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
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .w_full()
                                .max_w(TIMELINE_CONTENT_MAX_WIDTH)
                                .p_5()
                                .rounded_lg()
                                .bg(rgb(0x171c25))
                                .border_1()
                                .border_color(rgb(0x293244))
                                .text_size(gpui_kit::rems(14. / BASE_FONT_SIZE))
                                .text_color(rgb(0xb7c0d0))
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(16. / BASE_FONT_SIZE))
                                        .text_color(rgb(0xf3f4f6))
                                        .child("Ready when you are"),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .text_size(gpui_kit::rems(14. / BASE_FONT_SIZE))
                                        .text_color(rgb(0x8f98a6))
                                        .child("Describe a task below and Loom will keep the work, decisions, and results together."),
                                ),
                        )
                );
        }

        let parent = self.parent.clone();
        let parent_for_rows = parent.clone();
        let timeline = list(self.list_state.clone(), move |index, _window, cx| {
            let view = parent_for_rows.read(cx);
            let item = &view.timeline[index];
            div()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    div()
                        .w_full()
                        .max_w(TIMELINE_CONTENT_MAX_WIDTH)
                        .child(view.render_timeline_item(item, index, &parent_for_rows)),
                )
                .into_any()
        })
        .size_full();
        if parent_state.transcript_has_older || parent_state.transcript_loading {
            let loading = parent_state.transcript_loading;
            let parent_for_page = parent.clone();
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div().w_full().flex().justify_center().p_2().child(
                        Button::new("load-older-transcript")
                            .label(if loading {
                                "Loading older messages…"
                            } else {
                                "Load older messages"
                            })
                            .small()
                            .disabled(loading)
                            .on_click(move |_, _, cx| {
                                parent_for_page.update(cx, |view, cx| {
                                    view.begin_transcript_page(view.transcript_before_ordinal, cx);
                                });
                            }),
                    ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .child(div().size_full().p_3().child(timeline)),
                )
        } else {
            div().size_full().p_3().child(timeline)
        }
    }
}

impl LoomView {
    #[cfg(test)]
    fn new_for_test(focus_handle: FocusHandle) -> Self {
        let connection = ClientConnection::InProcess(Box::new(InProcessBackend::new().connect()));
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
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            timeline_view: None,
            activity_records_seen: false,
            expanded_activities: BTreeSet::new(),
            expanded_activity_groups: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer_input: None,
            composer_placeholder: None,
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
    fn initialize_from_connection(
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
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            timeline_view: None,
            activity_records_seen: false,
            expanded_activities: BTreeSet::new(),
            expanded_activity_groups: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer_input: None,
            composer_placeholder: None,
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
                    TimelineItem::Assistant("Loom gives you a workspace for steering coding agents. Connect a backend to work with a real repository, run tools, and keep sessions available across clients. This browser demo is a static preview.".to_owned()),
                ]
            } else {
                Vec::new()
            },
            transcript_before_ordinal: None,
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            timeline_view: None,
            activity_records_seen: false,
            expanded_activities: BTreeSet::new(),
            expanded_activity_groups: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer_input: None,
            composer_placeholder: None,
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
            transcript_has_older: false,
            transcript_loading: false,
            transcript_generation: 0,
            timeline_view: None,
            activity_records_seen: false,
            expanded_activities: BTreeSet::new(),
            expanded_activity_groups: BTreeSet::new(),
            approval_request_in_flight: false,
            approval_settings_request_in_flight: false,
            archive_request_in_flight: false,
            pending_approval: None,
            pending_input: None,
            composer_input: None,
            composer_placeholder: None,
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
        self.transcript_has_older = false;
        self.transcript_loading = false;
        self.activity_records_seen = false;
        self.expanded_activities.clear();
        self.expanded_activity_groups.clear();
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

    fn rebuild_project_message_timeline(&mut self) {
        self.timeline
            .retain(|item| !matches!(item, TimelineItem::ProjectMessage(_)));
        if self
            .project_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.root_session_id == self.active_session.id)
        {
            self.timeline.extend(
                self.project_messages
                    .iter()
                    .cloned()
                    .map(TimelineItem::ProjectMessage),
            );
        }
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
        let workspace_changed = self.active_session.workspace_id != session.workspace_id;
        self.active_session = session;
        self.project_snapshot = None;
        self.project_snapshot_stale = false;
        self.project_messages.clear();
        self.project_message_cursors.clear();
        self.project_messages_stale = false;
        self.project_messages_loading = false;
        self.project_message_generation = self.project_message_generation.wrapping_add(1);
        if workspace_changed {
            self.project_feed_after_sequence = None;
            self.project_feed_epoch = None;
            self.project_poll_scheduled = false;
        }
        self.session_repositories.clear();
        self.session_directories.clear();
        self.selected_repository_id = None;
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
        self.review.selected_path = None;
        self.review.selected_staged = false;
        self.review.selected_diff = None;
        self.review.selected_file = None;
        self.review.rows.clear();
        self.review.hunk_rows.clear();
        self.review.list_state.reset(0);
        self.review.changes.clear();
        self.review.vcs = None;
        self.review.repositories_loaded = false;
    }

    fn refresh_active_project_snapshot(&mut self, cx: &mut Context<Self>) {
        let session_id = self.active_session.id;
        let backend = match self.backend_for_session(session_id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("load project snapshot", error);
                return;
            }
        };
        self.project_snapshot_stale = false;
        let request = backend.submit(RequestEnvelope::new(
            ClientRequest::GetProjectSnapshotForSession { session_id },
        ));
        cx.spawn(async move |view, cx| {
            let response = cx
                .background_spawn(async move { request.wait().await })
                .await;
            view.update(cx, |view, cx| {
                if view.active_session.id != session_id {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::ProjectSnapshot(snapshot)) => {
                        view.project_snapshot = Some(snapshot);
                        view.project_messages_stale = true;
                    }
                    Err(error) if error.code == ErrorCode::NotFound => {
                        view.project_snapshot = None;
                        view.project_messages.clear();
                        view.project_message_cursors.clear();
                        view.rebuild_project_message_timeline();
                    }
                    Err(error) => {
                        view.record_backend_error("load project snapshot", error);
                    }
                    Ok(response) => view.record_backend_error(
                        "load project snapshot",
                        unexpected_response("project snapshot", response),
                    ),
                }
                view.refresh_project_messages(cx);
                view.schedule_project_poll(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn refresh_project_messages(&mut self, cx: &mut Context<Self>) {
        if self.project_messages_loading || !self.project_messages_stale {
            return;
        }
        let Some(project) = self
            .project_snapshot
            .as_ref()
            .filter(|snapshot| snapshot.root_session_id == self.active_session.id)
        else {
            return;
        };
        let project_id = project.project_id;
        let mut recipients = project
            .agents
            .iter()
            .map(|agent| agent.session_id)
            .collect::<Vec<_>>();
        recipients.push(project.root_session_id);
        recipients.sort();
        recipients.dedup();
        if recipients.len() <= 1 {
            self.project_messages.clear();
            self.project_message_cursors.clear();
            self.project_messages_stale = false;
            self.rebuild_project_message_timeline();
            return;
        }
        let backend = match self.backend_for_session(self.active_session.id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("load project messages", error);
                return;
            }
        };
        let mut cursors = self.project_message_cursors.clone();
        let generation = self.project_message_generation;
        self.project_messages_stale = false;
        self.project_messages_loading = true;
        cx.spawn(async move |view, cx| {
            let mut messages = Vec::new();
            let mut failure = None;
            for session_id in recipients {
                let mut cursor = cursors.get(&session_id).copied();
                loop {
                    let request = backend.submit(RequestEnvelope::new(
                        ClientRequest::ListProjectAgentMessages {
                            project_id,
                            session_id,
                            after_project_sequence: cursor,
                            limit: 512,
                        },
                    ));
                    let response = cx
                        .background_spawn(async move { request.wait().await })
                        .await;
                    match response.result {
                        Ok(ServerResponse::ProjectAgentMessages {
                            messages: page,
                            next_after_project_sequence,
                        }) => {
                            if let Some(last) = page.last() {
                                cursor = Some(last.project_sequence);
                                cursors.insert(session_id, last.project_sequence);
                            }
                            messages.extend(page);
                            if let Some(next) = next_after_project_sequence {
                                cursor = Some(next);
                                cursors.insert(session_id, next);
                                continue;
                            }
                            break;
                        }
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                        Ok(response) => {
                            failure = Some(unexpected_response("project messages", response));
                            break;
                        }
                    }
                }
                if failure.is_some() {
                    break;
                }
            }
            view.update(cx, |view, cx| {
                if view.project_message_generation != generation
                    || !view
                        .project_snapshot
                        .as_ref()
                        .is_some_and(|snapshot| view.active_session.id == snapshot.root_session_id)
                {
                    return;
                }
                let succeeded = failure.is_none();
                view.project_messages_loading = false;
                if let Some(error) = failure {
                    view.project_messages_stale = true;
                    view.record_backend_error("load project messages", error);
                } else {
                    view.project_message_cursors = cursors;
                    for message in messages {
                        if !view
                            .project_messages
                            .iter()
                            .any(|existing| existing.message_id == message.message_id)
                        {
                            view.project_messages.push(message);
                        }
                    }
                    view.project_messages
                        .sort_by_key(|message| message.project_sequence);
                    view.rebuild_project_message_timeline();
                }
                if succeeded && view.project_messages_stale {
                    view.refresh_project_messages(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn project_root_is_active(&self) -> bool {
        self.project_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.root_session_id == self.active_session.id)
    }

    fn project_has_live_children(&self) -> bool {
        let Some(project) = self.project_snapshot.as_ref() else {
            return false;
        };
        let has_live_task = project.tasks.iter().any(|task| {
            !matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            )
        });
        let has_live_session = project.agents.iter().any(|agent| {
            agent.session_id != project.root_session_id
                && matches!(
                    agent.state,
                    AgentSessionState::Queued
                        | AgentSessionState::Planning
                        | AgentSessionState::AwaitingApproval
                        | AgentSessionState::Paused
                        | AgentSessionState::Executing
                        | AgentSessionState::Evaluating
                        | AgentSessionState::NeedsInput
                )
        });
        has_live_task || has_live_session
    }

    fn control_project_child_from_ui(
        &mut self,
        manager_session_id: AgentSessionId,
        project_id: loom_core::ProjectId,
        task_id: loom_core::TaskId,
        action: ProjectChildControlAction,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::ControlProjectChild {
                project_id,
                manager_session_id,
                task_id,
                action,
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::ProjectChildControlled { task, .. }) => {
                    view.record_status(format!("{} · {:?}", task.child_name, task.status));
                    view.project_snapshot_stale = true;
                    view.refresh_active_project_snapshot(cx);
                }
                Err(error) => view.record_backend_error("control project child", error),
                Ok(response) => view.record_backend_error(
                    "control project child",
                    unexpected_response("project child control", response),
                ),
            },
        );
    }

    fn review_project_child_from_ui(
        &mut self,
        manager_session_id: AgentSessionId,
        project_id: loom_core::ProjectId,
        task_id: loom_core::TaskId,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::GetProjectChildReview {
                project_id,
                manager_session_id,
                task_id,
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::ProjectChildReview {
                    worktree,
                    status,
                    diff,
                }) => {
                    view.project_child_review =
                        Some((worktree.clone(), status.clone(), diff.clone()));
                    view.project_snapshot_stale = true;
                    view.review.open = true;
                    view.review.panel = ReviewPanel::Changes;
                    view.review.selected_file = None;
                    view.review.selected_path = Some(format!(
                        "{} · diff from {}",
                        worktree.branch_name, worktree.base_revision
                    ));
                    view.review.selected_staged = false;
                    view.review.selection_revision = view.review.selection_revision.wrapping_add(1);
                    view.review.vcs = Some(status);
                    view.review.show_diff(diff);
                    view.refresh_active_project_snapshot(cx);
                }
                Err(error) => view.record_backend_error("review project child", error),
                Ok(response) => view.record_backend_error(
                    "review project child",
                    unexpected_response("project child review", response),
                ),
            },
        );
    }

    fn integrate_project_child_from_ui(
        &mut self,
        manager_session_id: AgentSessionId,
        project_id: loom_core::ProjectId,
        task_id: loom_core::TaskId,
        expected_parent_revision: String,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::IntegrateProjectChild {
                project_id,
                manager_session_id,
                task_id,
                expected_parent_revision,
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::ProjectChildWorktreeUpdated(worktree)) => {
                    view.project_child_review = None;
                    view.project_snapshot_stale = true;
                    view.record_status(format!(
                        "Child changes integrated at {}",
                        worktree
                            .integrated_revision
                            .as_deref()
                            .unwrap_or("unknown revision")
                    ));
                    view.refresh_active_project_snapshot(cx);
                    view.refresh_review(cx);
                }
                Err(error) => view.record_backend_error("integrate project child", error),
                Ok(response) => view.record_backend_error(
                    "integrate project child",
                    unexpected_response("project child integration", response),
                ),
            },
        );
    }

    fn cleanup_project_child_from_ui(
        &mut self,
        manager_session_id: AgentSessionId,
        project_id: loom_core::ProjectId,
        task_id: loom_core::TaskId,
        disposition: loom_core::ProjectWorktreeCleanupDisposition,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::CleanupProjectChildWorktree {
                project_id,
                manager_session_id,
                task_id,
                disposition,
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::ProjectChildWorktreeUpdated(worktree)) => {
                    view.project_snapshot_stale = true;
                    view.record_status(format!("Child checkout is {:?}", worktree.status));
                    if view
                        .project_child_review
                        .as_ref()
                        .is_some_and(|(current, _, _)| current.task_id == worktree.task_id)
                    {
                        view.project_child_review = None;
                        view.review.open = false;
                    }
                    view.refresh_active_project_snapshot(cx);
                }
                Err(error) => view.record_backend_error("clean up project child", error),
                Ok(response) => view.record_backend_error(
                    "clean up project child",
                    unexpected_response("project child cleanup", response),
                ),
            },
        );
    }

    fn schedule_project_poll(&mut self, cx: &mut Context<Self>) {
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

    fn poll_project_once(&mut self, cx: &mut Context<Self>) {
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

    /// Loads a session synchronously.
    ///
    /// Only used by the startup bootstrap, before a window exists; every
    /// interactive path uses [`Self::select_session`], which goes through the
    /// connection worker.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn load_session(&mut self, session: AgentSessionSnapshot) {
        self.activate_session(session);
        let metadata_response = self.connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionInitialState {
                session_id: self.active_session.id,
            },
        ));
        let snapshot_response = if metadata_response.result.is_err() {
            self.connection.request(RequestEnvelope::new(
                ClientRequest::GetAgentSessionSnapshot {
                    session_id: self.active_session.id,
                },
            ))
        } else {
            metadata_response
        };
        let mut event_cursor = None;
        let snapshot_result = match snapshot_response.result {
            Ok(ServerResponse::AgentSessionInitialState(initial)) => {
                event_cursor = Some(initial.cursor);
                Ok(ServerResponse::AgentSessionSnapshot(initial.projection))
            }
            result => result,
        };
        let mut needs_transcript_page = false;
        let fallback_projection = match snapshot_result {
            Ok(ServerResponse::AgentSessionSnapshot(projection)) => {
                needs_transcript_page = projection
                    .active_run
                    .as_ref()
                    .is_some_and(|run| run.messages.is_empty());
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
            event_cursor,
            fallback_projection.and_then(|projection| projection.active_run),
        ) {
            self.record_backend_error("load session events", error);
        }
        if needs_transcript_page && let Some(run_id) = self.active_run_id {
            match load_transcript_page_sync(&self.connection, run_id, None) {
                Ok((messages, next_before, has_older)) => {
                    self.apply_transcript_page(run_id, None, messages, next_before, has_older);
                }
                Err(error) => self.record_backend_error("load conversation history", error),
            }
        }
        self.ensure_session_task_message(self.active_session.id);
        if let Some(run) = &self.active_run {
            self.session_task_cache
                .insert(self.active_session.id, run.task.clone());
            self.ensure_session_task_message(self.active_session.id);
        }
        match self
            .connection
            .request(RequestEnvelope::new(
                ClientRequest::ListSessionRepositories {
                    session_id: self.active_session.id,
                },
            ))
            .result
        {
            Ok(ServerResponse::SessionRepositories { repositories }) => {
                self.selected_repository_id = repositories.first().map(|repository| repository.id);
                self.session_repositories = repositories;
            }
            Err(error) => self.record_backend_error("load session repositories", error),
            Ok(response) => self.record_backend_error(
                "load session repositories",
                unexpected_response("session repository list", response),
            ),
        }
        if let Ok(ServerResponse::SessionDirectories { directories }) = self
            .connection
            .request(RequestEnvelope::new(
                ClientRequest::ListSessionDirectories {
                    session_id: self.active_session.id,
                },
            ))
            .result
        {
            self.session_directories = directories;
        }
        if let Ok(ServerResponse::ProjectSnapshot(snapshot)) = self
            .connection
            .request(RequestEnvelope::new(
                ClientRequest::GetProjectSnapshotForSession {
                    session_id: self.active_session.id,
                },
            ))
            .result
        {
            self.project_snapshot = Some(snapshot);
            self.project_messages_stale = true;
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn collect_events_since(
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
    fn apply_event_snapshot(
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
    fn poll_run_once(&mut self, cx: &mut Context<Self>) {
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
                self.context_inspection = None;
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
            AgentEvent::ContextInspected { inspection, .. } => {
                if inspection.compacted {
                    self.record_status(format!("Context compacted into lossy excerpts (approximately {} tokens removed). Full history is retained.", inspection.omitted_tokens));
                }
                self.context_inspection = Some(inspection.clone());
            }
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
            AgentEvent::ToolApprovalRequired {
                run_id,
                attempt_id,
                control_revision,
                call,
                ..
            } => {
                self.update_active_run_control(*run_id, *attempt_id, *control_revision);
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
            AgentEvent::ToolApprovalDecided {
                run_id,
                attempt_id,
                control_revision,
                ..
            } => {
                self.update_active_run_control(*run_id, *attempt_id, *control_revision);
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
            AgentEvent::NeedsInput {
                run_id,
                attempt_id,
                control_revision,
                prompt,
                ..
            } => {
                self.update_active_run_control(*run_id, *attempt_id, *control_revision);
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

    fn update_active_run_control(
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
        self.review.repositories_loaded = false;
        self.dispatch(
            cx,
            ClientRequest::ListSessionDirectories {
                session_id: self.active_session.id,
            },
            |view, response, _| match response.result {
                Ok(ServerResponse::SessionDirectories { directories }) => {
                    view.session_directories = directories;
                }
                Err(error) => view.record_backend_error("list session directories", error),
                Ok(response) => view.record_backend_error(
                    "list session directories",
                    unexpected_response("session directory list", response),
                ),
            },
        );
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::GetSessionFilesystemChanges {
                session_id,
                after_sequence: None,
            },
            move |view, response, _| {
                if view.active_session.id != session_id {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::SessionFilesystemChanges { changes, truncated }) => {
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
                }
            },
        );
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::ListSessionRepositories { session_id },
            move |view, response, cx| {
                if view.active_session.id != session_id {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::SessionRepositories { repositories }) => {
                        view.review.repositories_loaded = true;
                        let repository = repositories
                            .iter()
                            .find(|repository| Some(repository.id) == view.selected_repository_id)
                            .or_else(|| repositories.first())
                            .cloned();
                        view.session_repositories = repositories;
                        view.selected_repository_id =
                            repository.as_ref().map(|repository| repository.id);
                        if let Some(repository) = repository {
                            let repository_id = repository.id;
                            let session_id = view.active_session.id;
                            view.dispatch(
                                cx,
                                ClientRequest::GetSessionVcsStatus {
                                    session_id,
                                    repository_id,
                                },
                                move |view, response, _| {
                                    if view.active_session.id != session_id
                                        || view.selected_repository_id != Some(repository_id)
                                    {
                                        return;
                                    }
                                    match response.result {
                                        Ok(ServerResponse::VcsStatus(status)) => {
                                            view.review.vcs = Some(status)
                                        }
                                        Err(error) => {
                                            view.review.vcs = None;
                                            view.record_status(format!(
                                                "VCS review unavailable: {error}"
                                            ));
                                        }
                                        Ok(response) => view.record_backend_error(
                                            "VCS review refresh",
                                            unexpected_response("VCS status", response),
                                        ),
                                    }
                                },
                            );
                        } else {
                            view.review.vcs = None;
                        }
                    }
                    Err(error) => {
                        view.review.repositories_loaded = true;
                        view.review.vcs = None;
                        view.record_status(format!("VCS review unavailable: {error}"));
                    }
                    Ok(response) => {
                        view.review.repositories_loaded = true;
                        view.record_backend_error(
                            "VCS review refresh",
                            unexpected_response("session repository list", response),
                        );
                    }
                }
            },
        );
    }

    pub(crate) fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let name = self
            .rename_input_state
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_owned();
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
                            view.activate_session(empty_session_snapshot(view.workspace_id));
                            view.review.open = false;
                            cx.notify();
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
        #[cfg(target_family = "wasm")]
        if self.browser_demo_mode {
            self.timeline.push(TimelineItem::User(message));
            self.timeline.push(TimelineItem::Assistant(
                "This is demo mode. The browser client needs to connect to a backend to work."
                    .to_owned(),
            ));
            cx.notify();
            return;
        }
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
            let Some(run) = self.active_run.as_ref().filter(|run| run.id == run_id) else {
                self.sending_message = false;
                self.record_backend_error(
                    "send message",
                    LoomError::invalid_state("active run control state is unavailable"),
                );
                return;
            };
            self.optimistic_messages.push(message.clone());
            ClientRequest::SendAgentMessage {
                run_id,
                attempt_id: run.attempt_id,
                expected_control_revision: run.control_revision,
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
            ClientRequest::StartSessionAgentRun {
                session_id: self.active_session.id,
                task: message.clone(),
                model: self.model.clone(),
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
        let Some(run) = self.active_run.as_ref().filter(|run| run.id == run_id) else {
            self.record_backend_error(
                "approve action",
                LoomError::invalid_state("active run control state is unavailable"),
            );
            return;
        };
        self.approval_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::ApproveAgentAction {
                run_id,
                attempt_id: run.attempt_id,
                expected_control_revision: run.control_revision,
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
        let Some(run) = self.active_run.as_ref().filter(|run| run.id == run_id) else {
            self.record_backend_error(
                "reject action",
                LoomError::invalid_state("active run control state is unavailable"),
            );
            return;
        };
        self.approval_request_in_flight = true;
        self.dispatch(
            cx,
            ClientRequest::RejectAgentAction {
                run_id,
                attempt_id: run.attempt_id,
                expected_control_revision: run.control_revision,
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
        let text = self
            .composer_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_owned();
        if text.is_empty() {
            return;
        }
        self.clear_composer_on_render = true;
        #[cfg(target_family = "wasm")]
        if self.browser_demo_mode {
            self.send_message(text, cx);
            return;
        }
        if text.starts_with('/') {
            self.run_slash_command(&text, cx);
            return;
        }
        self.send_message(text, cx);
    }

    fn run_slash_command(&mut self, text: &str, cx: &mut Context<Self>) {
        match text.split_whitespace().next().unwrap_or_default() {
            "/repo" | "/repository" => {
                self.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
            }
            "/review" => {
                self.review.open = true;
                self.review.panel = ReviewPanel::Changes;
                self.refresh_review(cx);
            }
            "/help" => self.record_status("Available tools: /repo, /review, /help"),
            command => self.record_status(format!("Unknown command '{command}'. Try /help.")),
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
        self.source_dialog = None;
        self.settings_open = false;
        self.providers_open = false;
        self.about_open = false;
        self.review.open = false;
        self.project_child_review = None;
        let project_context = self.project_snapshot.clone().filter(|snapshot| {
            snapshot
                .agents
                .iter()
                .any(|agent| agent.session_id == session.id)
        });
        self.activate_session(session.clone());
        self.project_snapshot = project_context;
        if let Some(node_id) = self.session_node_ids.get(&session.id).cloned()
            && self.model_catalog_node_id.as_deref() != Some(node_id.as_str())
        {
            self.refresh_models_for_node_async(node_id, cx);
        }
        self.ensure_session_task_message(session.id);
        let session_id = session.id;
        let mut event_stream_epoch = self.event_stream_epoch.clone();
        let snapshot_request = backend.submit(RequestEnvelope::new(
            ClientRequest::GetAgentSessionInitialState { session_id },
        ));
        cx.spawn(async move |view, cx| {
            let mut snapshot = cx
                .background_spawn(async move { snapshot_request.wait().await })
                .await;
            if snapshot.result.is_err() {
                snapshot = backend
                    .submit(RequestEnvelope::new(
                        ClientRequest::GetAgentSessionSnapshot { session_id },
                    ))
                    .wait()
                    .await;
            }
            let cursor = match &snapshot.result {
                Ok(ServerResponse::AgentSessionInitialState(initial)) => Some(initial.cursor),
                _ => None,
            };
            if let Ok(ServerResponse::AgentSessionInitialState(initial)) = snapshot.result.clone() {
                snapshot.result = Ok(ServerResponse::AgentSessionSnapshot(initial.projection));
            }
            let events_request =
                backend.submit(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    workspace_id: None,
                    after_sequence: cursor,
                    stream_epoch: event_stream_epoch.clone(),
                }));
            let mut events = cx
                .background_spawn(async move { events_request.wait().await })
                .await;
            if let Ok(ServerResponse::SessionEventsSnapshot {
                stream_epoch: Some(epoch),
                ..
            }) = &events.result
            {
                event_stream_epoch = Some(epoch.clone());
            }
            if matches!(
                &events.result,
                Ok(ServerResponse::SessionEventsSnapshot { .. })
            ) {
                let refresh_request = backend.submit(RequestEnvelope::new(
                    ClientRequest::GetAgentSessionInitialState { session_id },
                ));
                let mut refreshed = cx
                    .background_spawn(async move { refresh_request.wait().await })
                    .await;
                if let Ok(ServerResponse::AgentSessionInitialState(initial)) =
                    refreshed.result.clone()
                {
                    let refreshed_cursor = initial.cursor;
                    refreshed.result = Ok(ServerResponse::AgentSessionSnapshot(initial.projection));
                    let retry_request =
                        backend.submit(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                            session_id: Some(session_id),
                            workspace_id: None,
                            after_sequence: Some(refreshed_cursor),
                            stream_epoch: event_stream_epoch.clone(),
                        }));
                    snapshot = refreshed;
                    events = cx
                        .background_spawn(async move { retry_request.wait().await })
                        .await;
                }
            }
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
        let needs_transcript_page = match &snapshot_response.result {
            Ok(ServerResponse::AgentSessionSnapshot(projection)) => projection
                .active_run
                .as_ref()
                .is_some_and(|run| run.messages.is_empty()),
            _ => false,
        };
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
        self.after_sequence = fallback_projection
            .as_ref()
            .map(|projection| projection.latest_sequence);
        match events_response.result {
            Ok(ServerResponse::SessionEvents {
                events,
                stream_epoch,
            }) => {
                self.event_stream_epoch = stream_epoch;
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
                stream_epoch,
                ..
            }) => {
                self.event_stream_epoch = stream_epoch;
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
        if needs_transcript_page && self.active_run_id.is_some() {
            self.begin_transcript_page(None, cx);
        }
        self.refresh_review(cx);
        self.refresh_active_project_snapshot(cx);
        cx.notify();
    }

    fn begin_transcript_page(&mut self, before_ordinal: Option<u64>, cx: &mut Context<Self>) {
        let Some(run_id) = self.active_run_id else {
            return;
        };
        if self.transcript_loading || (before_ordinal.is_some() && !self.transcript_has_older) {
            return;
        }
        let backend = match self.backend_for_session(self.active_session.id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("load conversation history", error);
                cx.notify();
                return;
            }
        };
        let transcript_generation = self.transcript_generation;
        self.transcript_loading = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = load_transcript_page(backend, run_id, before_ordinal).await;
            view.update(cx, |view, cx| {
                if view.active_run_id != Some(run_id)
                    || view.transcript_generation != transcript_generation
                {
                    return;
                }
                view.transcript_loading = false;
                match result {
                    Ok((messages, next_before, has_older)) => {
                        view.apply_transcript_page(
                            run_id,
                            before_ordinal,
                            messages,
                            next_before,
                            has_older,
                        );
                    }
                    Err(error) => view.record_backend_error("load conversation history", error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_transcript_page(
        &mut self,
        run_id: RunId,
        before_ordinal: Option<u64>,
        messages: Vec<ModelMessage>,
        next_before: Option<u64>,
        has_older: bool,
    ) {
        if self.active_run_id != Some(run_id) {
            return;
        }
        let page_items = timeline_items_from_messages(messages, self.activity_records_seen);
        if before_ordinal.is_none() {
            self.timeline.retain(|item| {
                !matches!(
                    item,
                    TimelineItem::User(_)
                        | TimelineItem::Assistant(_)
                        | TimelineItem::ToolOutput(_)
                )
            });
            let insertion_index = self.transcript_insertion_index();
            self.timeline
                .splice(insertion_index..insertion_index, page_items);
            self.place_restored_activities_after_task();
        } else {
            let insertion_index = self.transcript_insertion_index();
            prepend_timeline_page(&mut self.timeline, page_items, insertion_index);
        }
        self.transcript_before_ordinal = next_before;
        self.transcript_has_older = has_older;
        self.ensure_session_task_message(self.active_session.id);
    }

    fn transcript_insertion_index(&self) -> usize {
        let mut index = usize::from(matches!(
            self.timeline.first(),
            Some(TimelineItem::Plan { .. })
        ));
        let task = self.session_task_cache.get(&self.active_session.id);
        if matches!(self.timeline.get(index), Some(TimelineItem::User(text)) if task == Some(text))
        {
            index += 1;
        }
        index
    }

    fn place_restored_activities_after_task(&mut self) {
        if !self
            .timeline
            .iter()
            .any(|item| matches!(item, TimelineItem::User(_)))
        {
            return;
        }
        let mut activities = Vec::new();
        self.timeline.retain(|item| {
            if matches!(item, TimelineItem::ActivitySection { .. }) {
                activities.push(item.clone());
                false
            } else {
                true
            }
        });
        let insertion_index = self
            .timeline
            .iter()
            .rposition(|item| matches!(item, TimelineItem::User(_)))
            .expect("user message was checked above")
            + 1;
        self.timeline
            .splice(insertion_index..insertion_index, activities);
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
            let index = usize::from(matches!(
                self.timeline.first(),
                Some(TimelineItem::Plan { .. })
            ));
            self.timeline.insert(index, TimelineItem::User(task));
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

    fn adjust_project_agent_concurrency(&mut self, delta: i8, cx: &mut Context<Self>) {
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

    fn set_font_scale_percent(
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

    fn adjust_font_scale(&mut self, delta: i16, window: &mut Window, cx: &mut Context<Self>) {
        self.set_font_scale_percent(
            (self.font_scale_percent as i16 + delta)
                .clamp(MIN_FONT_SCALE_PERCENT as i16, MAX_FONT_SCALE_PERCENT as i16)
                as u16,
            window,
            cx,
        );
    }

    fn persist_and_distribute_workspace_config(
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

    fn configure_api_key_provider(
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
        self.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
    }

    fn begin_source_dialog(&mut self, purpose: SessionSourceDialogPurpose, cx: &mut Context<Self>) {
        let active_node_id = self.session_node_ids.get(&self.active_session.id);
        let local_directory_available = local_source_available(
            purpose,
            self.local_directory_sources_available,
            active_node_id.map(String::as_str),
            &self.default_backend_node_id,
        );
        let choice = source_dialog_initial_state(purpose, local_directory_available);
        self.source_dialog = Some(SessionSourceDialog {
            purpose,
            choice,
            local_directory_available,
            filter_subscription: None,
            repositories: Vec::new(),
            selected_repository: None,
            repositories_loading: false,
            error: None,
        });
        self.source_path_input = None;
        self.repository_filter_input = None;
        self.pending_source_path = None;
        if choice == SessionSourceChoice::GitHub {
            self.load_github_repositories(cx);
        }
        cx.notify();
    }

    fn choose_source(&mut self, choice: SessionSourceChoice, cx: &mut Context<Self>) {
        let Some(dialog) = self.source_dialog.as_ref() else {
            return;
        };
        if !source_choice_is_allowed(dialog.purpose, dialog.local_directory_available, choice) {
            return;
        }
        if let Some(dialog) = &mut self.source_dialog {
            dialog.choice = choice;
            dialog.error = None;
            if choice == SessionSourceChoice::GitHub && dialog.repositories.is_empty() {
                dialog.repositories_loading = true;
            }
        }
        if choice == SessionSourceChoice::GitHub {
            self.load_github_repositories(cx);
        }
        cx.notify();
    }

    fn browse_local_directory(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a local folder".into()),
        });
        cx.spawn(async move |view, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    view.update(cx, |view, cx| {
                        view.pending_source_path = Some(path.display().to_string());
                        cx.notify();
                    })
                    .ok();
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                view.update(cx, |view, cx| {
                    view.record_status(format!("Could not open folder browser: {error}"));
                    cx.notify();
                })
                .ok();
            }
            Err(_) => {}
        })
        .detach();
    }

    fn load_github_repositories(&mut self, cx: &mut Context<Self>) {
        let node_id = self
            .source_dialog
            .as_ref()
            .map(|dialog| match dialog.purpose {
                SessionSourceDialogPurpose::StartSession => self.default_backend_node_id.clone(),
                SessionSourceDialogPurpose::AddToSession => self
                    .session_node_ids
                    .get(&self.active_session.id)
                    .cloned()
                    .unwrap_or_else(|| self.default_backend_node_id.clone()),
            });
        let Some(node_id) = node_id else {
            return;
        };
        if let Some(dialog) = &mut self.source_dialog {
            dialog.repositories_loading = true;
            dialog.error = None;
        }
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::ListGitHubRepositories,
            |view, response, _| {
                if let Some(dialog) = &mut view.source_dialog {
                    dialog.repositories_loading = false;
                    match response.result {
                        Ok(ServerResponse::GitHubRepositories { repositories }) => {
                            dialog.repositories = repositories;
                            dialog.error = None;
                        }
                        Err(error) => dialog.error = Some(error.message),
                        Ok(response) => {
                            dialog.error = Some(
                                unexpected_response("GitHub repository list", response).message,
                            )
                        }
                    }
                }
            },
        );
    }

    fn confirm_source_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.source_dialog.take() else {
            return;
        };
        let local_path = self
            .source_path_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let source = match dialog.choice {
            SessionSourceChoice::Empty => None,
            SessionSourceChoice::LocalDirectory => {
                let path = local_path.trim();
                if path.is_empty() || !PathBuf::from(path).is_absolute() {
                    self.source_dialog = Some(dialog);
                    self.record_status("Enter an absolute local directory path");
                    return;
                }
                Some(SessionCreationSource::LocalDirectory(path.to_owned()))
            }
            SessionSourceChoice::GitHub => {
                let selected_repository = dialog.selected_repository.as_deref();
                let Some(repository) = dialog
                    .repositories
                    .iter()
                    .find(|repository| selected_repository == Some(repository.full_name.as_str()))
                    .cloned()
                else {
                    self.source_dialog = Some(dialog);
                    self.record_status("Choose a GitHub repository");
                    return;
                };
                Some(SessionCreationSource::GitHub(repository))
            }
        };
        self.source_dialog = None;
        self.source_path_input = None;
        self.repository_filter_input = None;
        match dialog.purpose {
            SessionSourceDialogPurpose::StartSession => {
                let name = source.as_ref().map_or_else(
                    || format!("Session {}", self.sessions.len().saturating_add(1)),
                    session_name_for_source,
                );
                self.create_session_on_node_with_source(
                    self.default_backend_node_id.clone(),
                    name,
                    source,
                    cx,
                );
            }
            SessionSourceDialogPurpose::AddToSession => {
                if let Some(source) = source {
                    self.add_source_to_active_session(source, cx);
                }
            }
        }
    }

    fn add_source_to_active_session(
        &mut self,
        source: SessionCreationSource,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.active_session.id;
        match source {
            SessionCreationSource::LocalDirectory(source) => self.dispatch(
                cx,
                ClientRequest::AttachSessionDirectory {
                    session_id,
                    source,
                    path: format!("sources/{}", uuid::Uuid::new_v4()),
                },
                |view, response, cx| match response.result {
                    Ok(ServerResponse::SessionDirectoryAttached {
                        directory,
                        repositories,
                    }) => {
                        view.session_directories.push(directory);
                        if let Some(repository) = repositories.first() {
                            view.selected_repository_id = Some(repository.id);
                        }
                        view.session_repositories.extend(repositories);
                        view.refresh_review(cx);
                        cx.notify();
                    }
                    Err(error) => view.record_backend_error("attach directory", error),
                    Ok(response) => view.record_backend_error(
                        "attach directory",
                        unexpected_response("directory attachment", response),
                    ),
                },
            ),
            SessionCreationSource::GitHub(repository) => {
                let repository_id = RepositoryId::new();
                self.dispatch(
                    cx,
                    ClientRequest::AttachSessionRepository {
                        session_id,
                        source: repository.clone_url,
                        path: format!("repositories/{repository_id}"),
                        revision: None,
                    },
                    move |view, response, cx| match response.result {
                        Ok(ServerResponse::SessionRepositoryAttached(repository)) => {
                            view.selected_repository_id = Some(repository.id);
                            view.session_repositories.push(repository);
                            view.refresh_review(cx);
                        }
                        Err(error) => view.record_backend_error("attach repository", error),
                        Ok(response) => view.record_backend_error(
                            "attach repository",
                            unexpected_response("repository attachment", response),
                        ),
                    },
                );
            }
        }
    }

    fn render_new_session_button(
        &self,
        view: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let _ = view;
        Button::new("new-session")
            .icon(Icon::new(IconName::Plus))
            .ghost()
            .small()
            .tooltip("Start a session")
            .on_click(cx.listener(Self::new_session))
            .into_any_element()
    }

    fn create_session_on_node_with_source(
        &mut self,
        node_id: String,
        name: String,
        source: Option<SessionCreationSource>,
        cx: &mut Context<Self>,
    ) {
        let creation_status = match source.as_ref() {
            Some(SessionCreationSource::GitHub(repository)) => {
                format!("Creating session and cloning {}…", repository.full_name)
            }
            Some(SessionCreationSource::LocalDirectory(_)) => {
                "Creating session and attaching directory…".to_owned()
            }
            None => "Creating session…".to_owned(),
        };
        self.record_status(creation_status);
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
        let workspace_id = self.workspace_id;
        let Some(workspace) = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .cloned()
        else {
            self.record_backend_error(
                "create session",
                LoomError::not_found("workspace", workspace_id),
            );
            cx.notify();
            return;
        };
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
                log::info!("registering workspace before session creation");
                let registered = backend
                    .submit(RequestEnvelope::new(ClientRequest::RegisterWorkspace {
                        workspace: workspace.clone(),
                    }))
                    .wait()
                    .await;
                match registered.result? {
                    ServerResponse::WorkspaceCreated(_) => {}
                    response => {
                        return Err(unexpected_response("workspace registration", response));
                    }
                }
                let response = backend
                    .submit(RequestEnvelope::new(
                        ClientRequest::CreateAgentSessionInWorkspace { workspace_id, name },
                    ))
                    .wait()
                    .await;
                let snapshot = match response.result? {
                    ServerResponse::AgentSessionCreated(snapshot) => snapshot,
                    response => return Err(unexpected_response("session creation", response)),
                };
                log::info!("created session {}; attaching source", snapshot.id);
                let setup = match source {
                    None => Ok(()),
                    Some(SessionCreationSource::LocalDirectory(source)) => {
                        let response = backend
                            .submit(RequestEnvelope::new(ClientRequest::AttachSessionDirectory {
                                session_id: snapshot.id,
                                source,
                                path: format!("sources/{}", uuid::Uuid::new_v4()),
                            }))
                            .wait()
                            .await;
                        match response.result? {
                            ServerResponse::SessionDirectoryAttached { .. } => Ok(()),
                            response => Err(unexpected_response("directory attachment", response)),
                        }
                    }
                    Some(SessionCreationSource::GitHub(repository)) => {
                        log::info!("cloning GitHub repository {} into session {}", repository.full_name, snapshot.id);
                        let repository_id = RepositoryId::new();
                        let response = backend
                            .submit(RequestEnvelope::new(ClientRequest::AttachSessionRepository {
                                session_id: snapshot.id,
                                source: repository.clone_url,
                                path: format!("repositories/{repository_id}"),
                                revision: None,
                            }))
                            .wait()
                            .await;
                        match response.result? {
                            ServerResponse::SessionRepositoryAttached(_) => Ok(()),
                            response => Err(unexpected_response("repository attachment", response)),
                        }
                    }
                };
                if let Err(error) = setup {
                    log::error!("session source setup failed: {}", error.message);
                    let _ = backend
                        .submit(RequestEnvelope::new(ClientRequest::ArchiveAgentSession {
                            session_id: snapshot.id,
                        }))
                        .wait()
                        .await;
                    return Err(error);
                }
                Ok(snapshot)
            }
            .await;
            view.update(cx, |view, cx| match result {
                Ok(snapshot) => {
                    view.record_status("Session created successfully");
                    view.node_model_catalogs
                        .insert(node_id.clone(), models);
                    view.session_models.insert(snapshot.id, model);
                    view.session_node_ids
                        .insert(snapshot.id, node_id.clone());
                    view.sessions.push(snapshot.clone());
                    view.select_session(snapshot, cx);
                }
                Err(error) => {
                    log::error!("session creation failed: {}", error.message);
                    view.record_backend_error("create session", error)
                },
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn begin_session_rename(
        &mut self,
        session: AgentSessionSnapshot,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_session(session, cx);
        self.rename_dialog = Some(RenameDialogState {
            session: self.active_session.clone(),
            input: self.active_session.name.clone(),
        });
        self.rename_input_state = None;
    }

    fn select_session_repository(&mut self, repository_id: RepositoryId, cx: &mut Context<Self>) {
        let session_id = self.active_session.id;
        self.selected_repository_id = Some(repository_id);
        self.review.selected_path = None;
        self.review.selected_diff = None;
        self.review.selected_file = None;
        self.review.rows.clear();
        self.review.hunk_rows.clear();
        self.review.list_state.reset(0);
        self.review.selection_revision += 1;
        self.review.vcs = None;
        self.dispatch(
            cx,
            ClientRequest::GetSessionVcsStatus {
                session_id,
                repository_id,
            },
            move |view, response, _| {
                if view.active_session.id != session_id
                    || view.selected_repository_id != Some(repository_id)
                {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::VcsStatus(status)) => view.review.vcs = Some(status),
                    Err(error) => {
                        view.review.vcs = None;
                        view.record_status(format!("VCS review unavailable: {error}"));
                    }
                    Ok(response) => view.record_backend_error(
                        "VCS review refresh",
                        unexpected_response("VCS status", response),
                    ),
                }
            },
        );
        cx.notify();
    }

    fn detach_session_repository(&mut self, repository_id: RepositoryId, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::DetachSessionRepository {
                session_id: self.active_session.id,
                repository_id,
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::SessionRepositoryDetached) => {
                    view.session_repositories
                        .retain(|repository| repository.id != repository_id);
                    if view.selected_repository_id == Some(repository_id) {
                        view.selected_repository_id = view
                            .session_repositories
                            .first()
                            .map(|repository| repository.id);
                    }
                    view.refresh_review(cx);
                }
                Err(error) => view.record_backend_error("detach repository", error),
                Ok(response) => view.record_backend_error(
                    "detach repository",
                    unexpected_response("repository detachment", response),
                ),
            },
        );
    }

    fn detach_session_directory(&mut self, path: String, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::DetachSessionDirectory {
                session_id: self.active_session.id,
                path: path.clone(),
            },
            move |view, response, cx| match response.result {
                Ok(ServerResponse::SessionDirectoryDetached) => {
                    view.session_directories
                        .retain(|directory| directory.path != path);
                    view.session_repositories.retain(|repository| {
                        repository.path != path && !repository.path.starts_with(&format!("{path}/"))
                    });
                    if !view
                        .session_repositories
                        .iter()
                        .any(|repository| Some(repository.id) == view.selected_repository_id)
                    {
                        view.selected_repository_id = view
                            .session_repositories
                            .first()
                            .map(|repository| repository.id);
                    }
                    view.refresh_review(cx);
                }
                Err(error) => view.record_backend_error("detach directory", error),
                Ok(response) => view.record_backend_error(
                    "detach directory",
                    unexpected_response("directory detachment", response),
                ),
            },
        );
    }

    fn toggle_review_pane(&mut self, cx: &mut Context<Self>) {
        self.review.open = !self.review.open;
        if self.review.open {
            self.refresh_review(cx);
        }
        cx.notify();
    }

    pub(crate) fn close_review(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.review.open = false;
        self.project_child_review = None;
        self.session_drawer_open = false;
        cx.notify();
    }

    pub(crate) fn open_review_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.project_child_review = None;
        self.review.selected_path = Some(path.clone());
        self.review.selection_revision += 1;
        let selection_revision = self.review.selection_revision;
        self.review.selected_staged = false;
        self.review.selected_diff = None;
        self.review.rows.clear();
        self.review.hunk_rows.clear();
        self.review.list_state.reset(0);
        self.review.loading_diff = true;
        self.review.diff_error = None;
        let session_id = self.active_session.id;
        let requested_path = path.clone();
        self.dispatch(
            cx,
            ClientRequest::ReadSessionFile { session_id, path },
            move |view, response, _| {
                if view.active_session.id != session_id
                    || view.review.selected_path.as_deref() != Some(&requested_path)
                    || view.review.selected_staged
                    || view.review.selection_revision != selection_revision
                {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::SessionFilesystemFile(mut file)) => {
                        file.content = bounded_to(&file.content, MAX_REVIEW_DIFF);
                        view.review.selected_file = Some(loom_protocol::SessionFilesystemFile {
                            session_id: file.session_id,
                            path: file.path,
                            content: file.content,
                            revision: file.revision,
                        });
                        view.review.open = true;
                        view.review.panel = ReviewPanel::Changes;
                        view.review.loading_diff = false;
                    }
                    Err(error) => {
                        view.review.loading_diff = false;
                        view.review.diff_error = Some(error.to_string());
                    }
                    Ok(response) => {
                        view.review.loading_diff = false;
                        view.review.diff_error =
                            Some(unexpected_response("workspace file", response).to_string());
                    }
                }
            },
        );
    }

    fn jump_review_hunk(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.review.hunk_rows.is_empty() {
            return;
        }
        let visible_row = self.review.list_state.logical_scroll_top().item_ix;
        self.review.selected_hunk = if forward {
            self.review
                .hunk_rows
                .iter()
                .position(|row| *row > visible_row)
                .unwrap_or(self.review.hunk_rows.len() - 1)
        } else {
            self.review
                .hunk_rows
                .iter()
                .rposition(|row| *row < visible_row)
                .unwrap_or(0)
        };
        self.review.list_state.scroll_to(gpui_kit::ListOffset {
            item_ix: self.review.hunk_rows[self.review.selected_hunk],
            offset_in_item: px(0.),
        });
        cx.notify();
    }

    fn open_review_diff(&mut self, path: String, staged: bool, cx: &mut Context<Self>) {
        let Some(repository_id) = self.selected_repository_id else {
            return;
        };
        self.project_child_review = None;
        self.review.selected_path = Some(path.clone());
        self.review.selection_revision += 1;
        let selection_revision = self.review.selection_revision;
        self.review.selected_staged = staged;
        self.review.selected_file = None;
        self.review.selected_diff = None;
        self.review.rows.clear();
        self.review.hunk_rows.clear();
        self.review.list_state.reset(0);
        self.review.loading_diff = true;
        self.review.diff_error = None;
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::GetSessionVcsDiff {
                session_id,
                repository_id,
                path: Some(path.clone()),
                staged,
            },
            move |view, response, _| {
                if view.active_session.id != session_id
                    || view.review.selected_path.as_deref() != Some(&path)
                    || view.review.selected_staged != staged
                    || view.review.selection_revision != selection_revision
                {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::VcsDiff(diff)) => view.review.show_diff(diff),
                    Err(error) => {
                        view.review.loading_diff = false;
                        view.review.diff_error = Some(error.to_string());
                    }
                    Ok(response) => {
                        view.review.loading_diff = false;
                        view.review.diff_error =
                            Some(unexpected_response("VCS diff", response).to_string());
                    }
                }
            },
        );
        cx.notify();
    }

    pub(crate) fn render_session_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let projection = project_session_list_projection(
            &self.sessions,
            self.active_session.id,
            self.project_snapshot.as_ref(),
        );
        let tree_nodes = projection.tree;
        let tree_items = tree_nodes.iter().map(session_tree_item).collect::<Vec<_>>();
        let selected_session_id = self.active_session.id.to_string();
        let selected_item = find_session_tree_item(&tree_items, &selected_session_id);
        let tree = if let Some(tree) = self.session_tree.clone() {
            if self.session_tree_entries != tree_nodes {
                tree.update(cx, |state, cx| state.set_items(tree_items.clone(), cx));
                self.session_tree_entries = tree_nodes.clone();
            }
            let current_selected_id = tree
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string());
            if current_selected_id.as_deref() != Some(selected_session_id.as_str()) {
                tree.update(cx, |state, cx| state.set_selected_item(selected_item, cx));
            }
            tree
        } else {
            self.session_tree_entries = tree_nodes;
            let tree = cx.new(|cx| TreeState::new(cx).items(tree_items.clone()));
            tree.update(cx, |state, cx| state.set_selected_item(selected_item, cx));
            self.session_tree = Some(tree.clone());
            tree
        };

        let sessions = self.sessions.clone();
        let view = cx.entity();
        let menu_sessions = sessions.clone();
        let menu_view = view.clone();
        let menu_project = self.project_snapshot.clone();
        KitTree::new(&tree, move |index, entry, selected, _, app| {
            let session_id = entry.item().id.to_string();
            let Some(session) = sessions
                .iter()
                .find(|session| session.id.to_string() == session_id)
                .cloned()
            else {
                return ListItem::new(("session-tree-root", index));
            };
            let label = entry.item().label.to_string();
            let depth = entry.depth() as f32;
            let tree_indicator = if entry.is_folder() {
                if entry.is_expanded() { "⌄" } else { "›" }
            } else {
                " "
            };
            let node_indicator = view
                .read(app)
                .render_session_node_indicator(session.id, index);
            let click_view = view.clone();
            let click_session = session.clone();
            ListItem::new(("session-tree-root", index))
                .selected(selected)
                .px_2()
                .py_2()
                .text_size(gpui_kit::rems(0.8125))
                .child(
                    div()
                        .pl(px(depth * 12.))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(10.))
                                .text_color(rgb(0x8f98a6))
                                .child(tree_indicator),
                        )
                        .child(
                            Icon::new(AssetIconName::MessagesSquare)
                                .size_4()
                                .text_color(if selected {
                                    rgb(0x93c5fd)
                                } else {
                                    rgb(0x8f98a6)
                                }),
                        )
                        .child(div().flex_1().min_w(px(0.)).truncate().child(label))
                        .child(node_indicator),
                )
                .on_click(move |_, _, cx| {
                    click_view.update(cx, |this, cx| {
                        this.select_session(click_session.clone(), cx);
                    });
                })
        })
        .context_menu(move |_, entry, menu, _window, _cx| {
            let session_id = entry.item().id.to_string();
            let Some(session) = menu_sessions
                .iter()
                .find(|session| session.id.to_string() == session_id)
                .cloned()
            else {
                return menu;
            };
            let rename_view = menu_view.clone();
            let archive_view = menu_view.clone();
            let rename_session = session.clone();
            let archive_session = session.clone();
            let archive_label = menu_project
                .as_ref()
                .filter(|project| project.root_session_id == session.id)
                .map_or("Archive", |_| "Archive project");
            let mut menu = menu
                .item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                    let rename_session = rename_session.clone();
                    rename_view.update(cx, |view, cx| {
                        view.begin_session_rename(rename_session, window, cx);
                    });
                }))
                .item(PopupMenuItem::new(archive_label).on_click(move |_, _, cx| {
                    archive_view.update(cx, |view, cx| {
                        view.select_session(archive_session.clone(), cx);
                        view.archive_active(cx);
                    });
                }));
            if let Some(project) = menu_project.as_ref()
                && let Some(child) = project.agents.iter().find(|agent| {
                    agent.session_id == session.id
                        && agent.parent_session_id.is_some_and(|parent_session_id| {
                            parent_session_id == project.root_session_id
                                || project
                                    .agents
                                    .iter()
                                    .any(|manager| manager.session_id == parent_session_id)
                        })
                })
                && let Some(manager_session_id) = child.parent_session_id
                && let Some(task) = project
                    .tasks
                    .iter()
                    .find(|task| task.target_session_id == child.session_id)
            {
                let project_id = project.project_id;
                for action in project_child_control_actions(child.state, task.status) {
                    let control_view = menu_view.clone();
                    let task_id = task.task_id;
                    let action_label = if action == ProjectChildControlAction::Cancel
                        && (child.state == AgentSessionState::Failed
                            || task.status == loom_core::DelegatedTaskStatus::Failed)
                    {
                        "Cancel remaining descendants"
                    } else {
                        project_child_control_label(action)
                    };
                    menu = menu.item(PopupMenuItem::new(action_label).on_click(move |_, _, cx| {
                        control_view.update(cx, |view, cx| {
                            view.control_project_child_from_ui(
                                manager_session_id,
                                project_id,
                                task_id,
                                action,
                                cx,
                            );
                        });
                    }));
                }
                if task.code_change
                    && let Some(worktree) = project
                        .worktrees
                        .iter()
                        .find(|worktree| worktree.task_id == task.task_id)
                {
                    let task_id = task.task_id;
                    let terminal = matches!(
                        task.status,
                        loom_core::DelegatedTaskStatus::Completed
                            | loom_core::DelegatedTaskStatus::Failed
                            | loom_core::DelegatedTaskStatus::Cancelled
                    );
                    if !matches!(
                        worktree.status,
                        loom_core::ProjectWorktreeStatus::CleanupPending
                            | loom_core::ProjectWorktreeStatus::Removed
                    ) {
                        let review_view = menu_view.clone();
                        menu = menu.item(PopupMenuItem::new("Review child changes").on_click(
                            move |_, _, cx| {
                                review_view.update(cx, |view, cx| {
                                    view.review_project_child_from_ui(
                                        manager_session_id,
                                        project_id,
                                        task_id,
                                        cx,
                                    );
                                });
                            },
                        ));
                    }
                    if terminal
                        && task.status == loom_core::DelegatedTaskStatus::Completed
                        && worktree.status == loom_core::ProjectWorktreeStatus::Ready
                    {
                        let integrate_view = menu_view.clone();
                        let expected_parent_revision = worktree.base_revision.clone();
                        menu =
                            menu.item(PopupMenuItem::new("Fast-forward child changes").on_click(
                                move |_, _, cx| {
                                    integrate_view.update(cx, |view, cx| {
                                        view.integrate_project_child_from_ui(
                                            manager_session_id,
                                            project_id,
                                            task_id,
                                            expected_parent_revision.clone(),
                                            cx,
                                        );
                                    });
                                },
                            ));
                    }
                    if terminal && worktree.status != loom_core::ProjectWorktreeStatus::Removed {
                        if worktree.status != loom_core::ProjectWorktreeStatus::Retained {
                            let retain_view = menu_view.clone();
                            menu = menu.item(PopupMenuItem::new("Keep child checkout").on_click(
                                move |_, _, cx| {
                                    retain_view.update(cx, |view, cx| {
                                        view.cleanup_project_child_from_ui(
                                            manager_session_id,
                                            project_id,
                                            task_id,
                                            loom_core::ProjectWorktreeCleanupDisposition::Retain,
                                            cx,
                                        );
                                    });
                                },
                            ));
                        }
                        let cleanup_view = menu_view.clone();
                        menu =
                            menu.item(PopupMenuItem::new("Remove clean child checkout").on_click(
                                move |_, _, cx| {
                                    cleanup_view.update(cx, |view, cx| {
                                        view.cleanup_project_child_from_ui(
                                        manager_session_id,
                                        project_id,
                                        task_id,
                                        loom_core::ProjectWorktreeCleanupDisposition::RemoveClean,
                                        cx,
                                    );
                                    });
                                },
                            ));
                    }
                }
            }
            menu
        })
        .size_full()
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
                    text: tooltip_text.clone(),
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
                    .menu_width(px(320.))
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
        let mut rows = div().flex().flex_col().gap_1();
        for (activity_index, activity) in activities.iter().enumerate() {
            if !matches!(activity.data, AgentActivityData::ModelTurn { .. }) {
                rows =
                    rows.child(self.render_activity_row(activity, index, activity_index, parent));
            }
        }
        let group_id = activities[0].id;
        let status = activity_group_status(activities);
        let expanded = self.expanded_activity_groups.contains(&group_id)
            || matches!(
                status,
                AgentActivityStatus::AwaitingApproval | AgentActivityStatus::AwaitingInput
            );
        let count = activities
            .iter()
            .filter(|activity| !matches!(activity.data, AgentActivityData::ModelTurn { .. }))
            .count();
        let count_label = match count {
            0 => String::new(),
            1 => " · 1 activity".to_owned(),
            count => format!(" · {count} activities"),
        };
        let title = activity_turn_title(activities);
        let header_color = rgb(0xf3f4f6).opacity(0.72);
        let parent_for_toggle = parent.clone();
        // The first activity gives the group a stable identity as more records arrive.
        div()
            .id(("activity-section", index))
            .w_full()
            .min_w_0()
            .mx_2()
            .my_1()
            .pl_3()
            .border_l_1()
            .border_color(rgb(0x3b4555))
            .child(
                Collapsible::new()
                    .open(expanded)
                    .child(
                        Button::new(("activity-group", index))
                            .w_full()
                            .ghost()
                            .small()
                            .text_color(header_color)
                            .child(
                                div()
                                    .w_full()
                                    .text_left()
                                    .text_size(gpui_kit::rems(0.75))
                                    .text_color(header_color)
                                    .child(format!(
                                        "{} {title}{count_label} · {}",
                                        if expanded { "⌄" } else { "›" },
                                        activity_status_label(status)
                                    )),
                            )
                            .on_click(move |_, _, cx| {
                                parent_for_toggle.update(cx, |this, cx| {
                                    if !this.expanded_activity_groups.remove(&group_id) {
                                        this.expanded_activity_groups.insert(group_id);
                                    }
                                    cx.notify();
                                });
                            }),
                    )
                    .content(rows),
            )
            .into_any()
    }

    fn render_activity_row(
        &self,
        activity: &AgentActivityRecord,
        index: usize,
        activity_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let (label, detail) = activity_label(activity);
        let duration = activity.elapsed_ms.map(format_duration);
        let output = activity_output(activity);
        let is_command = matches!(activity.data, AgentActivityData::Command { .. });
        let expanded = is_command || self.expanded_activities.contains(&activity.id);
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
            .test_support()
            .flex()
            .flex_col()
            .text_xs()
            .text_color(rgb(0xcbd5e1))
            .when(!is_command, |row| {
                row.cursor_pointer().on_click(move |_, _, cx| {
                    parent_for_toggle.update(cx, |this, cx| this.toggle_activity(activity_id, cx));
                })
            })
            .child(
                session_header_title()
                    .child(
                        div()
                            .w(px(12.))
                            .text_color(status_color)
                            .child(activity_marker(activity.status)),
                    )
                    .when(!is_command, |header| {
                        header.child(if expanded { "⌄" } else { "›" })
                    })
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
                        .child(render_timeline_text(
                            format!("activity-detail-{index}-{activity_index}"),
                            detail,
                            0x94a3b8,
                        )),
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
                        .child(render_timeline_text(
                            format!("activity-output-{index}-{activity_index}"),
                            if matches!(activity.data, AgentActivityData::Command { .. }) {
                                command_output_summary(output)
                            } else {
                                bounded_to(output, 420)
                            },
                            0x8f98a6,
                        )),
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
                            parent_for_reject.update(cx, |this, cx| this.reject_pending_action(cx));
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
        row.into_any()
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
            TimelineItem::ProjectMessage(message) => {
                let kind = match message.kind {
                    loom_core::AgentMessageKind::Progress => "Progress",
                    loom_core::AgentMessageKind::Result => "Result",
                    loom_core::AgentMessageKind::Question => "Question",
                    loom_core::AgentMessageKind::Blocker => "Blocker",
                    loom_core::AgentMessageKind::Direction => "Direction",
                    loom_core::AgentMessageKind::Answer => "Answer",
                };
                let participant = |session_id: AgentSessionId| {
                    self.sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| session.name.clone())
                        .unwrap_or_else(|| session_id.to_string())
                };
                let accent = if message.kind == loom_core::AgentMessageKind::Blocker {
                    rgb(0xfbbf24)
                } else if message.kind == loom_core::AgentMessageKind::Result {
                    rgb(0x86efac)
                } else {
                    rgb(0x93c5fd)
                };
                div()
                    .mx_3()
                    .my_1()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x172033))
                    .border_1()
                    .border_color(rgb(0x334155))
                    .text_color(rgb(0xe2e8f0))
                    .child(div().text_xs().text_color(accent).child(format!(
                        "Project {kind} · {} → {} · #{}",
                        participant(message.sender_session_id),
                        participant(message.target_session_id),
                        message.project_sequence
                    )))
                    .child(render_timeline_text(
                        format!("project-message-{}", message.message_id),
                        message.body.clone(),
                        0xe2e8f0,
                    ))
                    .into_any()
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
        let title = if self.project_child_review.is_some() {
            "Project child review"
        } else {
            "Changes"
        };
        let mut body = div()
            .when(!layout.phone, |element| {
                element
                    .w(px(220.))
                    .h_full()
                    .border_r_1()
                    .border_color(rgb(0x30343f))
            })
            .when(layout.phone, |element| {
                element
                    .h(px(170.))
                    .w_full()
                    .border_b_1()
                    .border_color(rgb(0x30343f))
            })
            .id("changes-sidebar-scroll")
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_2();
        match self.review.panel {
            ReviewPanel::Changes => {
                if self.project_child_review.is_none() && self.session_repositories.len() > 1 {
                    body = body.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x93c5fd))
                            .child("REPOSITORIES"),
                    );
                    for (index, repository) in self.session_repositories.iter().enumerate() {
                        let selected = self.selected_repository_id == Some(repository.id);
                        let repository_id = repository.id;
                        let name = repository
                            .source
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .filter(|name| !name.is_empty())
                            .unwrap_or("Repository");
                        body = body.child(
                            div()
                                .id(("review-repository", index))
                                .p_1()
                                .cursor_pointer()
                                .when(selected, |element| element.bg(rgb(0x293244)))
                                .text_sm()
                                .child(name.to_owned())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_session_repository(repository_id, cx);
                                })),
                        );
                    }
                }
                if let Some(status) = &self.review.vcs {
                    body = body.child(div().mt_2().text_xs().text_color(rgb(0x93c5fd)).child(
                        if self.project_child_review.is_some() {
                            "CHILD WORKTREE CHANGES"
                        } else {
                            "REPOSITORY CHANGES"
                        },
                    ));
                    for (index, file) in status.files.iter().enumerate() {
                        let path = file.path.clone();
                        let project_child_review = self.project_child_review.is_some();
                        let staged = matches!(
                            file.worktree,
                            GitFileStatusKind::Unknown | GitFileStatusKind::Ignored
                        );
                        let selected = self.review.selected_path.as_deref() == Some(&file.path)
                            && self.review.selected_staged == staged;
                        let (additions, deletions) = if staged {
                            (file.index_additions, file.index_deletions)
                        } else {
                            (file.worktree_additions, file.worktree_deletions)
                        };
                        body = body.child(
                            div()
                                .id(("git-file", index))
                                .p_1()
                                .when(selected, |element| element.bg(rgb(0x293244)))
                                .text_sm()
                                .text_color(rgb(0xfef3c7))
                                .cursor_pointer()
                                .child(format!(
                                    "{:?}  {}  +{} −{}",
                                    if staged { file.index } else { file.worktree },
                                    file.path,
                                    additions,
                                    deletions
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !project_child_review {
                                        this.open_review_diff(path.clone(), staged, cx);
                                    }
                                })),
                        );
                        if !staged && file.index != GitFileStatusKind::Unknown {
                            let path = file.path.clone();
                            let project_child_review = self.project_child_review.is_some();
                            body = body.child(
                                div()
                                    .id(("git-staged-file", index))
                                    .p_1()
                                    .pl_3()
                                    .text_xs()
                                    .text_color(rgb(0x93c5fd))
                                    .cursor_pointer()
                                    .child(format!(
                                        "Staged  +{} −{}",
                                        file.index_additions, file.index_deletions
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if !project_child_review {
                                            this.open_review_diff(path.clone(), true, cx);
                                        }
                                    })),
                            );
                        }
                    }
                }
                let workspace_changes = if self.project_child_review.is_some() {
                    Vec::new()
                } else {
                    self.review
                        .changes
                        .iter()
                        .enumerate()
                        .filter(|(_, change)| {
                            self.review.repositories_loaded
                                && !belongs_to_repository(&change.path, &self.session_repositories)
                        })
                        .collect::<Vec<_>>()
                };
                if !workspace_changes.is_empty() {
                    body = body.child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(0x93c5fd))
                            .child("OTHER WORKSPACE FILES"),
                    );
                }
                for (index, change) in workspace_changes {
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
                if self.project_child_review.is_none() && !self.review.repositories_loaded {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("Loading repositories…"),
                    );
                } else if self.review.changes.is_empty()
                    && self
                        .review
                        .vcs
                        .as_ref()
                        .is_none_or(|status| status.files.is_empty())
                {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No changed files"),
                    );
                }
            }
        }
        let parent = cx.entity();
        let diff_list = list(self.review.list_state.clone(), move |index, _window, cx| {
            let view = parent.read(cx);
            view.render_review_row(index).into_any()
        })
        .size_full();
        let mut detail = div().flex_1().min_w(px(0.)).flex().flex_col();
        if let Some(path) = &self.review.selected_path {
            detail = detail.child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(0x30343f))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child(path.clone()))
                            .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                                if self.project_child_review.is_some() {
                                    "Project child checkout · read only"
                                } else if self.review.selected_file.is_some() {
                                    "Current file · no repository diff"
                                } else if self.review.selected_staged {
                                    "Staged changes · read only"
                                } else {
                                    "Working changes · read only"
                                },
                            )),
                    )
                    .when(!self.review.hunk_rows.is_empty(), |header| {
                        header.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Button::new("previous-review-hunk")
                                        .label("Previous hunk")
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.jump_review_hunk(false, cx)
                                        })),
                                )
                                .child(
                                    Button::new("next-review-hunk")
                                        .label("Next hunk")
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.jump_review_hunk(true, cx)
                                        })),
                                ),
                        )
                    }),
            );
        }
        if self.review.loading_diff {
            detail = detail.child(div().p_3().text_sm().child("Loading diff…"));
        } else if let Some(error) = &self.review.diff_error {
            detail = detail.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        } else if let Some(diff) = &self.review.selected_diff {
            if diff.binary {
                detail = detail.child(
                    div()
                        .p_3()
                        .text_sm()
                        .child("Binary file: no text diff is available."),
                );
            } else if self.review.rows.is_empty() {
                detail = detail.child(div().p_3().text_sm().child(if diff.truncated {
                    "The first changed line exceeds the review size limit."
                } else {
                    "No line changes in this version of the file."
                }));
            } else {
                detail = detail.child(diff_list);
            }
            if diff.truncated {
                detail =
                    detail.child(
                        div().p_2().text_xs().text_color(rgb(0xfef3c7)).child(
                            "Diff exceeds the review size limit; showing the beginning only.",
                        ),
                    );
            }
        } else if let Some(file) = &self.review.selected_file {
            detail = detail.child(
                div()
                    .flex_1()
                    .id("review-file-scroll")
                    .overflow_y_scroll()
                    .p_3()
                    .child(SelectableText::new(
                        "review-file-content",
                        file.content.clone(),
                    )),
            );
        } else {
            detail = detail.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0x8f98a6))
                    .child("Select a changed file to review its diff."),
            );
        }
        let content = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .when(layout.phone, |element| element.flex_col())
            .child(body)
            .child(detail);
        div()
            .when(layout.phone, |element| {
                element.size_full().absolute().top(px(0.)).left(px(0.))
            })
            .when(!layout.phone, |element| element.size_full())
            .flex()
            .flex_col()
            .bg(rgb(0x17191f))
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
                                        .tooltip("Show changes")
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
                                    .tooltip("Close review panel")
                                    .on_click(cx.listener(Self::close_review)),
                            ),
                    ),
            )
            .child(content)
    }

    fn render_review_row(&self, index: usize) -> gpui_kit::Div {
        let Some(row) = self.review.rows.get(index) else {
            return div().w_full().min_h(px(22.));
        };
        match row {
            ReviewRow::Hunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
            } => div()
                .w_full()
                .px_2()
                .py_1()
                .bg(rgb(0x293b56))
                .text_xs()
                .text_color(rgb(0x93c5fd))
                .child(format!(
                    "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@"
                )),
            ReviewRow::Line(line) => {
                let (marker, background, foreground) = match line.kind {
                    GitDiffLineKind::Added => ("+", 0x263d36, 0xbbf7d0),
                    GitDiffLineKind::Removed => ("−", 0x452b36, 0xfecaca),
                    GitDiffLineKind::Context => (" ", 0x17191f, 0xcbd5e1),
                };
                div()
                    .w_full()
                    .min_h(px(22.))
                    .flex()
                    .items_start()
                    .bg(rgb(background))
                    .text_xs()
                    .text_color(rgb(foreground))
                    .child(
                        div()
                            .w(px(40.))
                            .flex_shrink_0()
                            .text_color(rgb(0x8f98a6))
                            .child(line.old_line.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .w(px(40.))
                            .flex_shrink_0()
                            .text_color(rgb(0x8f98a6))
                            .child(line.new_line.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(div().w(px(18.)).flex_shrink_0().child(marker))
                    .child(div().flex_1().min_w(px(0.)).child(SelectableText::new(
                        ("review-line", index),
                        line.content.clone(),
                    )))
            }
        }
    }

    fn render_composer(
        &mut self,
        layout: ResponsiveLayout,
        window: &mut Window,
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
        let composer = self
            .composer_input
            .as_ref()
            .expect("composer input initialized before rendering");
        if self.composer_placeholder.as_deref() != Some(placeholder) {
            composer.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx)
            });
            self.composer_placeholder = Some(placeholder.to_owned());
            // Store the last presented placeholder so updating the component
            // does not keep invalidating the view on every render.
            // This is presentation state only; the text stays in InputState.
        }
        div()
            .w_full()
            .p_3()
            .bg(rgb(0x17191f))
            .border_t_1()
            .border_color(rgb(0x30343f))
            .when_some(self.context_inspection.as_ref(), |element, inspection| {
                let budget = inspection.budget.effective_input_tokens.map_or_else(
                    || "unknown budget".to_owned(),
                    |limit| format!("{limit} input tokens"),
                );
                let fallback = if inspection
                    .items
                    .iter()
                    .any(|item| item.label.contains("fallback"))
                {
                    " · model limit unknown; conservative estimate"
                } else {
                    ""
                };
                element.child(
                    div()
                        .id("context-usage")
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .mb_2()
                        .child(format!(
                            "Context ≈ {} / {budget} · {} reserved for output{fallback}",
                            inspection.included_tokens, inspection.budget.reserved_output_tokens
                        )),
                )
            })
            .child(
                div()
                    .id("session-source-dialog")
                    .w_full()
                    .min_h(px(46.))
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0x10141b))
                    .border_1()
                    .border_color(rgb(0x3b4555))
                    .text_color(rgb(0xe5e7eb))
                    .child(
                        Textarea::new(composer)
                            .aria_label(placeholder)
                            .h(px(72.))
                            .appearance(false)
                            .bordered(false),
                    ),
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
                div().mt_3().child(
                    KitInput::new(
                        self.rename_input_state
                            .as_ref()
                            .expect("rename input initialized before rendering"),
                    )
                    .id("rename-session-input")
                    .small(),
                ),
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

    fn render_source_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(dialog) = &self.source_dialog else {
            return div().into_any();
        };
        let is_start = dialog.purpose == SessionSourceDialogPurpose::StartSession;
        let selected_repo = dialog.selected_repository.as_deref();
        let repository_query = self
            .repository_filter_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let mut repository_rows = div().mt_2().flex().flex_col().gap_1();
        let mut filtered_repository_count = 0;
        for repository in &dialog.repositories {
            let searchable = format!(
                "{} {}",
                repository.full_name,
                repository.description.as_deref().unwrap_or_default()
            )
            .to_lowercase();
            if !searchable.contains(&repository_query) {
                continue;
            }
            filtered_repository_count += 1;
            let name = repository.full_name.clone();
            let selected = selected_repo == Some(name.as_str());
            repository_rows = repository_rows.child(
                div()
                    .id(format!("github-repository-{name}"))
                    .p_2()
                    .rounded_sm()
                    .bg(if selected {
                        rgb(0x263b58)
                    } else {
                        rgb(0x171c25)
                    })
                    .border_1()
                    .border_color(if selected {
                        rgb(0x2563eb)
                    } else {
                        rgb(0x293244)
                    })
                    .cursor_pointer()
                    .child(div().text_sm().child(name.clone()))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                        repository.description.clone().unwrap_or_else(|| {
                            if repository.private {
                                "Private repository"
                            } else {
                                "Public repository"
                            }
                            .to_owned()
                        }),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(dialog) = &mut this.source_dialog {
                            dialog.selected_repository = Some(name.clone());
                        }
                        cx.notify();
                    })),
            );
        }

        let mut dialog_body = div().mt_3();
        if dialog.choice == SessionSourceChoice::LocalDirectory {
            dialog_body = dialog_body
                .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                    "Attach a local folder in place. Git repositories in that folder are available for review.",
                ))
                .child(
                    div()
                        .mt_2()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(34.))
                                .px_2()
                                .rounded_sm()
                                .bg(rgb(0x0f1115))
                                .border_1()
                                .border_color(rgb(0x3b4555))
                                .child(
                                    KitInput::new(
                                        self.source_path_input
                                            .as_ref()
                                            .expect("source input initialized before rendering"),
                                    )
                                    .id("local-session-directory-path")
                                    .appearance(false)
                                    .bordered(false),
                                ),
                        )
                        .when(cfg!(not(target_family = "wasm")), |row| {
                            row.child(
                                Button::new("browse-local-session-directory")
                                    .label("Browse…")
                                    .small()
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.browse_local_directory(cx);
                                    })),
                            )
                        }),
                );
        } else if dialog.choice == SessionSourceChoice::GitHub {
            dialog_body = if dialog.repositories_loading {
                dialog_body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child("Loading repositories…"),
                )
            } else if let Some(error) = &dialog.error {
                dialog_body
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xfca5a5))
                            .child(error.clone()),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child("Connect GitHub in Settings to browse repositories."),
                    )
                    .child(
                        Button::new("connect-github-from-repository-picker")
                            .label("Open GitHub settings")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.source_dialog = None;
                                this.open_settings_from_menu(cx);
                            })),
                    )
            } else if dialog.repositories.is_empty() {
                dialog_body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x8f98a6))
                        .child("No repositories found."),
                )
            } else {
                dialog_body
                    .child(
                        KitInput::new(
                            self.repository_filter_input
                                .as_ref()
                                .expect("repository filter initialized before rendering"),
                        )
                        .id("github-repository-filter")
                        .small()
                        .into_any_element(),
                    )
                    .child(if filtered_repository_count == 0 {
                        div()
                            .mt_2()
                            .text_sm()
                            .text_color(rgb(0x8f98a6))
                            .child("No repositories match this filter.")
                            .into_any_element()
                    } else {
                        div()
                            .max_h(px(280.))
                            .overflow_y_scrollbar()
                            .child(repository_rows)
                            .into_any_element()
                    })
            };
        } else {
            dialog_body = dialog_body
                .text_sm()
                .text_color(rgb(0x8f98a6))
                .child("Start a new session with no files or repositories.");
        }
        if let Some(error) = &dialog.error
            && dialog.choice != SessionSourceChoice::GitHub
        {
            dialog_body = dialog_body.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        }

        Dialog::new(cx)
            .title(if is_start {
                "Start a session"
            } else {
                "Add to this session"
            })
            .on_close(cx.listener(|this, _, _, cx| {
                this.source_dialog = None;
                cx.notify();
            }))
            .keyboard(false)
            .overlay_closable(false)
            .w(px(560.))
            .max_h(px(600.))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child(if is_start {
                                "Choose what the new session starts with."
                            } else {
                                "Choose a repository or folder to add to the active session."
                            }),
                    )
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .gap_1()
                            .when(is_start, |row| {
                                row.child(
                                    Button::new("source-empty")
                                        .label("Empty session")
                                        .small()
                                        .when(
                                            dialog.choice == SessionSourceChoice::Empty,
                                            |button| button.primary(),
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.choose_source(SessionSourceChoice::Empty, cx)
                                        })),
                                )
                            })
                            .when(dialog.local_directory_available, |row| {
                                row.child(
                                    Button::new("source-local-directory")
                                        .label("Local folder")
                                        .small()
                                        .when(
                                            dialog.choice == SessionSourceChoice::LocalDirectory,
                                            |button| button.primary(),
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.choose_source(
                                                SessionSourceChoice::LocalDirectory,
                                                cx,
                                            )
                                        })),
                                )
                            })
                            .child(
                                Button::new("source-github")
                                    .label("GitHub repository")
                                    .small()
                                    .when(dialog.choice == SessionSourceChoice::GitHub, |button| {
                                        button.primary()
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_source(SessionSourceChoice::GitHub, cx)
                                    })),
                            ),
                    )
                    .child(dialog_body)
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .justify_end()
                            .gap_1()
                            .child(
                                Button::new("cancel-session-source")
                                    .label("Cancel")
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.source_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-session-source")
                                    .label(if is_start {
                                        "Start session"
                                    } else {
                                        "Add to session"
                                    })
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_source_dialog(cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
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
                .child("GitHub is connected. Repository browsing and the GitHub Copilot provider are available."),
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
                            .child("Connect GitHub account"),
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
                            .test_support()
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
                    .mt_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .child("Default model for new sessions"),
                    )
                    .child(body),
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
                    .mt_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .child("Parallel project agents")
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child("Maximum delegated agents running at once"),
                            ),
                    )
                    .child(
                        Button::new("project-agent-concurrency-decrease")
                            .label("-")
                            .small()
                            .disabled(
                                !self.is_connected()
                                    || self.workspace_config.project_agent_concurrency
                                        <= loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY,
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.adjust_project_agent_concurrency(-1, cx);
                            })),
                    )
                    .child(
                        div()
                            .w(px(44.))
                            .text_center()
                            .text_sm()
                            .child(self.workspace_config.project_agent_concurrency.to_string()),
                    )
                    .child(
                        Button::new("project-agent-concurrency-increase")
                            .label("+")
                            .small()
                            .disabled(
                                !self.is_connected()
                                    || self.workspace_config.project_agent_concurrency
                                        >= loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY,
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.adjust_project_agent_concurrency(1, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(rgb(0x93c5fd))
                    .child("GITHUB ACCOUNT"),
            )
            .child(
                div()
                    .mt_2()
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
                            .child(div().text_sm().child("GitHub"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if self.github_connected {
                                        rgb(0x9ad7bd)
                                    } else {
                                        rgb(0xfef3c7)
                                    })
                                    .child(if self.github_connected {
                                        "Connected"
                                    } else {
                                        "Not connected"
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child("Connect GitHub to browse and clone repositories. This also adds GitHub Copilot as a model provider. GitHub access includes read and write permissions for repositories you can access."),
                    )
                    .when(!self.github_connected && self.login_enabled, |card| {
                        card.child(
                            Button::new("connect-github-account")
                                .label("Connect GitHub")
                                .small()
                                .on_click(cx.listener(Self::toggle_github_login)),
                        )
                    }),
            )
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
                                    "No worker connected. Add one below to load your sessions.",
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
                        KitInput::new(
                            self.node_input_state
                                .as_ref()
                                .expect("worker connection input initialized before rendering"),
                        )
                        .id("worker-node-connection-input")
                        .small(),
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
            .child(
                div()
                    .mt_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .child("Font size")
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x8f98a6))
                                    .child("Relative to the system display scale"),
                            ),
                    )
                    .child(
                        Button::new("font-scale-decrease")
                            .label("−")
                            .small()
                            .disabled(self.font_scale_percent <= MIN_FONT_SCALE_PERCENT)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.adjust_font_scale(-FONT_SCALE_STEP_PERCENT, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .w(px(48.))
                            .text_center()
                            .text_sm()
                            .child(format!("{}%", self.font_scale_percent)),
                    )
                    .child(
                        Button::new("font-scale-increase")
                            .label("+")
                            .small()
                            .disabled(self.font_scale_percent >= MAX_FONT_SCALE_PERCENT)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.adjust_font_scale(FONT_SCALE_STEP_PERCENT, window, cx);
                            })),
                    )
                    .child(
                        Button::new("font-scale-reset")
                            .label("Reset")
                            .small()
                            .disabled(self.font_scale_percent == DEFAULT_FONT_SCALE_PERCENT)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_font_scale_percent(
                                    DEFAULT_FONT_SCALE_PERCENT,
                                    window,
                                    cx,
                                );
                            })),
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
                            .child("Local agent"),
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
                let api_key_configurable = provider.api_key_configurable;
                let provider_id = provider.id.clone();
                let mut provider_card = div()
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
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child(format!(
                                "{} model{} available",
                                provider.models.len(),
                                if provider.models.len() == 1 { "" } else { "s" }
                            )),
                    )
                    .child(div().mt_1().text_xs().text_color(rgb(0x8f98a6)).child(
                        if provider.credential_id.is_some() {
                            "API key configured".to_owned()
                        } else {
                            "API key not configured".to_owned()
                        },
                    ));
                if api_key_configurable {
                    if let Some(input) = self.provider_api_key_inputs.get(&provider.id) {
                        provider_card = provider_card.child(
                            div().mt_2().child(
                                KitInput::new(input)
                                    .id(format!("provider-api-key-input-{}", provider.id.as_str()))
                                    .small(),
                            ),
                        );
                    }
                    if let Some(status) = self.provider_setup_status.get(&provider.id) {
                        provider_card = provider_card.child(
                            div()
                                .id(format!("provider-setup-status-{}", provider.id.as_str()))
                                .mt_2()
                                .text_xs()
                                .text_color(rgb(0x8f98a6))
                                .child(status.clone()),
                        );
                    }
                    provider_card = provider_card.child(
                        div().mt_2().child(
                            Button::new(format!("configure-api-key-{}", provider.id.as_str()))
                                .label("Save API key")
                                .small()
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.configure_api_key_provider(
                                        provider_id.clone(),
                                        window,
                                        cx,
                                    );
                                })),
                        ),
                    );
                    if provider.credential_id.is_some() {
                        let node_id = self
                            .providers_node_id
                            .clone()
                            .unwrap_or_else(|| self.default_backend_node_id.clone());
                        provider_card = provider_card.child(
                            div()
                                .id(format!("refresh-provider-models-{}", provider.id.as_str()))
                                .mt_1()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(0x242833))
                                .hover(|style| style.bg(rgb(0x293244)))
                                .text_sm()
                                .cursor_pointer()
                                .child("Refresh models")
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.refresh_models_for_node_async(node_id.clone(), cx);
                                })),
                        );
                    }
                }
                local_body = local_body.child(provider_card);
            }
        }

        let github_card =
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
                ))
                .child(
                    div()
                        .mt_2()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("Manage GitHub authentication in Settings. Connecting GitHub also adds this model provider."),
                );

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
        &mut self,
        view: &Entity<Self>,
        layout: ResponsiveLayout,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .h_full()
            .relative()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .bg(rgb(0x17191f))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .when(layout.phone, |element| {
                        element.child(
                            Button::new("close-session-drawer")
                                .label("Close")
                                .ghost()
                                .small()
                                .tooltip("Close session drawer")
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
                    .when(!layout.phone, |row| {
                        row.child(
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
                                .child(div().flex().items_center().justify_between().child(
                                    div().text_sm().text_color(rgb(0xf3f4f6)).child("Sessions"),
                                ))
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
                        )
                    })
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
                                        div().flex().flex_col().child("No worker connected").child(
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
                            .child(div().flex_1().flex().items_center().justify_center())
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
            .when(
                !self.settings_open && !self.welcome_dialog_dismissed,
                |element| {
                    element.child(
                    Dialog::new(cx)
                        .title("You’re ready to go")
                        .on_close(cx.listener(|view, _, _, cx| {
                            view.welcome_dialog_dismissed = true;
                            cx.notify();
                        }))
                        .keyboard(false)
                        .overlay_closable(false)
                        .w(px(460.))
                        .child(div().text_sm().text_color(rgb(0x8f98a6)).child(
                            "Connect a Loom worker from Settings to load your sessions and models.",
                        ))
                        .child(
                            Button::new("disconnected-connect-worker")
                                .label("Open Settings")
                                .small()
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.open_settings_from_menu(cx);
                                })),
                        ),
                )
                },
            )
            .into_any()
    }
}

impl Render for LoomView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(px(
            BASE_FONT_SIZE * self.font_scale_percent as f32 / DEFAULT_FONT_SCALE_PERCENT as f32
        ));
        window.set_window_title(&format!("Loom - {}", self.active_session.name));
        #[cfg(target_family = "wasm")]
        if !self.browser_window_initialized {
            self.browser_window_initialized = true;
            self.observe_system_appearance(window, cx);
            self.composer_focus_handle.focus(window, cx);
            self.select_theme(ThemeChoice::System, window, cx);
        }
        if self.composer_input.is_none() {
            let input = cx.new(|cx| TextareaState::new(window, cx));
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                        view.submit_composer(cx);
                    }
                },
            ));
            self.composer_input = Some(input);
        }
        if self.clear_composer_on_render {
            if let Some(input) = self.composer_input.as_ref() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.clear_composer_on_render = false;
        }
        if self.node_input_state.is_none() {
            let initial_value = self.node_input_initial.clone();
            let input = cx.new(|cx| InputState::new(window, cx).default_value(initial_value));
            if self.settings_open {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.connect_worker_node(cx);
                    }
                },
            ));
            self.node_input_state = Some(input);
        }
        if self.clear_node_on_render {
            if let Some(input) = self.node_input_state.as_ref() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.clear_node_on_render = false;
        }
        #[cfg(target_family = "wasm")]
        if !self.connected {
            return self.render_disconnected(window, cx);
        }
        if self.rename_dialog.is_some() && self.rename_input_state.is_none() {
            let initial_value = self
                .rename_dialog
                .as_ref()
                .map(|dialog| dialog.input.clone())
                .unwrap_or_default();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(initial_value)
                    .placeholder("Session name")
            });
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.confirm_rename(cx);
                    }
                },
            ));
            self.rename_input_state = Some(input);
        }
        if self.providers_open {
            for provider in self
                .providers
                .iter()
                .filter(|provider| provider.api_key_configurable)
            {
                if !self.provider_api_key_inputs.contains_key(&provider.id) {
                    let placeholder = format!("{} API key", provider.display_name);
                    self.provider_api_key_inputs.insert(
                        provider.id.clone(),
                        cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(placeholder)
                                .masked(true)
                        }),
                    );
                }
            }
        } else {
            for input in self.provider_api_key_inputs.values() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.provider_api_key_inputs.clear();
            self.provider_setup_status.clear();
        }
        if let Some(path) = self.pending_source_path.take() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(path)
                    .placeholder("Absolute folder path")
            });
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.confirm_source_dialog(cx);
                    }
                },
            ));
            self.source_path_input = Some(input);
        }
        if self
            .source_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.choice == SessionSourceChoice::LocalDirectory)
            && self.source_path_input.is_none()
            && self.pending_source_path.is_none()
        {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Absolute folder path"));
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        view.confirm_source_dialog(cx);
                    }
                },
            ));
            self.source_path_input = Some(input);
        }
        if self.source_dialog.as_ref().is_some_and(|dialog| {
            dialog.choice == SessionSourceChoice::GitHub && dialog.filter_subscription.is_none()
        }) {
            let filter = cx.new(|cx| {
                InputState::new(window, cx).placeholder("Filter by repository name or description")
            });
            filter.update(cx, |state, cx| state.focus(window, cx));
            let subscription = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
            self.repository_filter_input = Some(filter);
            if let Some(dialog) = &mut self.source_dialog {
                dialog.filter_subscription = Some(subscription);
            }
        }
        self.schedule_worker_node_poll(cx);
        self.sync_model_select_states(window, cx);
        self.sync_agent_mode_select_state(window, cx);
        self.schedule_run_poll(cx);
        if self.project_messages_stale {
            self.refresh_project_messages(cx);
        }
        self.schedule_project_poll(cx);
        let view = cx.entity();
        let layout = responsive_layout(window.bounds().size.width);
        let review_panel_visible = review_panel_is_visible(
            layout,
            self.review.open,
            self.sessions.len(),
            self.settings_open,
            self.about_open,
            self.providers_open,
            self.github_login.is_some(),
        );
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
        let panel_layout = h_resizable("loom-workspace-panels")
            .with_handle_appearance(Rc::new(|handle, _, _| {
                let active = handle.is_active();
                let line = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(2.))
                    .w(px(1.))
                    .bg(rgb(0x30343f));
                let grip = div()
                    .w(px(5.))
                    .h(px(28.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(0x60a5fa))
                    .opacity(0.)
                    .group_hover("handle", |element| element.opacity(1.))
                    .when(active, |element| element.opacity(1.).h(px(40.)));
                Some(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .relative()
                                .w(px(5.))
                                .h_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(line)
                                .child(grip),
                        )
                        .into_any(),
                )
            }))
            .children([
                resizable_panel()
                    .size(layout.sidebar_width)
                    .size_range(px(150.)..px(420.))
                    .flex_none()
                    .visible(!layout.phone)
                    .child(div().size_full().when(!layout.phone, |element| {
                        element.child(self.render_session_sidebar(&view, layout, cx))
                    })),
                resizable_panel().min_w(px(0.)).child(
                div()
                    .flex_1()
                    .min_w(px(0.))
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
                    .child(
                        session_header_actions()
                            .child(header_tooltip("session-sources-tooltip", "Session sources",
                        Button::new("session-sources")
                            .icon(Icon::new(AssetIconName::ListTree))
                            .ghost()
                            .small()
                            .dropdown_menu({
                                let view = view.clone();
                                let repositories = self.session_repositories.clone();
                                let directories = self.session_directories.clone();
                                let selected_repository_id = self.selected_repository_id;
                                move |mut menu, window, cx| {
                                    menu = menu.label("Session sources");
                                    let add_view = view.clone();
                                    menu = menu.item(PopupMenuItem::new("Add source…").on_click(
                                        move |_, _, cx| {
                                            add_view.update(cx, |this, cx| {
                                                this.begin_source_dialog(
                                                    SessionSourceDialogPurpose::AddToSession,
                                                    cx,
                                                );
                                            });
                                        },
                                    ));
                                    if directories.is_empty() && repositories.is_empty() {
                                        menu = menu.separator().label("No sources attached");
                                    } else {
                                        menu = menu.separator();
                                    }
                                    for directory in &directories {
                                        let detach_view = view.clone();
                                        let path = directory.path.clone();
                                        let root_repository = repositories.iter().find(|repository| repository.path == directory.path);
                                        let name = Path::new(&directory.source)
                                            .file_name()
                                            .map(|name| name.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| directory.source.clone());
                                        let label = if root_repository.is_some() {
                                            format!("Git repository: {name}")
                                        } else {
                                            format!("Folder: {name}")
                                        };
                                        let source_menu = PopupMenu::build(window, cx, |mut menu, _, _| {
                                            menu = menu.label(directory.source.clone());
                                            if let Some(repository) = root_repository {
                                                let repository_id = repository.id;
                                                let select_view = view.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new("Select for review")
                                                        .checked(selected_repository_id == Some(repository_id))
                                                        .on_click(move |_, _, cx| {
                                                            select_view.update(cx, |this, cx| {
                                                                this.select_session_repository(repository_id, cx);
                                                            });
                                                        }),
                                                );
                                            }
                                            menu.item(PopupMenuItem::new("Detach source")
                                                .on_click(move |_, _, cx| {
                                                    detach_view.update(cx, |this, cx| {
                                                        this.detach_session_directory(path.clone(), cx);
                                                    });
                                                }))
                                        });
                                        menu = menu.item(PopupMenuItem::submenu(
                                            label, source_menu,
                                        ));
                                    }
                                    let other_repositories = repositories.iter().filter(|repository| {
                                        !directories.iter().any(|directory| directory.path == repository.path)
                                    }).collect::<Vec<_>>();
                                    if !other_repositories.is_empty() {
                                        menu = menu.separator().label("Git repositories");
                                        for repository in other_repositories {
                                            let repository_id = repository.id;
                                            let select_view = view.clone();
                                            let detach_view = view.clone();
                                            let source_menu = PopupMenu::build(window, cx, |menu, _, _| {
                                                menu.label(repository.source.clone()).item(
                                                    PopupMenuItem::new("Select for review")
                                                        .checked(selected_repository_id == Some(repository_id))
                                                        .on_click(move |_, _, cx| {
                                                            select_view.update(cx, |this, cx| {
                                                                this.select_session_repository(repository_id, cx);
                                                            });
                                                        }),
                                                )
                                                .item(
                                                    PopupMenuItem::new("Detach source")
                                                        .on_click(move |_, _, cx| {
                                                            detach_view.update(cx, |this, cx| {
                                                                this.detach_session_repository(repository_id, cx);
                                                            });
                                                        }),
                                                )
                                            });
                                            let name = Path::new(&repository.source)
                                                .file_name()
                                                .map(|name| name.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| repository.source.clone());
                                            menu = menu.item(PopupMenuItem::submenu(name, source_menu));
                                        }
                                    }
                                    menu
                                }
                            }),
                            ))
                            .child(header_tooltip("toggle-review-sidebar-tooltip", "Toggle side panel",
                        Button::new("toggle-review-sidebar")
                            .icon(Icon::new(if self.review.open {
                                IconName::PanelRightClose
                            } else {
                                IconName::PanelRightOpen
                            }))
                            .ghost()
                            .small()
                            .when(self.review.open, |button| button.secondary())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_review_pane(cx);
                            })),
                            )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .id("timeline-scroll")
                    .overflow_hidden()
                    .child(self.timeline_entity(cx)),
            )
            .child(self.render_composer(layout, window, cx))
            .when(self.sessions.is_empty(), |element| {
                element.child(
                    div()
                        .absolute()
                        .top(px(0.))
                        .right(px(0.))
                        .bottom(px(0.))
                        .left(px(0.))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .bg(rgb(0x111318))
                        .child(div().text_base().child("No sessions"))
                        .child(
                            Button::new("start-first-session")
                                .label("Start a session")
                                .on_click(cx.listener(Self::new_session)),
                        ),
                )
            })
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
            ),
                resizable_panel()
                    .size(layout.review_width)
                    .size_range(px(340.)..px(900.))
                    .flex_none()
                    .visible(review_panel_visible)
                    .child(div().size_full().when(review_panel_visible, |element| {
                        element.child(self.render_review(window, cx))
                    })),
            ]);
        let content = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .text_size(gpui_kit::rems(0.8125))
            // Initializes the per-frame selection registry before selectable
            // text participants prepaint and register themselves.
            .child(TextSelectionLayer)
            .when(self.source_dialog.is_some(), |element| {
                element.child(self.render_source_dialog(cx))
            })
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
                    .when(cfg!(target_os = "macos"), |element| element.pl(px(72.)))
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
                            .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                                if self.sessions.is_empty() {
                                    "No session".to_owned()
                                } else {
                                    self.active_session.name.clone()
                                },
                            )),
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
                    .child(panel_layout)
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
                                .test_support()
                                .absolute()
                                .top(px(0.))
                                .bottom(px(0.))
                                .left(px(0.))
                                .shadow_lg()
                                .child(self.render_session_sidebar(&view, layout, cx)),
                        )
                    })
                    .when(
                        layout.phone
                            && self.review.open
                            && !self.sessions.is_empty()
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
                        if self.sessions.is_empty() {
                            "No session"
                        } else {
                            session_state_label(self.session_state)
                        },
                        self.timeline.len(),
                        if self.demo_workspace { "Demo" } else { "Local" }
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
mod review_path_tests {
    use super::belongs_to_repository;
    use loom_core::{RepositoryId, Timestamp};
    use loom_protocol::SessionRepository;

    #[test]
    fn mounted_repository_activity_does_not_appear_as_other_workspace_files() {
        let repositories = ["repositories/first-id", "sources/local/second-id"]
            .into_iter()
            .map(|path| SessionRepository {
                id: RepositoryId::new(),
                source: "/code/project".to_owned(),
                path: path.to_owned(),
                revision: None,
                attached_at: Timestamp::from_unix_millis(0),
            })
            .collect::<Vec<_>>();
        assert!(belongs_to_repository(
            "repositories/first-id/src/lib.rs",
            &repositories
        ));
        assert!(belongs_to_repository(
            "sources/local/second-id/README.md",
            &repositories
        ));
        assert!(!belongs_to_repository(
            "repositories/first-id-extra/file",
            &repositories
        ));
        assert!(!belongs_to_repository("notes/todo.md", &repositories));
    }
}

#[cfg(test)]
mod session_name_tests {
    use super::{SessionCreationSource, session_name_for_path, session_name_for_source};
    use loom_protocol::GitHubRepository;
    use std::path::Path;

    #[test]
    fn local_source_uses_its_folder_name() {
        let source =
            SessionCreationSource::LocalDirectory("/home/user/work/my-project/".to_owned());
        assert_eq!(session_name_for_source(&source), "my-project");
        assert_eq!(
            session_name_for_path(Path::new("/home/user/work/my-project")),
            Some("my-project".to_owned())
        );
    }

    #[test]
    fn github_source_uses_repository_name_without_owner() {
        let source = SessionCreationSource::GitHub(GitHubRepository {
            full_name: "bearmuckle/loom".to_owned(),
            description: None,
            clone_url: "https://github.com/bearmuckle/loom.git".to_owned(),
            private: false,
            default_branch: "main".to_owned(),
        });
        assert_eq!(session_name_for_source(&source), "loom");
    }

    #[test]
    fn sources_without_a_usable_name_receive_a_safe_fallback() {
        assert_eq!(
            session_name_for_source(&SessionCreationSource::LocalDirectory("/".to_owned())),
            "New session"
        );
        assert_eq!(session_name_for_path(Path::new("/")), None);
        assert_eq!(
            session_name_for_source(&SessionCreationSource::GitHub(GitHubRepository {
                full_name: "owner/ ".to_owned(),
                description: None,
                clone_url: "https://github.com/owner/repo.git".to_owned(),
                private: false,
                default_branch: "main".to_owned(),
            })),
            "New session"
        );
    }
}

#[cfg(test)]
mod display_helper_tests {
    use super::{
        AgentActivityData, AgentActivityRecord, AgentActivityStatus, FileActivityOperation,
        activity_group_status, activity_label, activity_marker, activity_output,
        activity_turn_title, change_kind_label, command_group_title, command_line,
        command_output_summary, command_purpose, format_bytes, format_duration, format_percentage,
        is_redundant_completion_summary, run_state_label, session_state_label,
    };
    use loom_core::{ActivityId, AgentSessionState, RunId, Timestamp};
    use loom_model::{ModelId, ToolCall};
    use loom_protocol::{AgentActivityKind, AgentRunState, ToolResult};
    use serde_json::json;

    fn activity(data: AgentActivityData) -> AgentActivityRecord {
        AgentActivityRecord {
            id: ActivityId::new(),
            run_id: RunId::new(),
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ToolCall,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(0),
            completed_at: None,
            elapsed_ms: None,
            data,
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: loom_core::ToolCallId::new(),
            name: name.to_owned(),
            arguments: json!({"path":"src/lib.rs"}),
        }
    }

    #[test]
    fn resource_and_duration_labels_handle_missing_and_boundary_values() {
        assert_eq!(format_bytes(None), "n/a");
        assert_eq!(format_bytes(Some(1024)), "1.0 KiB");
        assert_eq!(format_bytes(Some(1 << 20)), "1.0 MiB");
        assert_eq!(format_bytes(Some(1 << 30)), "1.0 GiB");
        assert_eq!(format_percentage(None), "n/a");
        assert_eq!(format_percentage(Some(100)), "100%");
        assert_eq!(format_percentage(Some(101)), "n/a");
        assert_eq!(format_duration(999), "999ms");
        assert_eq!(format_duration(1_500), "1.5s");
        assert_eq!(format_duration(61_000), "1m 1s");
    }

    #[test]
    fn command_groups_describe_work_and_preserve_result_summaries() {
        let command = |program: &str, args: &[&str]| {
            activity(AgentActivityData::Command {
                call: call("run_command"),
                command: program.to_owned(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                cwd: None,
                result: None,
            })
        };
        let test = command("cargo", &["test"]);
        let fmt = command("cargo", &["fmt", "--all", "--", "--check"]);
        assert_eq!(
            command_group_title(&[fmt, test.clone()]),
            "Check formatting · Run tests"
        );
        assert_eq!(
            command_group_title(&[test.clone(), test.clone()]),
            "Run tests"
        );
        assert_eq!(
            command_group_title(&[command("/usr/bin/bash", &["-lc", "cargo test --workspace"])]),
            "Run tests"
        );
        for (program, args, expected) in [
            ("cargo", vec!["clippy"], "Check code quality"),
            ("cargo", vec!["fmt"], "Format code"),
            ("cargo", vec!["check"], "Check the build"),
            ("git", vec!["diff"], "Inspect repository changes"),
            ("rg", vec!["needle"], "Search the workspace"),
            ("cat", vec!["src/lib.rs"], "Inspect workspace files"),
            ("/usr/bin/custom", vec![], "Run custom"),
        ] {
            assert_eq!(
                command_purpose(
                    program,
                    &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>()
                ),
                expected
            );
        }
        assert_eq!(
            command_line(
                "echo",
                &["two words".to_owned(), "it's".to_owned(), "".to_owned()]
            ),
            "echo 'two words' 'it'\\''s' ''"
        );
        assert_eq!(command_output_summary("test result: ok"), "test result: ok");
        let output = format!(
            "Starting checks\n{}\nerror: test failed",
            "Compiling dependency\n".repeat(100)
        );
        let summary = command_output_summary(&output);
        assert!(summary.starts_with("Starting checks"));
        assert!(summary.ends_with("error: test failed"));
        assert!(summary.contains("output abbreviated"));
        assert!(summary.len() < 450);
        let unicode = format!("{}final result", "界".repeat(1000));
        assert!(command_output_summary(&unicode).ends_with("final result"));
        assert_eq!(
            activity_group_status(std::slice::from_ref(&test)),
            AgentActivityStatus::Completed
        );
        let failed = AgentActivityRecord {
            status: AgentActivityStatus::Failed,
            ..test.clone()
        };
        assert_eq!(
            activity_group_status(&[test.clone(), failed.clone()]),
            AgentActivityStatus::Failed
        );
        for status in [
            AgentActivityStatus::AwaitingApproval,
            AgentActivityStatus::AwaitingInput,
            AgentActivityStatus::Started,
        ] {
            assert_eq!(
                activity_group_status(&[
                    failed.clone(),
                    AgentActivityRecord {
                        status,
                        ..test.clone()
                    }
                ]),
                status
            );
        }
        assert_eq!(
            activity_group_status(&[AgentActivityRecord {
                status: AgentActivityStatus::Cancelled,
                ..test
            }]),
            AgentActivityStatus::Cancelled
        );
    }

    #[test]
    fn state_and_activity_labels_cover_every_status_and_activity_kind() {
        let session_states = [
            (AgentSessionState::Idle, "Ready"),
            (AgentSessionState::Queued, "Queued"),
            (AgentSessionState::Planning, "Planning"),
            (AgentSessionState::AwaitingApproval, "Needs approval"),
            (AgentSessionState::Paused, "Paused"),
            (AgentSessionState::Executing, "Working"),
            (AgentSessionState::Evaluating, "Reviewing"),
            (AgentSessionState::NeedsInput, "Needs your input"),
            (AgentSessionState::Completed, "Complete"),
            (AgentSessionState::Failed, "Something went wrong"),
            (AgentSessionState::Cancelled, "Cancelled"),
            (AgentSessionState::Archived, "Archived"),
        ];
        for (state, label) in session_states {
            assert_eq!(session_state_label(state), label);
        }
        assert_eq!(run_state_label(None), "Ready");
        for (state, label) in [
            (AgentRunState::Planning, "Planning"),
            (AgentRunState::Executing, "Working"),
            (AgentRunState::AwaitingApproval, "Needs approval"),
            (AgentRunState::Paused, "Paused"),
            (AgentRunState::NeedsInput, "Needs your input"),
            (AgentRunState::Evaluating, "Reviewing"),
            (AgentRunState::Completed, "Complete"),
            (AgentRunState::Failed, "Something went wrong"),
            (AgentRunState::Cancelled, "Cancelled"),
        ] {
            assert_eq!(run_state_label(Some(state)), label);
        }
        for (kind, label) in [
            (loom_workspace::WorkspaceChangeKind::Created, "New"),
            (loom_workspace::WorkspaceChangeKind::Deleted, "Removed"),
            (loom_workspace::WorkspaceChangeKind::Modified, "Updated"),
        ] {
            assert_eq!(change_kind_label(kind), label);
        }
        for (status, marker) in [
            (AgentActivityStatus::Started, "›"),
            (AgentActivityStatus::Completed, "✓"),
            (AgentActivityStatus::Failed, "×"),
            (AgentActivityStatus::AwaitingApproval, "!"),
            (AgentActivityStatus::AwaitingInput, "?"),
            (AgentActivityStatus::Cancelled, "–"),
        ] {
            assert_eq!(activity_marker(status), marker);
        }

        let model = activity(AgentActivityData::ModelTurn {
            model: ModelId::new("test-model"),
        });
        assert_eq!(
            activity_label(&model),
            ("Agent turn · test-model".to_owned(), None)
        );
        assert_eq!(activity_output(&model), None);

        let tool = activity(AgentActivityData::ToolCall {
            call: call("apply_patch"),
            result: None,
        });
        assert_eq!(activity_label(&tool).0, "apply_patch");
        assert_eq!(
            activity_turn_title(std::slice::from_ref(&tool)),
            "Making changes".to_owned()
        );

        let file = activity(AgentActivityData::File {
            call: call("read_file"),
            operation: FileActivityOperation::Read,
            path: None,
            result: None,
        });
        assert_eq!(activity_label(&file).0, "Read file");
        let listed = activity(AgentActivityData::File {
            call: call("list_files"),
            operation: FileActivityOperation::List,
            path: None,
            result: Some(ToolResult::success(
                &call("list_files"),
                "src/lib.rs".to_owned(),
            )),
        });
        assert_eq!(activity_label(&listed).0, "List files");
        assert_eq!(activity_output(&listed), Some("src/lib.rs"));
        let empty_result = activity(AgentActivityData::ToolCall {
            call: call("inspect"),
            result: Some(ToolResult::success(&call("inspect"), String::new())),
        });
        assert_eq!(activity_output(&empty_result), None);
        let tool_only = activity(AgentActivityData::ToolCall {
            call: call("inspect"),
            result: None,
        });
        assert_eq!(
            activity_turn_title(std::slice::from_ref(&tool_only)),
            "Using tools".to_owned()
        );
        assert_eq!(
            activity_turn_title(std::slice::from_ref(&file)),
            "Inspecting the workspace".to_owned()
        );

        let write = activity(AgentActivityData::File {
            call: call("write_file"),
            operation: FileActivityOperation::Write,
            path: Some("src/main.rs".to_owned()),
            result: None,
        });
        assert_eq!(
            activity_turn_title(std::slice::from_ref(&write)),
            "Making changes".to_owned()
        );

        let search = activity(AgentActivityData::Search {
            call: call("search"),
            query: "needle".to_owned(),
            path: Some("src".to_owned()),
            result: None,
        });
        assert!(activity_label(&search).1.unwrap().contains("in src"));
        assert_eq!(
            activity_turn_title(std::slice::from_ref(&search)),
            "Searching the codebase".to_owned()
        );

        let command = activity(AgentActivityData::Command {
            call: call("run"),
            command: "cargo".to_owned(),
            args: vec!["test".to_owned()],
            cwd: Some("repo".to_owned()),
            result: None,
        });
        assert_eq!(activity_label(&command).0, "cargo test");
        assert_eq!(
            activity_turn_title(std::slice::from_ref(&command)),
            "Run tests"
        );
        assert_eq!(activity_turn_title(&[]), "Working on the task");
        for data in [
            AgentActivityData::File {
                call: call("read_file"),
                operation: FileActivityOperation::Read,
                path: Some("src/lib.rs".to_owned()),
                result: Some(ToolResult::success(
                    &call("read_file"),
                    "file output".to_owned(),
                )),
            },
            AgentActivityData::Search {
                call: call("search"),
                query: "needle".to_owned(),
                path: None,
                result: Some(ToolResult::success(
                    &call("search"),
                    "search output".to_owned(),
                )),
            },
            AgentActivityData::Command {
                call: call("run"),
                command: "cargo".to_owned(),
                args: vec!["test".to_owned()],
                cwd: None,
                result: Some(ToolResult::success(
                    &call("run"),
                    "command output".to_owned(),
                )),
            },
        ] {
            assert!(activity_output(&activity(data)).is_some());
        }
        assert!(is_redundant_completion_summary(
            "Completed task: fixed the bug"
        ));
        assert!(!is_redundant_completion_summary("The task was completed"));
    }
}

#[cfg(test)]
mod session_header_render_tests {
    use super::{header_tooltip, session_header_actions, session_header_title};
    use gpui_kit::component::button::Button;
    use gpui_kit::test::{TestAppContextExt, TestWindowExt};
    use gpui_kit::{Context, TestAppContext, Window, div, prelude::*, px, size};
    use std::time::Duration;

    struct SessionHeader;

    impl Render for SessionHeader {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    session_header_title()
                        .child("A long session name that must leave room for header actions"),
                )
                .child(
                    session_header_actions()
                        .child(header_tooltip(
                            "session-sources-tooltip",
                            "Session sources",
                            Button::new("session-sources").icon(gpui_kit::component::Icon::new(
                                gpui_kit::assets::IconName::ListTree,
                            )),
                        ))
                        .child(Button::new("toggle-review-sidebar").icon(
                            gpui_kit::component::Icon::new(
                                gpui_kit::component::IconName::PanelRightOpen,
                            ),
                        )),
                )
        }
    }

    #[gpui_kit::test]
    fn header_actions_remain_visible_at_desktop_width(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(950.), px(100.)), |_, _| SessionHeader);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let mut previous_right = px(0.);
            for id in ["session-sources", "toggle-review-sidebar"] {
                let action = window.find(id);
                assert!(action.visible(), "{id} should be visible");
                assert!(action.bounds().size.width > px(0.));
                assert!(action.bounds().right() <= window.viewport_size().width);
                assert!(action.bounds().left() >= previous_right);
                previous_right = action.bounds().right();
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    async fn header_tooltip_appears_on_hover(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(950.), px(100.)), |_, _| SessionHeader);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.hover("session-sources-tooltip", cx);
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_millis(1000), |window, _| {
            window
                .try_find("loom-header-tooltip")
                .is_some_and(|tooltip| tooltip.visible())
        })
        .await;
    }
}

#[cfg(test)]
mod loom_view_render_tests {
    use super::{
        LoomView, SessionSourceChoice, SessionSourceDialog, SessionSourceDialogPurpose,
        WorkerConnectionState, WorkerNodeEntry,
    };
    use crate::state::GitHubLoginState;
    use crate::state::RenameDialogState;
    use crate::state::ReviewRow;
    use crate::state::ThemeChoice;
    use crate::state::TimelineItem;
    use gpui_kit::test::{TestAppContextExt, TestWindowExt};
    use gpui_kit::{AppContext, TestAppContext, px, size};
    use loom_core::CapabilitySet;
    use loom_core::{ActivityId, AgentSessionId, ErrorCode, RunId, Timestamp, ToolCallId};
    use loom_model::{
        ModelCapabilities, ModelDescriptor, ModelId, ProviderHealth, ProviderKind, ProviderSummary,
        ToolCall,
    };
    use loom_protocol::ToolResult;
    use loom_protocol::{
        AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus,
        ClientRequest, FileActivityOperation, GitDiff, GitDiffHunk, GitDiffLine, GitDiffLineKind,
        GitFileStatus, GitFileStatusKind, GitHubRepository, GitRepositoryStatus, RequestEnvelope,
        ServerResponse, SessionFilesystemChange, SessionFilesystemFile, SessionRepository,
        WorkerNodeResources, WorkerNodeStatus, WorkspaceChangeKind,
    };
    use std::collections::BTreeSet;
    use std::time::Duration;

    fn render_scenario(cx: &mut TestAppContext, configure: impl FnOnce(&mut LoomView)) {
        render_scenario_at(cx, size(px(1280.), px(800.)), configure);
    }

    fn render_scenario_at(
        cx: &mut TestAppContext,
        window_size: gpui_kit::Size<gpui_kit::Pixels>,
        configure: impl FnOnce(&mut LoomView),
    ) {
        let handle = cx.open_window(window_size, |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                configure(&mut view);
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn empty_session_view_renders_without_a_backend_round_trip(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |_| {});
    }

    #[gpui_kit::test]
    fn startup_rejects_credential_bearing_remote_urls_and_missing_tokens(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let options = |remote: &str, token: Option<&str>| super::UiOptions {
                project: None,
                task: "startup validation".to_owned(),
                demo: false,
                model: ModelId::new("deterministic/demo"),
                endpoint: None,
                api_key: None,
                remote: Some(remote.to_owned()),
                token: token.map(str::to_owned),
            };
            let error = match LoomView::try_new(
                &options("ws://user:secret@worker.example", Some("token")),
                cx.focus_handle(),
            ) {
                Err(error) => error,
                Ok(_) => panic!("credential-bearing URL was accepted"),
            };
            assert!(error.message.contains("must not contain credentials"));

            let error =
                match LoomView::try_new(&options("ws://worker.example", None), cx.focus_handle()) {
                    Err(error) => error,
                    Ok(_) => panic!("remote connection without a token was accepted"),
                };
            assert!(error.message.contains("require LOOM_TOKEN"));

            let error = match LoomView::try_new(
                &options("not a WebSocket URL", Some("token")),
                cx.focus_handle(),
            ) {
                Err(error) => error,
                Ok(_) => panic!("invalid remote URL was accepted"),
            };
            assert_eq!(error.code, ErrorCode::InvalidRequest);

            let invalid_workspace =
                std::env::temp_dir().join(format!("loom-ui-missing-{}", uuid::Uuid::new_v4()));
            let local_options = super::UiOptions {
                project: Some(invalid_workspace),
                task: "startup validation".to_owned(),
                demo: false,
                model: ModelId::new("deterministic/demo"),
                endpoint: None,
                api_key: None,
                remote: None,
                token: None,
            };
            let error = match LoomView::try_new(&local_options, cx.focus_handle()) {
                Err(error) => error,
                Ok(_) => panic!("missing workspace directory was accepted"),
            };
            assert_eq!(error.code, ErrorCode::WorkspaceAccessDenied);
            LoomView::new_for_test(cx.focus_handle())
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn connection_bootstrap_creates_and_attaches_a_local_workspace_session(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let _handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let options = super::UiOptions {
                project: None,
                task: "bootstrap test".to_owned(),
                demo: false,
                model: ModelId::new("deterministic/demo"),
                endpoint: None,
                api_key: None,
                remote: None,
                token: None,
            };
            let workspace_root =
                std::env::temp_dir().join(format!("loom-ui-bootstrap-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&workspace_root).unwrap();
            let local_backend = loom_server::InProcessBackend::new();
            let connection = super::ClientConnection::InProcess(Box::new(local_backend.connect()));
            let connection_after_teardown = connection.clone();
            crate::connection::negotiate(&connection).unwrap();
            let mut view = LoomView::initialize_from_connection(
                &options,
                connection,
                workspace_root.clone(),
                false,
                None,
                cx.focus_handle(),
                false,
            )
            .unwrap();
            view.owned_backend = Some(local_backend);
            assert_eq!(view.workspaces.len(), 1);
            assert_eq!(view.sessions.len(), 1);
            assert!(view.models.contains(&ModelId::new("deterministic/demo")));
            assert_eq!(view.session_directories.len(), 1);
            // Avoid starting the live worker's delayed status poll in this synchronous UI test.
            view.worker_nodes.clear();
            let _ = std::fs::remove_dir_all(workspace_root);
            view.shutdown_owned_backend();
            assert!(
                connection_after_teardown
                    .request(RequestEnvelope::new(ClientRequest::ListWorkspaces))
                    .result
                    .is_err()
            );
            view
        });
    }

    #[gpui_kit::test]
    fn session_list_renders_owner_and_resource_summary(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.active_session.name = "Test session".to_owned();
            view.sessions = vec![view.active_session.clone()];
            view.session_node_ids
                .insert(view.active_session.id, "test-node".to_owned());
            view.node_names
                .insert("test-node".to_owned(), "Local worker".to_owned());
            view.worker_nodes.push(WorkerNodeEntry {
                id: 0,
                status: WorkerNodeStatus {
                    node_id: "test-node".to_owned(),
                    name: "Local worker".to_owned(),
                    online: true,
                    capabilities: CapabilitySet::default(),
                    resources: WorkerNodeResources {
                        cpu_count: 4,
                        cpu_usage_percent: Some(45),
                        memory_usage_percent: Some(61),
                        memory_total_bytes: Some(8 * 1024 * 1024 * 1024),
                        memory_available_bytes: Some(3 * 1024 * 1024 * 1024),
                        disk_total_bytes: Some(64 * 1024 * 1024 * 1024),
                        disk_available_bytes: Some(32 * 1024 * 1024 * 1024),
                    },
                },
                is_local: true,
                url: None,
                connection: None,
                connection_state: WorkerConnectionState::Connected,
                connection_detail: None,
                severe_load_streak: 0,
            });
            view
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find(("session-tree-root", 0usize)).visible());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn run_projection_maps_messages_plan_and_completion_evidence(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.sessions = vec![view.active_session.clone()];
            view.apply_run_projection(loom_protocol::AgentRunSnapshotProjection {
                run: loom_protocol::AgentRunSnapshot {
                    id: RunId::new(),
                    attempt_id: loom_core::RunAttemptId::new(),
                    control_revision: 0,
                    session_id: view.active_session.id,
                    task: "inspect the repository".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    state: loom_protocol::AgentRunState::Completed,
                    started_at: Timestamp::from_unix_millis(1),
                    updated_at: Timestamp::from_unix_millis(2),
                    completed_at: Some(Timestamp::from_unix_millis(2)),
                    summary: Some("Reviewed the project".to_owned()),
                    evidence: vec![loom_core::EvidenceLink {
                        label: "readme".to_owned(),
                        uri: "file:///README.md".to_owned(),
                    }],
                },
                plan: vec![loom_protocol::AgentPlanStep {
                    id: "step-1".to_owned(),
                    description: "Read the project files".to_owned(),
                }],
                messages: vec![
                    loom_model::ModelMessage::new(loom_model::MessageRole::System, "system"),
                    loom_model::ModelMessage::new(loom_model::MessageRole::User, "inspect"),
                    loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, "first"),
                    loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, "second"),
                    loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, ""),
                    loom_model::ModelMessage::new(loom_model::MessageRole::Tool, "tool output"),
                ],
                pending_approval: None,
                pending_input: None,
                usage: Default::default(),
                activities: Vec::new(),
            });
        });
    }

    #[gpui_kit::test]
    fn run_projection_keeps_existing_timeline_and_suppresses_redundant_summary(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.sessions = vec![view.active_session.clone()];
            view.timeline = vec![TimelineItem::Assistant("existing transcript".to_owned())];
            view.apply_run_projection(loom_protocol::AgentRunSnapshotProjection {
                run: loom_protocol::AgentRunSnapshot {
                    id: RunId::new(),
                    attempt_id: loom_core::RunAttemptId::new(),
                    control_revision: 0,
                    session_id: view.active_session.id,
                    task: "task".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    state: loom_protocol::AgentRunState::Completed,
                    started_at: Timestamp::from_unix_millis(1),
                    updated_at: Timestamp::from_unix_millis(2),
                    completed_at: Some(Timestamp::from_unix_millis(2)),
                    summary: Some("Completed task: task".to_owned()),
                    evidence: Vec::new(),
                },
                plan: Vec::new(),
                messages: vec![loom_model::ModelMessage::new(
                    loom_model::MessageRole::User,
                    "do not duplicate",
                )],
                pending_approval: None,
                pending_input: None,
                usage: Default::default(),
                activities: Vec::new(),
            });
        });
    }

    #[gpui_kit::test]
    fn session_activation_resets_projection_and_resolves_backend_ownership(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let session_id = loom_core::AgentSessionId::new();
            let model = ModelId::new("deterministic/next");
            view.session_models.insert(session_id, model.clone());
            view.session_auto_approve_actions.insert(session_id, false);
            view.timeline
                .push(TimelineItem::Status("old status".to_owned()));
            view.pending_input = Some("old prompt".to_owned());
            view.active_run_id = Some(RunId::new());
            view.review.selected_path = Some("old.rs".to_owned());
            view.session_node_ids
                .insert(session_id, view.default_backend_node_id.clone());

            view.activate_session(loom_core::AgentSessionSnapshot {
                id: session_id,
                workspace_id: view.workspace_id,
                name: "Next session".to_owned(),
                state: loom_core::AgentSessionState::Idle,
                created_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(1),
            });

            assert_eq!(view.model, model);
            assert!(!view.auto_approve_actions);
            assert!(view.timeline.is_empty());
            assert!(view.pending_input.is_none());
            assert!(view.active_run_id.is_none());
            assert!(view.review.selected_path.is_none());
            assert!(
                view.backend_for_request(&loom_protocol::ClientRequest::GetAgentSessionSnapshot {
                    session_id,
                })
                .is_ok()
            );
            assert!(
                view.backend_for_request(&loom_protocol::ClientRequest::ListProviders)
                    .is_ok()
            );

            view.review.rows = vec![
                ReviewRow::Hunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                },
                ReviewRow::Line(GitDiffLine {
                    kind: GitDiffLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                    content: "added".to_owned(),
                }),
                ReviewRow::Line(GitDiffLine {
                    kind: GitDiffLineKind::Removed,
                    old_line: Some(1),
                    new_line: None,
                    content: "removed".to_owned(),
                }),
                ReviewRow::Line(GitDiffLine {
                    kind: GitDiffLineKind::Context,
                    old_line: Some(2),
                    new_line: Some(2),
                    content: "context".to_owned(),
                }),
            ];
            for index in 0..=view.review.rows.len() {
                let _ = view.render_review_row(index);
            }
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn settings_about_and_providers_dialogs_render(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| view.settings_open = true);
        render_scenario(cx, |view| view.about_open = true);
        render_scenario(cx, |view| view.providers_open = true);
    }

    #[gpui_kit::test]
    fn settings_dialog_close_control_handles_a_real_click(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            LoomView::new_for_test(cx.focus_handle())
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("settings-button", cx);
            window.render_frame(cx);
            assert!(
                window
                    .within("settings-dialog")
                    .find("close-settings")
                    .visible()
            );
            window
                .within("settings-dialog")
                .click("cpu-pulse-threshold-decrease", cx);
            window
                .within("settings-dialog")
                .click("cpu-pulse-threshold-increase", cx);
            window
                .within("settings-dialog")
                .click("project-agent-concurrency-decrease", cx);
            window
                .within("settings-dialog")
                .click("project-agent-concurrency-increase", cx);
            window
                .within("settings-dialog")
                .click("session-auto-approve-toggle", cx);
            window.within("settings-dialog").click("close-settings", cx);
            window.render_frame(cx);
            assert!(window.try_find("settings-dialog").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn session_source_dialog_choices_and_close_button_work(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                view.source_dialog = Some(SessionSourceDialog {
                    purpose: SessionSourceDialogPurpose::StartSession,
                    choice: SessionSourceChoice::Empty,
                    local_directory_available: true,
                    filter_subscription: None,
                    repositories: Vec::new(),
                    selected_repository: None,
                    repositories_loading: false,
                    error: None,
                });
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
            assert!(window.find("source-local-directory").visible());
            window.click("source-local-directory", cx);
            window.click("source-github", cx);
            window.click("close", cx);
            window.render_frame(cx);
            assert!(window.try_find("source-local-directory").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn session_source_and_review_actions_cover_empty_invalid_and_missing_states(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());

                view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
                view.choose_source(SessionSourceChoice::LocalDirectory, cx);
                view.confirm_source_dialog(cx);
                assert!(view.source_dialog.is_some());
                assert!(
                    view.timeline
                        .iter()
                        .any(|item| matches!(item, TimelineItem::Status(_)))
                );

                view.choose_source(SessionSourceChoice::GitHub, cx);
                view.choose_source(SessionSourceChoice::Empty, cx);
                view.confirm_source_dialog(cx);
                assert!(view.source_dialog.is_none());

                view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
                view.choose_source(SessionSourceChoice::GitHub, cx);
                view.confirm_source_dialog(cx);
                assert!(view.source_dialog.is_some());

                view.review.open = false;
                view.toggle_review_pane(cx);
                assert!(view.review.open);
                view.jump_review_hunk(true, cx);
                view.review.hunk_rows = vec![2, 5];
                view.jump_review_hunk(true, cx);
                assert_eq!(view.review.selected_hunk, 0);
                view.jump_review_hunk(false, cx);
                assert_eq!(view.review.selected_hunk, 0);
                view.open_review_diff("missing.txt".to_owned(), false, cx);
                assert!(view.review.selected_path.is_none());
                view.open_review_file("missing.txt".to_owned(), cx);
                assert_eq!(view.review.selected_path.as_deref(), Some("missing.txt"));
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn account_views_and_theme_actions_update_the_view_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.review.open = true;
            view.settings_open = true;
            view.github_login = Some(GitHubLoginState::Starting);
            view.open_about_from_menu(cx);
            assert!(view.about_open);
            assert!(!view.settings_open);
            assert!(!view.review.open);
            assert!(view.github_login.is_none());

            view.open_providers_from_menu(cx);
            assert!(view.providers_open);
            assert!(!view.about_open);
            assert_eq!(view.providers_node_id.as_deref(), Some("test-node"));

            view.observe_system_appearance(window, cx);
            view.observe_system_appearance(window, cx);
            view.select_theme(ThemeChoice::Light, window, cx);
            assert_eq!(view.theme_choice, ThemeChoice::Light);
            view.select_theme(ThemeChoice::Dark, window, cx);
            assert_eq!(view.theme_choice, ThemeChoice::Dark);
            view.select_theme(ThemeChoice::System, window, cx);
            assert_eq!(view.theme_choice, ThemeChoice::System);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn github_login_failures_update_account_state(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.handle_github_device_code(
                Err(loom_core::LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    "device failed",
                    true,
                )),
                cx,
            );
            assert!(matches!(
                view.github_login,
                Some(GitHubLoginState::Error(_))
            ));
            view.finish_github_login(
                Err(loom_core::LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    "poll failed",
                    true,
                )),
                cx,
            );
            assert!(matches!(
                view.github_login,
                Some(GitHubLoginState::Error(_))
            ));
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn worker_connection_rejects_empty_credentialed_and_duplicate_inputs(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.connect_worker_node(cx);
            view.node_input_initial = "wss://user:secret@worker.example/ws token".to_owned();
            view.connect_worker_node(cx);
            assert_eq!(view.worker_nodes.len(), 1);
            assert_eq!(
                view.worker_nodes[0].connection_state,
                WorkerConnectionState::Failed
            );
            assert!(
                view.worker_nodes[0]
                    .connection_detail
                    .as_deref()
                    .unwrap()
                    .contains("Do not include credentials")
            );
            view.connect_worker_node(cx);
            assert_eq!(view.worker_nodes.len(), 1);
            view.node_input_initial = "wss://worker-without-token.example/ws".to_owned();
            view.connect_worker_node(cx);
            assert_eq!(view.worker_nodes.len(), 2);
            assert!(
                view.worker_nodes[1]
                    .connection_detail
                    .as_deref()
                    .unwrap()
                    .contains("URL followed by its access token")
            );
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    async fn worker_connection_failure_after_valid_input_is_reported(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.node_input_initial = "ws://127.0.0.1:1/ws test-token".to_owned();
            view.connect_worker_node(cx);
            assert_eq!(view.worker_nodes.len(), 1);
            view
        });
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                let view = view.read(cx);
                view.worker_nodes.iter().any(|node| {
                    node.url.as_deref() == Some("ws://127.0.0.1:1/ws")
                        && node.connection_state == WorkerConnectionState::Failed
                        && node.connection.is_none()
                })
            })
        })
        .await;
    }

    #[gpui_kit::test]
    fn reconnect_rejects_saved_url_credentials_and_worker_can_be_removed(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.worker_nodes.push(super::connection_placeholder(
                9,
                "wss://user:secret@worker.example/ws".to_owned(),
                WorkerConnectionState::Failed,
                None,
            ));
            view.reconnect_configured_worker_nodes(cx);
            assert_eq!(
                view.worker_nodes[0].connection_state,
                WorkerConnectionState::Failed
            );
            assert!(
                view.worker_nodes[0]
                    .connection_detail
                    .as_deref()
                    .unwrap()
                    .contains("credentials")
            );
            view.remove_worker_node(9, cx);
            assert!(view.worker_nodes.is_empty());
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn command_groups_expand_keep_new_results_and_show_approvals(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let group_id = ActivityId::new();
        let run_id = RunId::new();
        let call = ToolCall {
            id: ToolCallId::new(),
            name: "run_command".to_owned(),
            arguments: serde_json::json!({"command": "cargo", "args": ["test"]}),
        };
        let record = AgentActivityRecord {
            id: group_id,
            run_id,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::Command,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(1),
            completed_at: None,
            elapsed_ms: Some(10),
            data: AgentActivityData::Command {
                call: call.clone(),
                command: "cargo".to_owned(),
                args: vec!["test".to_owned()],
                cwd: Some("repo".to_owned()),
                result: Some(ToolResult::success(&call, "test result: ok".to_owned())),
            },
        };
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view.timeline = vec![TimelineItem::ActivitySection {
                activities: vec![record.clone()],
            }];
            view
        });
        cx.update_window(handle.into(), |view, window, cx| {
            let view = view.downcast::<LoomView>().unwrap();
            window.render_frame(cx);
            assert!(window.try_find(("activity", 0u64)).is_none());
            window.click(("activity-group", 0usize), cx);
            window.render_frame(cx);
            assert!(window.try_find(("activity", 0u64)).is_some());
            view.update(cx, |view, _| {
                assert!(view.expanded_activity_groups.contains(&group_id));
                view.consume_agent_event(&loom_protocol::AgentEvent::ActivityRecorded {
                    run_id,
                    activity: AgentActivityRecord {
                        id: ActivityId::new(),
                        ..record.clone()
                    },
                });
            });
            window.render_frame(cx);
            assert!(window.try_find(("activity", 1u64)).is_some());
            window.click(("activity-group", 0usize), cx);
            window.render_frame(cx);
            assert!(window.try_find(("activity", 0u64)).is_none());
            view.update(cx, |view, cx| {
                view.pending_approval = Some(call.clone());
                view.active_run_id = Some(run_id);
                view.consume_agent_event(&loom_protocol::AgentEvent::ActivityRecorded {
                    run_id,
                    activity: AgentActivityRecord {
                        status: AgentActivityStatus::AwaitingApproval,
                        ..record.clone()
                    },
                });
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find(("approve-activity", 0u64)).is_some());
            assert!(window.try_find(("reject-activity", 0u64)).is_some());
            window.click(("approve-activity", 0u64), cx);
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn activity_rows_toggle_details_and_offer_approval_actions(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            let call = ToolCall {
                id: ToolCallId::new(),
                name: "write_file".to_owned(),
                arguments: serde_json::json!({"path": "src/lib.rs"}),
            };
            let record = AgentActivityRecord {
                id: ActivityId::new(),
                run_id: RunId::new(),
                parent_id: None,
                step_id: None,
                kind: AgentActivityKind::File,
                status: AgentActivityStatus::AwaitingApproval,
                started_at: Timestamp::from_unix_millis(1),
                completed_at: None,
                elapsed_ms: None,
                data: AgentActivityData::File {
                    call: call.clone(),
                    operation: FileActivityOperation::Write,
                    path: Some("src/lib.rs".to_owned()),
                    result: None,
                },
            };
            view.timeline = vec![TimelineItem::ActivitySection {
                activities: vec![record],
            }];
            view.active_run_id = Some(RunId::new());
            view.pending_approval = Some(call);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window
                .within(("activity-section", 0usize))
                .click(("activity", 0u64), cx);
            window.render_frame(cx);
            window.click(("approve-activity", 0u64), cx);
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn context_events_update_usage_and_only_record_compaction(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let mut inspection = loom_protocol::ContextInspection {
                items: Vec::new(), total_tokens: 200, included_tokens: 200, omitted_tokens: 0,
                budget: loom_protocol::ContextBudget::new(Some(1_000), None, 100).unwrap(),
                compacted: false, summary: None,
            };
            let run_id = RunId::new();
            view.consume_agent_event(&loom_protocol::AgentEvent::ContextInspected { run_id, inspection: inspection.clone() });
            assert!(view.timeline.is_empty());
            assert_eq!(view.context_inspection.as_ref().unwrap().included_tokens, 200);
            inspection.compacted = true;
            inspection.omitted_tokens = 120;
            inspection.included_tokens = 80;
            view.consume_agent_event(&loom_protocol::AgentEvent::ContextInspected { run_id, inspection: inspection.clone() });
            assert!(matches!(view.timeline.last(), Some(TimelineItem::Status(text)) if text.contains("Context compacted") && text.contains("lossy excerpts")));
            let count = view.timeline.len();
            inspection.compacted = false;
            view.consume_agent_event(&loom_protocol::AgentEvent::ContextInspected { run_id, inspection });
            assert_eq!(view.timeline.len(), count);
            assert_eq!(view.context_inspection.as_ref().unwrap().included_tokens, 80);
            view.reset_projection();
            assert!(view.context_inspection.is_none());
        });
    }

    #[gpui_kit::test]
    fn agent_event_projection_handles_the_run_lifecycle_and_tool_fallback(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let run_id = RunId::new();
            let call = ToolCall {
                id: ToolCallId::new(),
                name: "write_file".to_owned(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
            };
            let approval_interaction_id = loom_core::InteractionId::new();
            let input_interaction_id = loom_core::InteractionId::new();
            let snapshot = loom_protocol::AgentRunSnapshot {
                id: run_id,
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: view.active_session.id,
                task: "update the app".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Executing,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: None,
                summary: None,
                evidence: Vec::new(),
            };
            let inspection = loom_protocol::ContextInspection {
                items: Vec::new(),
                total_tokens: 0,
                included_tokens: 0,
                omitted_tokens: 0,
                budget: loom_protocol::ContextBudget {
                    context_window: None,
                    requested_input_tokens: None,
                    reserved_output_tokens: 0,
                    effective_input_tokens: None,
                },
                compacted: false,
                summary: None,
            };
            for event in [
                loom_protocol::AgentEvent::RunStarted {
                    snapshot: snapshot.clone(),
                },
                loom_protocol::AgentEvent::PlanProposed {
                    run_id,
                    plan: loom_protocol::AgentPlan {
                        steps: vec![loom_protocol::AgentPlanStep {
                            id: "edit".to_owned(),
                            description: "Edit the app".to_owned(),
                        }],
                    },
                },
                loom_protocol::AgentEvent::StepStarted {
                    run_id,
                    step_id: loom_core::StepId::new(),
                    index: 0,
                },
                loom_protocol::AgentEvent::StepCompleted {
                    run_id,
                    step_id: loom_core::StepId::new(),
                    index: 0,
                },
                loom_protocol::AgentEvent::ContextInspected { run_id, inspection },
                loom_protocol::AgentEvent::UserMessage {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 1,
                    interaction_id: Some(input_interaction_id),
                    text: "new request".to_owned(),
                },
                loom_protocol::AgentEvent::AssistantMessageDelta {
                    run_id,
                    message_id: 1,
                    text: "The change ".to_owned(),
                },
                loom_protocol::AgentEvent::AssistantMessageDelta {
                    run_id,
                    message_id: 1,
                    text: "is ready.".to_owned(),
                },
                loom_protocol::AgentEvent::ToolCallRequested {
                    run_id,
                    call: call.clone(),
                },
                loom_protocol::AgentEvent::ToolApprovalRequired {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 2,
                    interaction_id: approval_interaction_id,
                    call: call.clone(),
                },
                loom_protocol::AgentEvent::ToolPolicyEvaluated {
                    run_id,
                    call: call.clone(),
                    evaluation: loom_core::PolicyEvaluation {
                        action: loom_core::ActionKind::Write,
                        decision: loom_core::PolicyDecision::RequireApproval,
                        reason: "user approval is required".to_owned(),
                    },
                },
                loom_protocol::AgentEvent::ToolCallStarted {
                    run_id,
                    call: call.clone(),
                },
                loom_protocol::AgentEvent::ToolOutputChunk {
                    run_id,
                    tool_call_id: call.id,
                    chunk: "file updated".to_owned(),
                },
                loom_protocol::AgentEvent::ToolCallCompleted {
                    run_id,
                    result: ToolResult::success(&call, "done".to_owned()),
                },
                loom_protocol::AgentEvent::ToolApprovalDecided {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 3,
                    interaction_id: approval_interaction_id,
                    tool_call_id: call.id,
                    decision: loom_protocol::ApprovalDecision::Approved,
                },
                loom_protocol::AgentEvent::NeedsInput {
                    run_id,
                    attempt_id: snapshot.attempt_id,
                    control_revision: 4,
                    interaction_id: input_interaction_id,
                    prompt: "Which branch?".to_owned(),
                },
                loom_protocol::AgentEvent::RunUsage {
                    run_id,
                    usage: Default::default(),
                },
                loom_protocol::AgentEvent::RunUsageUpdated {
                    run_id,
                    usage: Default::default(),
                },
                loom_protocol::AgentEvent::RunLimitReached {
                    run_id,
                    status: loom_core::LimitStatus::new(
                        loom_core::SessionLimits::default(),
                        loom_core::UsageSnapshot::default(),
                    ),
                },
                loom_protocol::AgentEvent::RecoveryRequired {
                    run_id,
                    reason: "resume the session".to_owned(),
                },
                loom_protocol::AgentEvent::RunStateChanged {
                    run_id,
                    state: loom_protocol::AgentRunState::Paused,
                },
                loom_protocol::AgentEvent::ProviderError {
                    run_id,
                    error: loom_core::LoomError::new(ErrorCode::Internal, "provider failed", true),
                },
                loom_protocol::AgentEvent::ContextError {
                    run_id,
                    error: loom_core::LoomError::new(ErrorCode::Internal, "context failed", false),
                },
                loom_protocol::AgentEvent::RunCompleted {
                    snapshot: loom_protocol::AgentRunSnapshot {
                        state: loom_protocol::AgentRunState::Completed,
                        summary: Some("Finished the app update".to_owned()),
                        ..snapshot
                    },
                },
            ] {
                view.consume_agent_event(&event);
            }
            assert_eq!(
                view.run_state,
                Some(loom_protocol::AgentRunState::Completed)
            );
            assert!(view.pending_approval.is_none());
            assert_eq!(view.pending_input.as_deref(), Some("Which branch?"));
            assert!(
                view.timeline
                    .iter()
                    .any(|item| matches!(item, TimelineItem::Summary { .. }))
            );

            view.activity_records_seen = true;
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolCallRequested {
                run_id,
                call: call.clone(),
            });
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolCallStarted { run_id, call });
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolOutputChunk {
                run_id,
                tool_call_id: ToolCallId::new(),
                chunk: "suppressed fallback".to_owned(),
            });
            view.consume_agent_event(&loom_protocol::AgentEvent::ToolCallCompleted {
                run_id,
                result: ToolResult::success(
                    &ToolCall {
                        id: ToolCallId::new(),
                        name: "read_file".to_owned(),
                        arguments: serde_json::Value::Null,
                    },
                    "done".to_owned(),
                ),
            });
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn startup_session_load_restores_snapshot_and_source_lists(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.refresh_models();
            assert!(!view.default_models.is_empty());
            let workspace =
                crate::connection::create_workspace(&view.connection, "Loaded workspace").unwrap();
            let session = crate::connection::create_session_in_workspace(
                &view.connection,
                workspace.id,
                "Loaded session",
            )
            .unwrap();
            let started = view.connection.request(RequestEnvelope::new(
                ClientRequest::StartSessionAgentRun {
                    session_id: session.id,
                    task: "startup transcript page".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    system_instructions: None,
                    repository_instructions: None,
                },
            ));
            let run_id = match started.result.unwrap() {
                ServerResponse::AgentRunStarted(run) => run.id,
                response => panic!("unexpected run start response: {response:?}"),
            };
            view.workspace_id = workspace.id;
            view.workspaces.push(workspace);
            view.refresh_sessions().unwrap();
            assert_eq!(view.sessions.len(), 1);
            view.load_session(session.clone());
            assert_eq!(view.active_session.id, session.id);
            assert_eq!(view.active_session.name, "Loaded session");
            assert!(view.after_sequence.is_some());
            assert!(view.event_stream_epoch.is_some());
            assert_eq!(view.active_run_id, Some(run_id));
            assert_eq!(view.transcript_before_ordinal, Some(0));
            assert!(view.timeline.iter().any(
                |item| matches!(item, TimelineItem::User(task) if task == "startup transcript page")
            ));
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn async_session_load_falls_back_to_run_projection_and_ignores_stale_responses(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let session_id = view.active_session.id;
            let run = loom_protocol::AgentRunSnapshot {
                id: RunId::new(),
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id,
                task: "recover the transcript".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::Completed,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: Some(Timestamp::from_unix_millis(2)),
                summary: Some("Recovered run".to_owned()),
                evidence: Vec::new(),
            };
            let projection = loom_protocol::AgentRunSnapshotProjection {
                run: run.clone(),
                plan: Vec::new(),
                messages: vec![loom_model::ModelMessage::new(
                    loom_model::MessageRole::User,
                    "recover the transcript",
                )],
                pending_approval: None,
                pending_input: None,
                usage: Default::default(),
                activities: Vec::new(),
            };
            let snapshot = loom_protocol::AgentSessionSnapshotProjection {
                session: view.active_session.clone(),
                active_run: Some(projection),
                latest_sequence: loom_core::EventSequence::new(7),
                approval_policy: Default::default(),
                auto_approve_actions: false,
            };
            view.finish_async_session_load(
                session_id,
                loom_protocol::ResponseEnvelope::success(
                    loom_core::RequestId::new(),
                    loom_protocol::ServerResponse::AgentSessionSnapshot(snapshot),
                ),
                loom_protocol::ResponseEnvelope::success(
                    loom_core::RequestId::new(),
                    loom_protocol::ServerResponse::SessionEvents {
                        events: Vec::new(),
                        stream_epoch: None,
                    },
                ),
                cx,
            );
            assert_eq!(view.active_run_id, Some(run.id));
            assert!(!view.auto_approve_actions);
            assert!(view.timeline.iter().any(
                |item| matches!(item, TimelineItem::Summary { text, .. } if text == "Recovered run")
            ));

            let old_timeline_len = view.timeline.len();
            view.finish_async_session_load(
                AgentSessionId::new(),
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::Internal, "stale", false),
                ),
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::Internal, "stale", false),
                ),
                cx,
            );
            assert_eq!(view.timeline.len(), old_timeline_len);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn composer_commands_and_failed_run_responses_are_projected(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.session_node_ids
                .insert(view.active_session.id, view.default_backend_node_id.clone());
            view.submit_composer(cx);
            view.run_slash_command("/help", cx);
            view.run_slash_command("/unknown", cx);
            view.run_slash_command("/repo", cx);
            assert!(view.source_dialog.is_some());
            view.source_dialog = None;
            view.run_slash_command("/review", cx);
            assert!(view.review.open);
            view.approve_pending_action(cx);
            view.reject_pending_action(cx);

            view.model = ModelId::new("worker/uncached-model");
            view.model_catalog_node_id = None;
            view.send_message("uncached model task".to_owned(), cx);
            assert!(view.timeline.iter().any(|item| matches!(
                item,
                TimelineItem::Error { operation, error }
                    if operation == "start run" && error.message.contains("has not been refreshed")
            )));

            view.model = ModelId::new("deterministic/demo");
            view.send_message("try a task".to_owned(), cx);
            assert!(!view.sending_message);
            assert!(
                !view
                    .timeline
                    .iter()
                    .any(|item| matches!(item, TimelineItem::User(_)))
            );
            view.sending_message = true;
            view.finish_send_response(
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::ProviderUnavailable, "offline", true),
                ),
                cx,
            );
            assert!(!view.sending_message);
            view.approval_request_in_flight = true;
            view.finish_approval_response(
                loom_protocol::ResponseEnvelope::failure(
                    loom_core::RequestId::new(),
                    loom_core::LoomError::new(ErrorCode::InvalidState, "approval expired", false),
                ),
                cx,
            );
            assert!(!view.approval_request_in_flight);
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn server_event_projection_updates_session_and_ignores_service_streams(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            let snapshot = view.active_session.clone();
            let session_id = snapshot.id;
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionCreated {
                snapshot: snapshot.clone(),
            });
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionForked {
                source_session_id: session_id,
                snapshot: snapshot.clone(),
            });
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionStateChanged {
                previous: loom_core::AgentSessionState::Idle,
                current: loom_core::AgentSessionState::Executing,
            });
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionRenamed {
                session_id,
                name: "Renamed from event".to_owned(),
            });
            view.consume_event(&loom_protocol::ServerEvent::AgentSessionArchived { session_id });
            view.consume_event(&loom_protocol::ServerEvent::SessionFilesystemChanged {
                change: SessionFilesystemChange {
                    sequence: loom_core::EventSequence::new(1),
                    session_id,
                    path: "README.md".to_owned(),
                    kind: WorkspaceChangeKind::Modified,
                    revision: None,
                },
            });
            view.consume_event(&loom_protocol::ServerEvent::Terminal {
                event: loom_protocol::TerminalEventRecord {
                    sequence: loom_core::EventSequence::new(1),
                    terminal_id: loom_core::TerminalId::new(),
                    event: loom_protocol::TerminalEvent::StateChanged {
                        status: loom_protocol::TerminalStatus::Exited,
                    },
                },
            });
            view.consume_event(&loom_protocol::ServerEvent::Task {
                event: loom_protocol::TaskEventRecord {
                    sequence: loom_core::EventSequence::new(1),
                    task_id: loom_core::TaskId::new(),
                    event: loom_protocol::TaskEvent::StateChanged {
                        status: loom_protocol::TaskStatus::Completed,
                    },
                },
            });
            view.consume_event(&loom_protocol::ServerEvent::ProviderHealthChanged {
                provider_id: loom_model::ProviderId::new("test-provider"),
                health: ProviderHealth::default(),
            });
            assert_eq!(view.session_state, loom_core::AgentSessionState::Archived);
            assert!(
                view.timeline
                    .iter()
                    .any(|item| matches!(item, TimelineItem::Status(_)))
            );
            view
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    #[gpui_kit::test]
    fn settings_and_provider_dialogs_render_configured_entries(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.settings_open = true;
            view.browser_startup_error = Some("Workspace setup failed".to_owned());
            view.worker_nodes.push(WorkerNodeEntry {
                id: 1,
                status: WorkerNodeStatus {
                    node_id: "remote-worker".to_owned(),
                    name: "Remote worker".to_owned(),
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
                url: Some("wss://example.test/ws".to_owned()),
                connection: None,
                connection_state: WorkerConnectionState::Failed,
                connection_detail: Some("Connection timed out".to_owned()),
                severe_load_streak: 0,
            });
            for (id, state, online) in [
                (2, WorkerConnectionState::Disconnected, false),
                (3, WorkerConnectionState::Connecting, false),
                (4, WorkerConnectionState::Connected, true),
                (5, WorkerConnectionState::Connected, false),
            ] {
                view.worker_nodes.push(WorkerNodeEntry {
                    id,
                    status: WorkerNodeStatus {
                        node_id: format!("worker-{id}"),
                        name: format!("Worker {id}"),
                        online,
                        capabilities: CapabilitySet::default(),
                        resources: WorkerNodeResources {
                            cpu_count: 2,
                            cpu_usage_percent: Some(50),
                            memory_usage_percent: Some(75),
                            memory_total_bytes: Some(8 * 1024 * 1024),
                            memory_available_bytes: Some(2 * 1024 * 1024),
                            disk_total_bytes: None,
                            disk_available_bytes: None,
                        },
                    },
                    is_local: false,
                    url: Some(format!("wss://worker-{id}.example.test/ws")),
                    connection: None,
                    connection_state: state,
                    connection_detail: None,
                    severe_load_streak: 0,
                });
            }
        });

        render_scenario(cx, |view| {
            let local_provider_id = loom_model::ProviderId::new("company-gateway");
            let github_provider_id = loom_model::ProviderId::new("github-copilot");
            view.providers_open = true;
            view.github_connected = true;
            view.providers = vec![
                ProviderSummary {
                    id: local_provider_id.clone(),
                    kind: ProviderKind::OpenAiCompatible,
                    display_name: "Company gateway".to_owned(),
                    models: vec![ModelDescriptor {
                        id: ModelId::new("gateway/model"),
                        provider: local_provider_id,
                        display_name: "Gateway model".to_owned(),
                        context_window: Some(32_000),
                        capabilities: ModelCapabilities::default(),
                    }],
                    credential_id: Some("gateway-key".to_owned()),
                    api_key_configurable: true,
                    health: ProviderHealth::default(),
                },
                ProviderSummary {
                    id: github_provider_id,
                    kind: ProviderKind::GitHubCopilot,
                    display_name: "GitHub Copilot".to_owned(),
                    models: Vec::new(),
                    credential_id: None,
                    api_key_configurable: false,
                    health: ProviderHealth::default(),
                },
                ProviderSummary {
                    id: loom_model::ProviderId::new("empty-ollama"),
                    kind: ProviderKind::Ollama,
                    display_name: "Ollama".to_owned(),
                    models: Vec::new(),
                    credential_id: None,
                    api_key_configurable: false,
                    health: ProviderHealth::default(),
                },
            ];
        });
    }

    #[gpui_kit::test]
    fn github_login_states_and_phone_session_drawer_render(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for state in [
            GitHubLoginState::Starting,
            GitHubLoginState::Awaiting {
                verification_uri: "https://github.com/login/device".to_owned(),
                user_code: "ABCD-EFGH".to_owned(),
                expires_in: 600,
            },
            GitHubLoginState::Success,
            GitHubLoginState::Error("Unable to connect".to_owned()),
        ] {
            render_scenario(cx, |view| view.github_login = Some(state));
        }
        render_scenario_at(cx, size(px(390.), px(844.)), |view| {
            view.session_drawer_open = true;
            view.sessions = vec![view.active_session.clone()];
        });
    }

    #[gpui_kit::test]
    fn phone_drawer_and_review_sidebar_controls_toggle_panels(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(390.), px(844.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("open-session-drawer", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("mobile-session-drawer").visible());
            assert!(
                window
                    .within("mobile-session-drawer")
                    .find(("session-tree-root", 0usize))
                    .visible()
            );
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.session_drawer_open = false;
                    cx.notify();
                });
            window.render_frame(cx);
            assert!(window.try_find("mobile-session-drawer").is_none());
            window.click("toggle-review-sidebar", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("toggle-review-sidebar-close").visible());
            window.click("toggle-review-sidebar-close", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("toggle-review-sidebar-close").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    async fn session_creation_uses_worker_model_catalog_and_selects_the_created_session(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            let workspace =
                crate::connection::create_workspace(&view.connection, "Session creation").unwrap();
            view.workspace_id = workspace.id;
            view.workspaces.push(workspace);
            view
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.create_session_on_node_with_source(
                        view.default_backend_node_id.clone(),
                        "Created session".to_owned(),
                        None,
                        cx,
                    );
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).sessions.len() == 1)
        })
        .await;
    }

    #[gpui_kit::test]
    async fn asynchronous_model_refresh_updates_the_active_worker_catalog(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            view.session_node_ids
                .insert(view.active_session.id, view.default_backend_node_id.clone());
            view
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.refresh_models_for_node_async(view.default_backend_node_id.clone(), cx);
                    view.refresh_models_for_node_async("missing-node".to_owned(), cx);
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                let view = view.read(cx);
                view.model_catalog_node_id.as_deref() == Some("test-node")
                    && !view.models.is_empty()
                    && view.model_refreshes_in_flight.is_empty()
            })
        })
        .await;
    }

    #[gpui_kit::test]
    async fn session_creation_attaches_a_local_source_before_selecting_it(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let source = std::env::temp_dir().join(format!("loom-ui-source-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), "local source").unwrap();
        let source_path = source.to_string_lossy().to_string();
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            let workspace =
                crate::connection::create_workspace(&view.connection, "Local source").unwrap();
            view.workspace_id = workspace.id;
            view.workspaces.push(workspace);
            view
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.create_session_on_node_with_source(
                        view.default_backend_node_id.clone(),
                        "Source session".to_owned(),
                        Some(super::SessionCreationSource::LocalDirectory(
                            source_path.clone(),
                        )),
                        cx,
                    );
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).sessions.len() == 1)
        })
        .await;

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.refresh_review(cx);
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                let view = view.read(cx);
                view.review.repositories_loaded && view.session_directories.len() == 1
            })
        })
        .await;

        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, cx| {
                let path = format!("{}/README.md", view.session_directories[0].path);
                view.open_review_file(path, cx);
            });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                view.read(cx)
                    .review
                    .selected_file
                    .as_ref()
                    .is_some_and(|file| file.content == "local source")
            })
        })
        .await;

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.add_source_to_active_session(
                        super::SessionCreationSource::LocalDirectory(source_path.clone()),
                        cx,
                    );
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).session_directories.len() == 2)
        })
        .await;

        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, cx| {
                assert!(
                    view.session_directories[0]
                        .source
                        .contains("loom-ui-source-")
                );
                view.detach_session_directory(view.session_directories[0].path.clone(), cx);
            });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).session_directories.len() == 1)
        })
        .await;
        cx.update_window(handle.into(), |_, window, cx| {
            let view = window.root::<LoomView>().unwrap().unwrap();
            view.update(cx, |view, cx| {
                view.detach_session_directory(view.session_directories[0].path.clone(), cx);
            });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).session_directories.is_empty())
        })
        .await;
        std::fs::remove_dir_all(source).unwrap();
    }

    #[gpui_kit::test]
    async fn repository_review_loads_git_status_diff_and_detaches_the_repository(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let source = std::env::temp_dir().join(format!("loom-ui-repo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), "before\n").unwrap();
        for arguments in [
            vec!["init", "-q"],
            vec!["config", "user.email", "loom@example.test"],
            vec!["config", "user.name", "Loom Test"],
            vec!["add", "README.md"],
            vec!["commit", "-qm", "initial"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(arguments)
                    .current_dir(&source)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let source_path = source.to_string_lossy().to_string();
        let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            crate::connection::negotiate(&view.connection).unwrap();
            let workspace =
                crate::connection::create_workspace(&view.connection, "Repository review").unwrap();
            let session = crate::connection::create_session_in_workspace(
                &view.connection,
                workspace.id,
                "Review session",
            )
            .unwrap();
            let repository = crate::connection::attach_session_repository(
                &view.connection,
                session.id,
                &source_path,
                "repo",
            )
            .unwrap();
            let edit = view.connection.request(RequestEnvelope::new(
                ClientRequest::ApplySessionFilesystemEdit {
                    session_id: session.id,
                    edit: loom_workspace::WorkspaceEdit {
                        path: "repo/README.md".to_owned(),
                        old_text: "before".to_owned(),
                        new_text: "after".to_owned(),
                        expected_revision: None,
                    },
                },
            ));
            assert!(matches!(
                edit.result,
                Ok(ServerResponse::WorkspaceEditApplied(_))
            ));
            view.workspace_id = workspace.id;
            view.workspaces.push(workspace);
            view.active_session = session.clone();
            view.sessions.push(session.clone());
            view.session_node_ids
                .insert(session.id, view.default_backend_node_id.clone());
            view.session_repositories.push(repository.clone());
            view.selected_repository_id = Some(repository.id);
            view
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    let repository_id = view.selected_repository_id.unwrap();
                    view.select_session_repository(repository_id, cx);
                    view.open_review_diff("README.md".to_owned(), false, cx);
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window.root::<LoomView>().flatten().is_some_and(|view| {
                let view = view.read(cx);
                view.review.vcs.is_some()
                    && view
                        .review
                        .selected_diff
                        .as_ref()
                        .is_some_and(|diff| !diff.hunks.is_empty())
            })
        })
        .await;
        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<LoomView>()
                .unwrap()
                .unwrap()
                .update(cx, |view, cx| {
                    view.jump_review_hunk(true, cx);
                    view.jump_review_hunk(false, cx);
                    view.detach_session_repository(view.selected_repository_id.unwrap(), cx);
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<LoomView>()
                .flatten()
                .is_some_and(|view| view.read(cx).session_repositories.is_empty())
        })
        .await;
        std::fs::remove_dir_all(source).unwrap();
    }

    #[gpui_kit::test]
    async fn github_source_selection_reports_unconfigured_provider_and_requires_a_repository(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                crate::connection::negotiate(&view.connection).unwrap();
                view.session_node_ids
                    .insert(view.active_session.id, view.default_backend_node_id.clone());
                view
            });
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<gpui_kit::component::Root>()
                .unwrap()
                .unwrap()
                .update(cx, |root, cx| {
                    root.view()
                        .clone()
                        .downcast::<LoomView>()
                        .unwrap()
                        .update(cx, |view, cx| {
                            view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
                            view.choose_source(SessionSourceChoice::GitHub, cx);
                        });
                });
        })
        .unwrap();
        cx.wait_for(handle.into(), Duration::from_secs(5), |window, cx| {
            window
                .root::<gpui_kit::component::Root>()
                .flatten()
                .is_some_and(|root| {
                    root.read(cx)
                        .view()
                        .clone()
                        .downcast::<LoomView>()
                        .ok()
                        .is_some_and(|view| {
                            view.read(cx).source_dialog.as_ref().is_some_and(|dialog| {
                                !dialog.repositories_loading && dialog.error.is_some()
                            })
                        })
                })
        })
        .await;
        cx.update_window(handle.into(), |_, window, cx| {
            window
                .root::<gpui_kit::component::Root>()
                .unwrap()
                .unwrap()
                .update(cx, |root, cx| {
                    root.view()
                        .clone()
                        .downcast::<LoomView>()
                        .unwrap()
                        .update(cx, |view, cx| {
                            view.confirm_source_dialog(cx);
                            assert!(view.source_dialog.is_some());
                            assert!(view.timeline.iter().any(|item| matches!(
                        item,
                        TimelineItem::Status(status) if status == "Choose a GitHub repository"
                    )));
                        });
                });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn rename_and_source_dialogs_render(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.rename_dialog = Some(RenameDialogState {
                session: view.active_session.clone(),
                input: "Renamed session".to_owned(),
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::Empty,
                local_directory_available: true,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: None,
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::AddToSession,
                choice: SessionSourceChoice::LocalDirectory,
                local_directory_available: true,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: Some("directory does not exist".to_owned()),
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: false,
                filter_subscription: None,
                repositories: vec![GitHubRepository {
                    full_name: "owner/project".to_owned(),
                    description: Some("example repository".to_owned()),
                    clone_url: "https://github.com/owner/project.git".to_owned(),
                    private: false,
                    default_branch: "main".to_owned(),
                }],
                selected_repository: Some("owner/project".to_owned()),
                repositories_loading: false,
                error: None,
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: true,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: true,
                error: None,
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::AddToSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: false,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: Some("GitHub authentication is required".to_owned()),
            });
        });
        render_scenario(cx, |view| {
            view.source_dialog = Some(SessionSourceDialog {
                purpose: SessionSourceDialogPurpose::StartSession,
                choice: SessionSourceChoice::GitHub,
                local_directory_available: true,
                filter_subscription: None,
                repositories: Vec::new(),
                selected_repository: None,
                repositories_loading: false,
                error: None,
            });
        });
    }

    #[gpui_kit::test]
    fn review_panel_renders_workspace_and_git_changes(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.review.changes = vec![SessionFilesystemChange {
                sequence: loom_core::EventSequence::new(1),
                session_id: view.active_session.id,
                path: "src/new.rs".to_owned(),
                kind: WorkspaceChangeKind::Created,
                revision: Some("revision-1".to_owned()),
            }];
            view.review.vcs = Some(GitRepositoryStatus {
                root: "/workspace".to_owned(),
                branch: Some("main".to_owned()),
                head: Some("abc123".to_owned()),
                files: vec![GitFileStatus {
                    path: "src/lib.rs".to_owned(),
                    original_path: None,
                    index: GitFileStatusKind::Modified,
                    worktree: GitFileStatusKind::Modified,
                    conflicted: false,
                    index_additions: 1,
                    index_deletions: 0,
                    worktree_additions: 2,
                    worktree_deletions: 1,
                }],
                conflicts: Vec::new(),
                clean: false,
                captured_at: Timestamp::from_unix_millis(0),
            });
            view.review.selected_path = Some("src/lib.rs".to_owned());
            view.review.selected_diff = Some(GitDiff {
                path: Some("src/lib.rs".to_owned()),
                staged: false,
                patch: String::new(),
                binary: false,
                hunks: vec![GitDiffHunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 2,
                    lines: vec![GitDiffLine {
                        kind: GitDiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        content: "new line".to_owned(),
                    }],
                }],
                truncated: false,
            });
            view.review.rows = vec![
                ReviewRow::Hunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 2,
                },
                ReviewRow::Line(GitDiffLine {
                    kind: GitDiffLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                    content: "new line".to_owned(),
                }),
            ];
            view.review.hunk_rows = vec![0];
        });
    }

    #[gpui_kit::test]
    fn review_panel_renders_loading_file_and_binary_diff_states(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = false;
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.review.selected_path = Some("README.md".to_owned());
            view.review.selected_file = Some(SessionFilesystemFile {
                session_id: view.active_session.id,
                path: "README.md".to_owned(),
                content: "Workspace file contents".to_owned(),
                revision: "revision-2".to_owned(),
            });
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.review.selected_path = Some("assets/image.png".to_owned());
            view.review.selected_diff = Some(GitDiff {
                path: Some("assets/image.png".to_owned()),
                staged: false,
                patch: String::new(),
                binary: true,
                hunks: Vec::new(),
                truncated: false,
            });
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.review.selected_path = Some("src/large.rs".to_owned());
            view.review.selected_diff = Some(GitDiff {
                path: Some("src/large.rs".to_owned()),
                staged: false,
                patch: String::new(),
                binary: false,
                hunks: Vec::new(),
                truncated: true,
            });
        });
        render_scenario(cx, |view| {
            view.review.open = true;
            view.sessions = vec![view.active_session.clone()];
            view.review.repositories_loaded = true;
            view.session_repositories = vec![
                SessionRepository {
                    id: loom_core::RepositoryId::new(),
                    source: "https://github.com/owner/first".to_owned(),
                    path: "/workspace/first".to_owned(),
                    revision: None,
                    attached_at: Timestamp::from_unix_millis(1),
                },
                SessionRepository {
                    id: loom_core::RepositoryId::new(),
                    source: "https://github.com/owner/second/".to_owned(),
                    path: "/workspace/second".to_owned(),
                    revision: None,
                    attached_at: Timestamp::from_unix_millis(2),
                },
            ];
            view.selected_repository_id = view.session_repositories.first().map(|repo| repo.id);
            view.review.changes = vec![SessionFilesystemChange {
                sequence: loom_core::EventSequence::new(2),
                session_id: view.active_session.id,
                path: "notes/todo.md".to_owned(),
                kind: WorkspaceChangeKind::Modified,
                revision: None,
            }];
        });
    }

    #[gpui_kit::test]
    fn transcript_renders_all_message_and_activity_variants(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let call = ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "write_file".to_owned(),
            arguments: serde_json::json!({"path":"src/lib.rs"}),
        };
        let activity = AgentActivityRecord {
            id: ActivityId::new(),
            run_id: RunId::new(),
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::File,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(0),
            completed_at: None,
            elapsed_ms: Some(1500),
            data: AgentActivityData::File {
                call: call.clone(),
                operation: FileActivityOperation::Write,
                path: Some("src/lib.rs".to_owned()),
                result: Some(ToolResult::success(&call, "updated file".to_owned())),
            },
        };
        render_scenario(cx, |view| {
            view.activity_records_seen = true;
            view.expanded_activities.insert(activity.id);
            view.timeline = vec![
                TimelineItem::User("Please update the file".to_owned()),
                TimelineItem::Assistant("# Done\nThe file is updated.".to_owned()),
                TimelineItem::ActivitySection {
                    activities: vec![activity],
                },
                TimelineItem::Plan {
                    steps: vec!["Inspect".to_owned(), "Edit".to_owned()],
                    completed: BTreeSet::from([0]),
                    active: Some(1),
                },
                TimelineItem::ToolRequested {
                    name: "write_file".to_owned(),
                    arguments: "{\"path\":\"src/lib.rs\"}".to_owned(),
                },
                TimelineItem::Approval {
                    name: "write_file".to_owned(),
                    active: true,
                },
                TimelineItem::ToolStarted("write_file".to_owned()),
                TimelineItem::ToolOutput("updated file".to_owned()),
                TimelineItem::ToolCompleted {
                    name: "write_file".to_owned(),
                    success: true,
                },
                TimelineItem::ToolCompleted {
                    name: "run_tests".to_owned(),
                    success: false,
                },
                TimelineItem::Status("Working".to_owned()),
                TimelineItem::Error {
                    operation: "save".to_owned(),
                    error: loom_core::LoomError::new(ErrorCode::Persistence, "write failed", true),
                },
                TimelineItem::NeedsInput("Which branch should I use?".to_owned()),
                TimelineItem::Summary {
                    text: "Completed task: updated the file".to_owned(),
                    evidence: vec!["src/lib.rs".to_owned()],
                },
            ];
            view.pending_input = Some("Which branch should I use?".to_owned());
            view.pending_approval = Some(call);
            view.model = ModelId::new("deterministic/demo");
        });
    }
}

#[cfg(test)]
mod responsive_layout_tests {
    use super::{
        COMPACT_REVIEW_WIDTH, COMPACT_SIDEBAR_WIDTH, FULL_REVIEW_WIDTH, FULL_SIDEBAR_WIDTH,
        PHONE_SIDEBAR_WIDTH, responsive_layout, review_panel_is_visible,
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
        assert_eq!(layout.review_width, px(430.));
        let wide_layout = responsive_layout(px(1400.));
        assert_eq!(wide_layout.review_width, FULL_REVIEW_WIDTH);
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

    #[test]
    fn review_panel_visibility_is_a_pure_layout_decision() {
        let desktop = responsive_layout(px(1280.));
        assert!(review_panel_is_visible(
            desktop, true, 1, false, false, false, false
        ));
        assert!(!review_panel_is_visible(
            desktop, false, 1, false, false, false, false
        ));
        assert!(!review_panel_is_visible(
            desktop, true, 0, false, false, false, false
        ));
        for modal_open in 0..4 {
            let mut blockers = [false; 4];
            blockers[modal_open] = true;
            assert!(!review_panel_is_visible(
                desktop,
                true,
                1,
                blockers[0],
                blockers[1],
                blockers[2],
                blockers[3]
            ));
        }
        assert!(!review_panel_is_visible(
            responsive_layout(px(390.)),
            true,
            1,
            false,
            false,
            false,
            false
        ));
    }
}

#[cfg(test)]
mod worker_node_tests {
    use super::{
        ACTIVE_BACKEND_NODE_ENTRY_ID, SessionNodeIndicatorState, SessionSourceChoice,
        SessionSourceDialogPurpose, WorkerConnectionStage, WorkerConnectionState, WorkerNodeEntry,
        adjusted_cpu_pulse_threshold, adjusted_project_agent_concurrency, assigned_node_id,
        connection_placeholder, format_percentage, format_session_resource_percentages,
        format_worker_node_resources, initial_worker_nodes, local_source_available,
        mark_worker_connection_failed, merge_node_sessions, next_severe_load_streak,
        order_session_nodes, project_child_control_actions, project_session_list_projection,
        remove_worker_node_entry, safe_worker_url_label, session_id_for_request,
        session_list_projection, session_node_indicator_state, session_node_pulse,
        session_owner_status, source_choice_is_allowed, source_dialog_initial_state,
        transition_worker_connection_to_connecting, update_worker_node_status,
        validate_model_for_node, worker_connection_failure_detail, worker_node_display_name,
        worker_node_name_for_id, worker_url_embeds_credential,
    };
    use loom_core::{
        AgentSessionId, AgentSessionSnapshot, AgentSessionState, CapabilitySet, EventSequence,
        RunId, Timestamp, WorkspaceId,
    };
    use loom_core::{ErrorCode, LoomError};
    use loom_model::ModelId;
    use loom_protocol::{
        ClientRequest, ProjectChildControlAction, WorkerNodeResources, WorkerNodeStatus,
    };
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

        let input = worker_connection_failure_detail(
            WorkerConnectionStage::InputValidation,
            &LoomError::invalid_request("empty input"),
            None,
        );
        assert!(input.contains("URL followed by its access token"));
        let cancelled = worker_connection_failure_detail(
            WorkerConnectionStage::Negotiation,
            &LoomError::new(ErrorCode::RequestCancelled, "closed", false),
            None,
        );
        assert!(cancelled.contains("before negotiation completed"));
        let cancelled_status = worker_connection_failure_detail(
            WorkerConnectionStage::Status,
            &LoomError::new(ErrorCode::RequestCancelled, "closed", false),
            None,
        );
        assert!(cancelled_status.contains("before returning status"));
        let token = worker_connection_failure_detail(
            WorkerConnectionStage::Transport,
            &LoomError::invalid_request("invalid bearer token"),
            None,
        );
        assert!(token.contains("unsupported characters"));
        let timeout_text = worker_connection_failure_detail(
            WorkerConnectionStage::Negotiation,
            &LoomError::new(ErrorCode::Internal, "gateway timeout", false),
            None,
        );
        assert!(timeout_text.contains("timed out"));
        #[cfg(target_family = "wasm")]
        let bootstrap_stage = worker_connection_failure_detail(
            WorkerConnectionStage::Bootstrap,
            &LoomError::new(ErrorCode::Internal, "failed with secret", false),
            Some("secret"),
        );
        #[cfg(target_family = "wasm")]
        {
            assert!(bootstrap_stage.contains("could not open its workspace"));
            assert!(!bootstrap_stage.contains("secret"));
        }
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
        assert_eq!(
            safe_worker_url_label("user@host/path?secret=value"),
            "host/path"
        );
        assert_eq!(
            safe_worker_url_label("wss://user:pass@worker.example"),
            "wss://worker.example"
        );
        assert_eq!(
            safe_worker_url_label("wss://worker.example/ws#secret"),
            "wss://worker.example/ws"
        );
        assert!(!worker_url_embeds_credential(
            "worker.example/ws?token=hidden"
        ));
        for key in [
            "token",
            "access_token",
            "auth",
            "authorization",
            "bearer",
            "key",
            "api_key",
            "password",
            "secret",
            "client_secret",
        ] {
            assert!(
                worker_url_embeds_credential(&format!("wss://worker.example/ws?{key}=hidden")),
                "{key}"
            );
        }
        assert!(!worker_url_embeds_credential(
            "wss://worker.example/ws?theme=dark"
        ));
    }

    #[test]
    fn worker_node_fixtures_preserve_local_connection_and_hide_url_credentials() {
        let placeholder = connection_placeholder(
            7,
            "wss://user:secret@worker.example/ws?token=hidden".to_owned(),
            WorkerConnectionState::Disconnected,
            Some("offline".to_owned()),
        );
        assert_eq!(placeholder.status.name, "wss://worker.example/ws");
        assert_eq!(
            placeholder.status.node_id,
            "wss://user:secret@worker.example/ws?token=hidden"
        );
        assert!(!placeholder.status.online);
        assert_eq!(placeholder.connection_detail.as_deref(), Some("offline"));

        let local_status = node(ACTIVE_BACKEND_NODE_ENTRY_ID, true).status;
        let local_connection = super::ClientConnection::InProcess(Box::new(
            loom_server::InProcessBackend::new().connect(),
        ));
        let config = loom_protocol::WorkspaceConfig {
            worker_nodes: vec![
                loom_protocol::WorkerNodeConfig {
                    url: "ws://local-worker/ws".to_owned(),
                },
                loom_protocol::WorkerNodeConfig {
                    url: "wss://remote-worker/ws".to_owned(),
                },
            ],
            ..loom_protocol::WorkspaceConfig::default()
        };
        let nodes = initial_worker_nodes(
            local_status,
            local_connection,
            &config,
            Some("ws://local-worker/ws"),
        );
        assert_eq!(nodes.len(), 2);
        assert!(nodes[0].is_local);
        assert_eq!(nodes[0].connection_state, WorkerConnectionState::Connected);
        assert_eq!(nodes[1].id, 1);
        assert_eq!(nodes[1].url.as_deref(), Some("wss://remote-worker/ws"));
        assert_eq!(
            nodes[1].connection_state,
            WorkerConnectionState::Disconnected
        );
        assert!(nodes[1].connection.is_none());
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn failed_connection_state_clears_transport_and_keeps_node_url() {
        let mut node = node(7, false);
        let backend = loom_server::InProcessBackend::new();
        node.connection = Some(super::ClientConnection::InProcess(Box::new(
            backend.connect(),
        )));
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
            workspace_id: WorkspaceId::new(),
            name: name.to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(1),
        }
    }

    #[test]
    fn session_list_projection_preserves_order_and_selects_active_session() {
        let first = AgentSessionId::new();
        let active = AgentSessionId::new();
        let sessions = vec![session(first, "First"), session(active, "Active")];

        let projection = session_list_projection(&sessions, active);

        assert_eq!(
            projection.entries,
            vec![(first, "First".to_owned()), (active, "Active".to_owned())]
        );
        assert_eq!(projection.selected_index, Some(1));
    }

    #[test]
    fn session_list_projection_has_no_selection_when_active_session_is_missing() {
        let sessions = vec![session(AgentSessionId::new(), "Only session")];

        let projection = session_list_projection(&sessions, AgentSessionId::new());

        assert_eq!(projection.selected_index, None);
    }

    #[test]
    fn project_session_list_groups_direct_children_and_selects_them() {
        let root_id = AgentSessionId::new();
        let child_id = AgentSessionId::new();
        let other_id = AgentSessionId::new();
        let sessions = vec![
            session(root_id, "Project"),
            session(child_id, "Researcher"),
            session(other_id, "Other session"),
        ];
        let project_id = loom_core::ProjectId::from_uuid(*root_id.as_uuid());
        let project = loom_core::ProjectSnapshot {
            project_id,
            root_session_id: root_id,
            agents: vec![
                loom_core::ProjectAgentRecord {
                    session_id: root_id,
                    project_id,
                    parent_session_id: None,
                    depth: 1,
                    state: AgentSessionState::Idle,
                    task_summary: None,
                    output_cursor: EventSequence::default(),
                    updated_at: Timestamp::from_unix_millis(1),
                },
                loom_core::ProjectAgentRecord {
                    session_id: child_id,
                    project_id,
                    parent_session_id: Some(root_id),
                    depth: 2,
                    state: AgentSessionState::Executing,
                    task_summary: Some("Review protocol changes".to_owned()),
                    output_cursor: EventSequence::default(),
                    updated_at: Timestamp::from_unix_millis(2),
                },
            ],
            tasks: vec![],
            worktrees: vec![],
        };

        let projection = project_session_list_projection(&sessions, child_id, Some(&project));

        assert_eq!(
            projection.entries,
            vec![
                (root_id, "Project · Project".to_owned()),
                (
                    child_id,
                    "↳ Researcher · Working — Review protocol changes".to_owned()
                ),
                (other_id, "Other session".to_owned()),
            ]
        );
        assert_eq!(projection.selected_index, Some(1));
    }

    #[test]
    fn project_child_controls_follow_run_and_task_state() {
        use AgentSessionState as SessionState;
        use ProjectChildControlAction as Action;
        use loom_core::DelegatedTaskStatus as TaskStatus;

        assert_eq!(
            project_child_control_actions(SessionState::Executing, TaskStatus::Running),
            vec![Action::Pause, Action::Interrupt, Action::Cancel]
        );
        assert_eq!(
            project_child_control_actions(SessionState::Paused, TaskStatus::Blocked),
            vec![Action::Continue, Action::Cancel]
        );
        assert_eq!(
            project_child_control_actions(SessionState::Queued, TaskStatus::Queued),
            vec![Action::Continue, Action::Cancel]
        );
        assert_eq!(
            project_child_control_actions(SessionState::Failed, TaskStatus::Failed),
            vec![Action::RetryFailedStep, Action::Cancel]
        );
        assert!(
            project_child_control_actions(SessionState::Completed, TaskStatus::Completed)
                .is_empty()
        );
    }

    #[test]
    fn project_worktree_updates_are_included_in_project_workspace_refreshes() {
        let project_id = loom_core::ProjectId::new();
        let root_session_id = AgentSessionId::new();
        let worktree = loom_core::ProjectWorktreeRecord {
            project_id,
            task_id: loom_core::TaskId::new(),
            parent_session_id: root_session_id,
            child_session_id: AgentSessionId::new(),
            parent_repository_id: loom_core::RepositoryId::new(),
            child_repository_id: loom_core::RepositoryId::new(),
            relative_path: "project-worktrees/example".to_owned(),
            worktree_name: "loom-child-example".to_owned(),
            branch_name: "loom/project-child-example".to_owned(),
            base_revision: "base".to_owned(),
            result_revision: None,
            integrated_revision: None,
            status: loom_core::ProjectWorktreeStatus::Ready,
            conflict_paths: Vec::new(),
            error: None,
            cleanup_disposition: None,
            created_at: loom_core::Timestamp::from_unix_millis(1),
            updated_at: loom_core::Timestamp::from_unix_millis(1),
        };
        let event =
            loom_protocol::WorkspaceFeedEvent::Session(loom_protocol::ServerEventEnvelope {
                protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(3),
                session_id: root_session_id,
                event: loom_protocol::ServerEvent::ProjectChildWorktreeUpdated { worktree },
            });
        let members = std::collections::BTreeSet::from([root_session_id]);

        assert!(super::is_project_workspace_event(
            &event, project_id, &members
        ));
        assert!(!super::is_project_workspace_event(
            &event,
            loom_core::ProjectId::new(),
            &members
        ));
    }

    #[test]
    fn source_dialog_initial_choice_tracks_purpose_and_local_availability() {
        assert_eq!(
            source_dialog_initial_state(SessionSourceDialogPurpose::StartSession, true),
            SessionSourceChoice::Empty
        );
        assert_eq!(
            source_dialog_initial_state(SessionSourceDialogPurpose::AddToSession, true),
            SessionSourceChoice::LocalDirectory
        );
        assert_eq!(
            source_dialog_initial_state(SessionSourceDialogPurpose::AddToSession, false),
            SessionSourceChoice::GitHub
        );
    }

    #[test]
    fn local_source_availability_respects_backend_and_session_ownership() {
        use SessionSourceDialogPurpose::{AddToSession, StartSession};

        assert!(local_source_available(
            StartSession,
            true,
            Some("remote"),
            "local"
        ));
        assert!(!local_source_available(StartSession, false, None, "local"));
        assert!(local_source_available(AddToSession, true, None, "local"));
        assert!(local_source_available(
            AddToSession,
            true,
            Some("local"),
            "local"
        ));
        assert!(!local_source_available(
            AddToSession,
            true,
            Some("remote"),
            "local"
        ));
        assert!(!local_source_available(
            AddToSession,
            false,
            Some("local"),
            "local"
        ));
    }

    #[test]
    fn source_choice_validation_rejects_empty_additions_and_unavailable_local_sources() {
        use SessionSourceChoice::{Empty, GitHub, LocalDirectory};
        use SessionSourceDialogPurpose::{AddToSession, StartSession};

        assert!(source_choice_is_allowed(StartSession, false, Empty));
        assert!(!source_choice_is_allowed(AddToSession, true, Empty));
        assert!(source_choice_is_allowed(AddToSession, true, LocalDirectory));
        assert!(!source_choice_is_allowed(
            AddToSession,
            false,
            LocalDirectory
        ));
        assert!(source_choice_is_allowed(AddToSession, false, GitHub));
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
        assert!(remove_worker_node_entry(&mut nodes, 999).is_none());
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
        assert!(
            format_worker_node_resources(&WorkerNodeResources {
                cpu_count: 0,
                cpu_usage_percent: None,
                memory_usage_percent: None,
                memory_total_bytes: None,
                memory_available_bytes: None,
                disk_total_bytes: None,
                disk_available_bytes: None,
            })
            .starts_with("CPU n/a · RAM")
        );
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
        assert_eq!(
            update_worker_node_status(&mut nodes, 1, refreshed.clone()),
            None
        );

        let summary = format_worker_node_resources(&nodes[0].status.resources);
        assert!(summary.contains("CPU 25% of 4 cores · RAM 50% of 8.0 GiB"));
        assert!(summary.contains("disk 50.0 GiB available"));
        assert_eq!(update_worker_node_status(&mut nodes, 999, refreshed), None);
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
                    attempt_id: loom_core::RunAttemptId::new(),
                    expected_control_revision: 0,
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
            session_id_for_request(&ClientRequest::ListWorkspaces, active_session_id),
            None
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::CreateSessionCheckpoint {
                    session_id: active_session_id,
                    label: "checkpoint".to_owned(),
                },
                AgentSessionId::new()
            ),
            Some(active_session_id)
        );
    }

    #[test]
    fn session_request_routing_covers_explicit_active_and_non_session_requests() {
        let session = AgentSessionId::new();
        let active = AgentSessionId::new();
        let repository = loom_core::RepositoryId::new();
        let terminal = loom_core::TerminalId::new();
        let task = loom_core::TaskId::new();
        let checkpoint = loom_core::CheckpointId::new();
        let explicit_requests = vec![
            ClientRequest::GetAgentSession {
                session_id: session,
            },
            ClientRequest::GetAgentSessionSnapshot {
                session_id: session,
            },
            ClientRequest::RenameAgentSession {
                session_id: session,
                name: "renamed".into(),
            },
            ClientRequest::ArchiveAgentSession {
                session_id: session,
            },
            ClientRequest::GetRecentSessionEvents {
                session_id: session,
                limit: 5,
            },
            ClientRequest::StartSessionAgentRun {
                session_id: session,
                task: "task".into(),
                model: ModelId::new("model"),
                system_instructions: None,
                repository_instructions: None,
            },
            ClientRequest::StartSessionAgentRunWithOptions {
                session_id: session,
                task: "task".into(),
                model: ModelId::new("model"),
                system_instructions: None,
                repository_instructions: None,
                limits: loom_core::SessionLimits::default(),
                context: loom_protocol::ContextAssemblyOptions::default(),
            },
            ClientRequest::AttachSessionRepository {
                session_id: session,
                source: "/repo".into(),
                path: "repo".into(),
                revision: None,
            },
            ClientRequest::AttachSessionDirectory {
                session_id: session,
                source: "/folder".into(),
                path: "folder".into(),
            },
            ClientRequest::ListSessionDirectories {
                session_id: session,
            },
            ClientRequest::DetachSessionDirectory {
                session_id: session,
                path: "folder".into(),
            },
            ClientRequest::ListSessionRepositories {
                session_id: session,
            },
            ClientRequest::DetachSessionRepository {
                session_id: session,
                repository_id: repository,
            },
            ClientRequest::GetSessionFilesystemSnapshot {
                session_id: session,
            },
            ClientRequest::GetSessionFilesystemChanges {
                session_id: session,
                after_sequence: None,
            },
            ClientRequest::ReadSessionFile {
                session_id: session,
                path: "file".into(),
            },
            ClientRequest::ApplySessionFilesystemEdit {
                session_id: session,
                edit: loom_protocol::WorkspaceEdit {
                    path: "file".into(),
                    old_text: "old".into(),
                    new_text: "new".into(),
                    expected_revision: None,
                },
            },
            ClientRequest::TakeSessionFilesystemControl {
                session_id: session,
                control: loom_protocol::WorkspaceControl::Agent,
            },
            ClientRequest::CreateSessionCheckpoint {
                session_id: session,
                label: "checkpoint".into(),
            },
            ClientRequest::RevertSessionCheckpoint {
                session_id: session,
                checkpoint_id: checkpoint,
            },
            ClientRequest::UndoSessionEdit {
                session_id: session,
            },
            ClientRequest::GetSessionContextFiles {
                session_id: session,
            },
            ClientRequest::GetSessionVcsStatus {
                session_id: session,
                repository_id: repository,
            },
            ClientRequest::GetSessionVcsDiff {
                session_id: session,
                repository_id: repository,
                path: None,
                staged: false,
            },
            ClientRequest::GetSessionVcsBranches {
                session_id: session,
                repository_id: repository,
            },
            ClientRequest::GetSessionVcsConflicts {
                session_id: session,
                repository_id: repository,
            },
            ClientRequest::OpenSessionTerminal {
                session_id: session,
                command: "sh".into(),
                args: Vec::new(),
                cwd: None,
            },
            ClientRequest::WriteSessionTerminalInput {
                session_id: session,
                terminal_id: terminal,
                input: "exit".into(),
            },
            ClientRequest::ResizeSessionTerminal {
                session_id: session,
                terminal_id: terminal,
                rows: 24,
                columns: 80,
            },
            ClientRequest::GetSessionTerminalEvents {
                session_id: session,
                terminal_id: terminal,
                after_sequence: None,
            },
            ClientRequest::CancelSessionTerminal {
                session_id: session,
                terminal_id: terminal,
            },
            ClientRequest::StartSessionTask {
                session_id: session,
                spec: loom_protocol::TaskSpec {
                    kind: loom_protocol::TaskKind::Test,
                    label: "test".into(),
                    command: "cargo".into(),
                    args: vec!["test".into()],
                    cwd: None,
                    output_limit_bytes: None,
                    artifact_paths: Vec::new(),
                },
            },
            ClientRequest::ListSessionTasks {
                session_id: session,
            },
            ClientRequest::GetSessionTask {
                session_id: session,
                task_id: task,
            },
            ClientRequest::GetSessionTaskEvents {
                session_id: session,
                task_id: task,
                after_sequence: None,
            },
            ClientRequest::CancelSessionTask {
                session_id: session,
                task_id: task,
            },
            ClientRequest::GetSessionTaskEvidence {
                session_id: session,
                task_id: task,
            },
            ClientRequest::SetSessionApprovalPolicy {
                session_id: session,
                policy: loom_core::ApprovalPolicy::default(),
                auto_approve_actions: None,
            },
            ClientRequest::ForkAgentSession {
                session_id: session,
                name: "fork".into(),
            },
            ClientRequest::GetSessionUsage {
                session_id: session,
            },
        ];
        assert!(
            explicit_requests
                .iter()
                .all(|request| { session_id_for_request(request, active) == Some(session) })
        );

        assert_eq!(
            session_id_for_request(
                &ClientRequest::GetSessionEvents {
                    session_id: None,
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                },
                active
            ),
            Some(active)
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::GetSessionEvents {
                    session_id: Some(session),
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                },
                active
            ),
            Some(session)
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::GetAgentRun {
                    run_id: RunId::new()
                },
                active
            ),
            Some(active)
        );
        assert_eq!(
            session_id_for_request(
                &ClientRequest::GetRunUsage {
                    run_id: RunId::new()
                },
                active
            ),
            Some(active)
        );
        assert_eq!(
            session_id_for_request(&ClientRequest::ListWorkspaces, active),
            None
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
        assert_eq!(
            validate_model_for_node(&catalogs, "missing", &worker_model),
            Err("model availability has not been checked".to_owned())
        );
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
        assert_eq!(adjusted_cpu_pulse_threshold(255, 0), 100);
    }

    #[test]
    fn project_agent_concurrency_adjustment_is_bounded() {
        assert_eq!(
            loom_protocol::WorkspaceConfig::default().project_agent_concurrency,
            4
        );
        assert_eq!(adjusted_project_agent_concurrency(4, -1), 3);
        assert_eq!(
            adjusted_project_agent_concurrency(loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY, -1),
            loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY
        );
        assert_eq!(
            adjusted_project_agent_concurrency(loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY, 1),
            loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY
        );
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
        assert_eq!(next_severe_load_streak(u8::MAX, &status.resources), u8::MAX);

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

#[cfg(test)]
mod transcript_paging_tests {
    use super::{TimelineItem, prepend_timeline_page, timeline_items_from_messages};
    use loom_model::{MessageRole, ModelMessage};
    use std::collections::BTreeSet;

    #[test]
    fn transcript_pages_keep_message_order_and_project_tool_output_only_without_activities() {
        let messages = vec![
            ModelMessage::new(MessageRole::User, "task"),
            ModelMessage::new(MessageRole::Assistant, "first"),
            ModelMessage::new(MessageRole::Assistant, "second"),
            ModelMessage::new(MessageRole::Tool, "tool output"),
            ModelMessage::new(MessageRole::System, "hidden"),
            ModelMessage::new(MessageRole::User, "follow-up"),
        ];

        assert!(matches!(
            timeline_items_from_messages(messages.clone(), true).as_slice(),
            [
                TimelineItem::User(task),
                TimelineItem::Assistant(answer),
                TimelineItem::User(follow_up)
            ] if task == "task" && answer == "first\n\nsecond" && follow_up == "follow-up"
        ));
        assert!(matches!(
            timeline_items_from_messages(messages, false).as_slice(),
            [
                TimelineItem::User(task),
                TimelineItem::Assistant(answer),
                TimelineItem::ToolOutput(tool),
                TimelineItem::User(follow_up)
            ] if task == "task" && answer == "first\n\nsecond"
                && tool.contains("tool output") && follow_up == "follow-up"
        ));
    }

    #[test]
    fn older_pages_prepend_and_join_assistant_messages_at_the_page_boundary() {
        let mut timeline = vec![
            TimelineItem::Plan {
                steps: vec!["plan".to_owned()],
                completed: BTreeSet::new(),
                active: None,
            },
            TimelineItem::User("task".to_owned()),
            TimelineItem::Assistant("newer answer".to_owned()),
        ];
        prepend_timeline_page(
            &mut timeline,
            vec![
                TimelineItem::User("older question".to_owned()),
                TimelineItem::Assistant("older answer".to_owned()),
            ],
            2,
        );
        assert!(matches!(
            timeline.as_slice(),
            [
                TimelineItem::Plan { .. },
                TimelineItem::User(task),
                TimelineItem::User(question),
                TimelineItem::Assistant(answer)
            ] if task == "task" && question == "older question"
                && answer == "older answer\n\nnewer answer"
        ));
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn asynchronous_page_loader_uses_the_bounded_transcript_endpoint() {
        use super::{BackendWorker, ClientConnection, load_transcript_page};
        use loom_protocol::{ClientRequest, RequestEnvelope, ServerResponse};

        let backend = loom_server::InProcessBackend::new();
        let connection = ClientConnection::InProcess(Box::new(backend.connect()));
        crate::connection::negotiate(&connection).unwrap();
        let workspace =
            crate::connection::create_workspace(&connection, "Transcript pages").unwrap();
        let session = crate::connection::create_session_in_workspace(
            &connection,
            workspace.id,
            "Paged session",
        )
        .unwrap();
        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id: session.id,
                task: "load only a transcript page".to_owned(),
                model: loom_model::ModelId::new("deterministic/demo"),
                system_instructions: None,
                repository_instructions: None,
            }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(run) => run.id,
            response => panic!("unexpected run start response: {response:?}"),
        };
        let worker = BackendWorker::spawn(connection);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (messages, next_before, has_older) = runtime
            .block_on(load_transcript_page(worker, run_id, None))
            .unwrap();
        assert_eq!(next_before, Some(0));
        assert!(!has_older);
        assert!(messages.iter().any(|message| {
            message.role == MessageRole::User && message.content == "load only a transcript page"
        }));
    }
}
