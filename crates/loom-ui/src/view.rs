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

fn commands_matching(query: &str) -> Vec<&'static CommandSpec> {
    let query = query.trim_start_matches('/').to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|command| {
            query.is_empty()
                || command.name.starts_with(&query)
                || command.title.to_ascii_lowercase().contains(&query)
        })
        .collect()
}

/// Derives the composer's inline completion from its current text.
fn completion_for_value(value: &str) -> Option<ComposerCompletion> {
    let trimmed = value.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('/') {
        if trimmed.split_once(char::is_whitespace).is_none() {
            let token = trimmed.split_whitespace().next().unwrap_or_default();
            return Some(ComposerCompletion {
                kind: CompletionKind::Command,
                query: token.to_string(),
                selected: 0,
            });
        }
        return None;
    }
    let last = value.split_whitespace().last().unwrap_or_default();
    if let Some(query) = last.strip_prefix('@') {
        return Some(ComposerCompletion {
            kind: CompletionKind::File,
            query: query.to_string(),
            selected: 0,
        });
    }
    None
}

/// Rewrites the leading `/command` token of the composer text.
fn replace_command_token(value: &str, name: &str) -> String {
    let rest = value
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest)
        .unwrap_or("");
    if rest.is_empty() {
        format!("/{name} ")
    } else {
        format!("/{name} {rest}")
    }
}

/// Replaces the final whitespace-delimited token of the composer text.
fn replace_last_token(value: &str, replacement: &str) -> String {
    let start = value
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    format!("{}{}", &value[..start], replacement)
}

/// A compact relative time for a session, e.g. `2h ago`.
fn relative_time(millis: u64, now: u64) -> String {
    let seconds = now.saturating_sub(millis) / 1000;
    match seconds {
        0..=45 => "just now".to_owned(),
        46..=90 => "1m ago".to_owned(),
        91..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        86_400..604_800 => format!("{}d ago", seconds / 86_400),
        _ => format!("{}w ago", seconds / 604_800),
    }
}

/// A rough auto-grow height for the composer, in pixels.
fn composer_height(value: &str) -> f32 {
    let lines = value.lines().count().clamp(1, 8);
    28. + (lines as f32 - 1.) * 20.
}

/// The accent color for a run state.
fn run_state_color(state: AgentRunState) -> gpui_kit::Rgba {
    match state {
        AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating => {
            rgb(0x93c5fd)
        }
        AgentRunState::AwaitingApproval | AgentRunState::NeedsInput => rgb(0xfbbf24),
        AgentRunState::Paused => rgb(0x94a3b8),
        AgentRunState::Completed => rgb(0x9ad7bd),
        AgentRunState::Failed | AgentRunState::Cancelled => rgb(0xfca5a5),
    }
}

/// Whether a session state represents work in progress.
fn session_is_active(state: AgentSessionState) -> bool {
    matches!(
        state,
        AgentSessionState::Planning
            | AgentSessionState::Executing
            | AgentSessionState::AwaitingApproval
            | AgentSessionState::NeedsInput
            | AgentSessionState::Evaluating
    )
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

#[cfg(test)]
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

#[cfg(not(target_family = "wasm"))]
fn load_transcript_page_sync(
    connection: &ClientConnection,
    run_id: RunId,
    before_ordinal: Option<u64>,
) -> Result<TranscriptPage, LoomError> {
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
            .map(|message| (message.ordinal, message.timeline_ordinal, message.message))
            .collect(),
        next_before,
        has_older,
    ))
}

fn timeline_items_from_messages(
    messages: Vec<TranscriptMessage>,
    mut activities: Vec<AgentActivityRecord>,
) -> Vec<TimelineItem> {
    enum Entry {
        Message(ModelMessage),
        Activity(Box<AgentActivityRecord>),
    }
    let mut entries = messages
        .into_iter()
        .map(|(_, timeline_ordinal, message)| (timeline_ordinal, Entry::Message(message)))
        .chain(activities.drain(..).map(|activity| {
            (
                activity.timeline_ordinal,
                Entry::Activity(Box::new(activity)),
            )
        }))
        .collect::<Vec<_>>();
    entries.sort_by_key(|(timeline_ordinal, _)| *timeline_ordinal);

    let mut timeline = Vec::new();
    for (_, entry) in entries {
        match entry {
            Entry::Message(message) => match message.role {
                MessageRole::User if message.name.as_deref() == Some("loom_project_message") => {
                    timeline.push(TimelineItem::ProjectMessageContext(message.content));
                }
                MessageRole::User => timeline.push(TimelineItem::User(message.content)),
                MessageRole::Assistant => {
                    if !message.content.is_empty() {
                        let appended = match timeline.last_mut() {
                            Some(TimelineItem::Assistant(turn)) => match turn.parts.last_mut() {
                                Some(AssistantPart::Text(existing)) => {
                                    if !existing.is_empty() {
                                        existing.push_str("\n\n");
                                    }
                                    existing.push_str(&message.content);
                                    true
                                }
                                _ => false,
                            },
                            _ => false,
                        };
                        if !appended {
                            timeline.push(TimelineItem::Assistant(AssistantTurn::text(
                                message.content.clone(),
                            )));
                        }
                    }
                    for call in &message.tool_calls {
                        upsert_tool_part(
                            &mut timeline,
                            tool_part_from_call(call, ToolPartStatus::Queued),
                        );
                    }
                }
                MessageRole::Tool => {
                    let call = loom_model::ToolCall {
                        id: message.tool_call_id.unwrap_or_default(),
                        name: message.name.clone().unwrap_or_else(|| "tool".to_owned()),
                        arguments: serde_json::Value::Null,
                    };
                    let mut part = tool_part_from_call(&call, ToolPartStatus::Completed);
                    part.output =
                        Some(bounded(&humanize_tool_output(&call.name, &message.content)));
                    upsert_tool_part(&mut timeline, part);
                }
                MessageRole::System => {}
            },
            Entry::Activity(activity) => {
                if let Some(part) = tool_part_from_activity(&activity) {
                    upsert_tool_part(&mut timeline, part);
                }
            }
        }
    }
    timeline
}

fn project_message_transcript_content(message: &AgentMessageRecord) -> String {
    let kind = match message.kind {
        loom_core::AgentMessageKind::Progress => "progress",
        loom_core::AgentMessageKind::Result => "result",
        loom_core::AgentMessageKind::Question => "question",
        loom_core::AgentMessageKind::Blocker => "blocker",
        loom_core::AgentMessageKind::Direction => "direction",
        loom_core::AgentMessageKind::Answer => "answer",
    };
    let task = message
        .task_id
        .map(|task_id| format!("; task {task_id}"))
        .unwrap_or_default();
    format!(
        "[Project message {} from agent {} ({kind}{task})]\n{}",
        message.project_sequence, message.sender_session_id, message.body
    )
}

fn remove_project_message_context_duplicates(
    timeline: &mut Vec<TimelineItem>,
    project_messages: &[AgentMessageRecord],
) {
    let project_message_contexts = project_messages
        .iter()
        .map(project_message_transcript_content)
        .collect::<BTreeSet<_>>();
    timeline.retain(|item| {
        !matches!(item, TimelineItem::ProjectMessageContext(content)
            if project_message_contexts.contains(content))
    });
}

fn unseen_transcript_messages(
    messages: Vec<TranscriptMessage>,
    loaded_ordinals: &mut BTreeSet<u64>,
) -> Vec<TranscriptMessage> {
    messages
        .into_iter()
        .filter_map(|(ordinal, timeline_ordinal, message)| {
            loaded_ordinals
                .insert(ordinal)
                .then_some((ordinal, timeline_ordinal, message))
        })
        .collect()
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
    let mut labels = BTreeMap::new();
    for model in models {
        let provider = provider_names
            .and_then(|names| names.get(model))
            .map(String::as_str)
            .unwrap_or("Provider");
        let short = model.as_str().to_owned();
        let label = if labels.contains_key(&short) {
            format!("{provider} · {short}")
        } else {
            short
        };
        labels.insert(label, model.clone());
    }
    labels
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

fn tool_element_id(index: usize, part_index: usize) -> u64 {
    ((index as u64) << 32) | part_index as u64
}

fn reasoning_preview(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview = normalized.chars().take(96).collect::<String>();
    if normalized.chars().count() > 96 {
        preview.push('…');
    }
    preview
}

fn streaming_caret(index: usize) -> gpui_kit::AnyElement {
    div()
        .w(px(9.))
        .h(px(16.))
        .rounded_sm()
        .bg(rgb(0x93c5fd))
        .with_animation(
            ("assistant-streaming", index),
            Animation::new(Duration::from_millis(900))
                .repeat_synced()
                .with_max_fps(12.),
            |element, progress| element.opacity(0.2 + 0.8 * (1. - progress)),
        )
        .into_any()
}

fn assistant_turn_matches(
    timeline: &[TimelineItem],
    predicate: impl Fn(&AssistantPart) -> bool,
) -> bool {
    timeline.iter().any(
        |item| matches!(item, TimelineItem::Assistant(turn) if turn.parts.iter().any(&predicate)),
    )
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
                    background_color: Some(rgb(0x1b1d24).alpha(0.).into()),
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

/// A monospace code block with a line-number gutter and horizontal scrolling.
fn render_code_block(
    id: impl Into<gpui_kit::ElementId>,
    code: &str,
    language: Language,
    numbered: bool,
) -> gpui_kit::AnyElement {
    let spans = syntax::highlight(code, language);
    let mut lines = code.split('\n').collect::<Vec<_>>();
    if lines.len() > 1 && lines.last() == Some(&"") {
        lines.pop();
    }
    let mut column = div()
        .id(id)
        .w_full()
        .flex()
        .flex_col()
        .font_family(mono_font())
        .text_size(gpui_kit::rems(mono_size() / BASE_FONT_SIZE))
        .overflow_x_scroll();
    let mut offset = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let line_start = offset;
        let line_end = line_start + line.len();
        let highlights = syntax::line_highlights(&spans, line_start..line_end);
        let mut row = div().flex().flex_row().items_start().whitespace_nowrap();
        if numbered {
            row = row.child(
                div()
                    .w(px(34.))
                    .flex_shrink_0()
                    .pr_2()
                    .text_right()
                    .text_color(rgb(0x64748b))
                    .child(format!("{}", index + 1)),
            );
        }
        column =
            column.child(row.child(StyledText::new(line.to_string()).with_highlights(highlights)));
        offset = line_end + 1;
    }
    column.into_any()
}

/// A diff rendered as colored, monospace lines.
fn render_patch_block(id: impl Into<gpui_kit::ElementId>, patch: &str) -> gpui_kit::AnyElement {
    let mut column = div()
        .id(id)
        .w_full()
        .flex()
        .flex_col()
        .font_family(mono_font())
        .text_size(gpui_kit::rems(mono_size() / BASE_FONT_SIZE))
        .overflow_x_scroll();
    for line in patch.split('\n') {
        let (background, foreground) = syntax::diff_line_style(syntax::classify_diff_line(line));
        column = column.child(
            div()
                .px_1()
                .whitespace_nowrap()
                .when_some(background, |element, background| element.bg(background))
                .text_color(foreground)
                .child(line.to_string()),
        );
    }
    column.into_any()
}

/// Picks a language for a tool result from its name and argument hint.
fn tool_output_language(part: &ToolPart) -> Language {
    match part.name.as_str() {
        "run_command" => Language::Bash,
        "search_text" | "web_search" => Language::Text,
        _ => {
            let hint = part.detail.as_deref().unwrap_or(&part.title);
            let language = Language::from_path(hint);
            if language == Language::Text {
                Language::from_hint(&part.name)
            } else {
                language
            }
        }
    }
}

/// The icon shown for a tool, by tool name.
fn tool_icon(name: &str) -> AssetIconName {
    match name {
        "read_file" => AssetIconName::FileText,
        "write_file" | "apply_patch" => AssetIconName::Pencil,
        "list_files" => AssetIconName::FolderOpen,
        "search_text" => AssetIconName::Search,
        "web_search" => AssetIconName::Globe,
        "run_command" => AssetIconName::SquareTerminal,
        "propose_plan" => AssetIconName::ListChecks,
        "delegate_project_task" => AssetIconName::UserPlus,
        "delegate_project_code_task" => AssetIconName::GitBranch,
        "wait_for_project_children" => AssetIconName::Clock,
        "control_project_child" => AssetIconName::Workflow,
        "send_project_agent_message" => AssetIconName::MessageSquare,
        "list_project_message_recipients" => AssetIconName::Users,
        "list_project_children" => AssetIconName::List,
        "review_project_child" => AssetIconName::Eye,
        "integrate_project_child" => AssetIconName::GitMerge,
        _ => AssetIconName::Wrench,
    }
}

/// The summary title for a collapsed group of same-kind tool calls.
fn tool_group_label(name: &str, count: usize) -> String {
    match name {
        "read_file" => format!("Read {count} files"),
        "write_file" | "apply_patch" => format!("Edited {count} files"),
        "list_files" => format!("Listed {count} directories"),
        "search_text" => format!("Searched {count} times"),
        "run_command" => format!("Ran {count} commands"),
        "web_search" => format!("Searched the web {count} times"),
        "propose_plan" => format!("Proposed {count} plans"),
        other => format!("{other} × {count}"),
    }
}

/// Renders a tool result as a patch when it looks like one, otherwise as code.
fn render_tool_output(id: impl Into<gpui_kit::ElementId>, part: &ToolPart) -> gpui_kit::AnyElement {
    let Some(output) = &part.output else {
        return div().into_any();
    };
    if syntax::looks_like_patch(output) {
        render_patch_block(id, output)
    } else {
        let language = tool_output_language(part);
        let numbered = !matches!(language, Language::Bash | Language::Text);
        render_code_block(id, output, language, numbered)
    }
}

/// The structured arguments shown under a tool block. Returns `None` for the
/// argument shapes that carry no information (`null` or an empty object) so the
/// transcript never renders a bare `null`. Project-agent tools get a readable
/// summary instead of their raw JSON arguments.
fn tool_detail(call: &loom_model::ToolCall) -> Option<String> {
    match call.name.as_str() {
        "delegate_project_task" | "delegate_project_code_task" => {
            string_argument(&call.arguments, "intent")
                .map(|intent| bounded_to(intent.trim(), 600))
                .filter(|intent| !intent.is_empty())
        }
        "wait_for_project_children" => {
            let ids = call
                .arguments
                .get("task_ids")
                .and_then(serde_json::Value::as_array)?;
            let joined = ids
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|id| id.chars().take(8).collect::<String>())
                .collect::<Vec<_>>()
                .join(", ");
            Some(if joined.is_empty() {
                format!("{} child tasks", ids.len())
            } else {
                format!("Waiting on {joined}")
            })
        }
        "send_project_agent_message" => {
            let kind = string_argument(&call.arguments, "kind").unwrap_or_default();
            let body = bounded_to(
                string_argument(&call.arguments, "body")
                    .unwrap_or_default()
                    .trim(),
                600,
            );
            if body.is_empty() {
                None
            } else if kind.is_empty() {
                Some(body)
            } else {
                Some(format!("[{kind}] {body}"))
            }
        }
        "control_project_child"
        | "review_project_child"
        | "integrate_project_child"
        | "list_project_children"
        | "list_project_message_recipients" => None,
        _ => match &call.arguments {
            serde_json::Value::Null => None,
            serde_json::Value::Object(map) if map.is_empty() => None,
            arguments => Some(bounded_to(
                &serde_json::to_string(arguments).unwrap_or_default(),
                180,
            )),
        },
    }
}

/// Rewrites a project-agent tool's JSON result into a short human-readable
/// summary. Other tools keep their raw output.
fn humanize_tool_output(name: &str, output: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return output.to_owned();
    };
    match name {
        "delegate_project_task" | "delegate_project_code_task" => {
            let child =
                string_field(&value, "child_name").unwrap_or_else(|| "sub-agent".to_owned());
            let status = string_field(&value, "status").unwrap_or_else(|| "created".to_owned());
            let mut summary = format!("Created sub-agent \"{child}\" · {status}");
            if let Some(task_id) = string_field(&value, "task_id") {
                let short = task_id.chars().take(8).collect::<String>();
                summary.push_str(&format!("\ntask {short}"));
            }
            summary
        }
        "wait_for_project_children" => {
            let ready = value
                .get("return_ready")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let children = value
                .get("children")
                .and_then(serde_json::Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut summary = if ready {
                "All selected children are return-ready".to_owned()
            } else {
                "Selected children are not ready yet".to_owned()
            };
            summary.push_str(&format!(
                "\n{} child{}:",
                children.len(),
                if children.len() == 1 { "" } else { "ren" }
            ));
            for child in children {
                let name =
                    string_field(child, "child_name").unwrap_or_else(|| "sub-agent".to_owned());
                let status = string_field(child, "status").unwrap_or_else(|| "unknown".to_owned());
                let code = child
                    .get("code_change")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                summary.push_str(&format!(
                    "\n  {name}: {status}{}",
                    if code { " (code)" } else { "" }
                ));
            }
            summary
        }
        "control_project_child" => match string_field(&value, "status") {
            Some(status) => format!("Sub-agent now {status}"),
            None => output.to_owned(),
        },
        "send_project_agent_message" => match value
            .get("project_sequence")
            .and_then(serde_json::Value::as_u64)
        {
            Some(sequence) => format!("Message accepted · #{sequence}"),
            None => output.to_owned(),
        },
        "integrate_project_child" => match string_field(&value, "status") {
            Some(status) => format!("Integrated · {status}"),
            None => output.to_owned(),
        },
        _ => output.to_owned(),
    }
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn string_argument(arguments: &serde_json::Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// A short human intent for a tool call, used as the tool block title.
fn tool_title(name: &str, arguments: &serde_json::Value) -> String {
    let path = || string_argument(arguments, "path");
    match name {
        "read_file" => path().map_or_else(|| "Read file".to_owned(), |p| format!("Read {p}")),
        "write_file" => path().map_or_else(|| "Edit file".to_owned(), |p| format!("Edit {p}")),
        "list_files" => path().map_or_else(|| "List files".to_owned(), |p| format!("List {p}")),
        "search_text" => string_argument(arguments, "query").map_or_else(
            || "Search text".to_owned(),
            |query| format!("Search \"{}\"", compact_activity_text(&query, 40)),
        ),
        "web_search" => string_argument(arguments, "query").map_or_else(
            || "Web search".to_owned(),
            |query| format!("Web search \"{}\"", compact_activity_text(&query, 40)),
        ),
        "apply_patch" => path().map_or_else(|| "Apply patch".to_owned(), |p| format!("Edit {p}")),
        "run_command" => {
            let Some(command) = string_argument(arguments, "command") else {
                return "Run command".to_owned();
            };
            let args = arguments
                .get("args")
                .and_then(serde_json::Value::as_array)
                .map(|args| {
                    args.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            command_purpose(&command, &args)
        }
        "propose_plan" => "Propose a plan".to_owned(),
        "ask_user" => "Ask the user".to_owned(),
        "delegate_project_task" | "delegate_project_code_task" => {
            let code = name == "delegate_project_code_task";
            match string_argument(arguments, "child_name") {
                Some(child) => format!(
                    "Delegate {}sub-agent \"{}\"",
                    if code { "code " } else { "" },
                    compact_activity_text(&child, 40)
                ),
                None => format!("Delegate {}sub-agent", if code { "code " } else { "" }),
            }
        }
        "wait_for_project_children" => {
            let count = arguments
                .get("task_ids")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            match count {
                0 => "Wait for sub-agents".to_owned(),
                1 => "Wait for 1 sub-agent".to_owned(),
                count => format!("Wait for {count} sub-agents"),
            }
        }
        "control_project_child" => match string_argument(arguments, "action").as_deref() {
            Some("continue") => "Continue sub-agent".to_owned(),
            Some("retry_failed_step") => "Retry sub-agent step".to_owned(),
            Some("cancel") => "Cancel sub-agent".to_owned(),
            _ => "Control sub-agent".to_owned(),
        },
        "send_project_agent_message" => "Message sub-agent".to_owned(),
        "list_project_message_recipients" => "List message recipients".to_owned(),
        "list_project_children" => "List sub-agents".to_owned(),
        "review_project_child" => "Review sub-agent".to_owned(),
        "integrate_project_child" => "Integrate sub-agent".to_owned(),
        other => other.to_owned(),
    }
}

fn tool_title_for_activity(activity: &AgentActivityRecord) -> String {
    match &activity.data {
        AgentActivityData::ModelTurn { .. } => String::new(),
        AgentActivityData::ToolCall { call, .. } => tool_title(&call.name, &call.arguments),
        AgentActivityData::File {
            operation, path, ..
        } => match operation {
            FileActivityOperation::List => path
                .clone()
                .map_or_else(|| "List files".to_owned(), |path| format!("List {path}")),
            FileActivityOperation::Read => path
                .clone()
                .map_or_else(|| "Read file".to_owned(), |path| format!("Read {path}")),
            FileActivityOperation::Write => path
                .clone()
                .map_or_else(|| "Edit file".to_owned(), |path| format!("Edit {path}")),
        },
        AgentActivityData::Search { query, path, .. } => {
            let suffix = path
                .as_deref()
                .map_or_else(String::new, |path| format!(" in {path}"));
            format!("Search \"{}\"{suffix}", compact_activity_text(query, 40))
        }
        AgentActivityData::Command { command, args, .. } => command_purpose(command, args),
    }
}

fn tool_status(status: AgentActivityStatus) -> ToolPartStatus {
    match status {
        AgentActivityStatus::Started => ToolPartStatus::Running,
        AgentActivityStatus::Completed => ToolPartStatus::Completed,
        AgentActivityStatus::Failed => ToolPartStatus::Failed,
        AgentActivityStatus::AwaitingApproval => ToolPartStatus::AwaitingApproval,
        AgentActivityStatus::AwaitingInput => ToolPartStatus::AwaitingInput,
        AgentActivityStatus::Cancelled => ToolPartStatus::Cancelled,
    }
}

fn tool_part_from_call(call: &loom_model::ToolCall, status: ToolPartStatus) -> ToolPart {
    ToolPart {
        id: call.id,
        name: call.name.clone(),
        title: tool_title(&call.name, &call.arguments),
        status,
        detail: tool_detail(call),
        output: None,
        elapsed_ms: None,
        approval_pending: status == ToolPartStatus::AwaitingApproval,
    }
}

fn tool_part_from_activity(activity: &AgentActivityRecord) -> Option<ToolPart> {
    let call = match &activity.data {
        AgentActivityData::ModelTurn { .. } => return None,
        AgentActivityData::ToolCall { call, .. }
        | AgentActivityData::File { call, .. }
        | AgentActivityData::Search { call, .. }
        | AgentActivityData::Command { call, .. } => call,
    };
    let detail = match &activity.data {
        AgentActivityData::ToolCall { .. } => tool_detail(call),
        AgentActivityData::File { path, .. } => {
            Some(path.clone().unwrap_or_else(|| ".".to_owned()))
        }
        AgentActivityData::Search { query, .. } => Some(format!("\"{}\"", bounded_to(query, 120))),
        AgentActivityData::Command {
            command, args, cwd, ..
        } => {
            let mut detail = command_line(command, args);
            if let Some(cwd) = cwd {
                detail.push_str(&format!("\nDirectory: {cwd}"));
            }
            Some(detail)
        }
        AgentActivityData::ModelTurn { .. } => None,
    };
    Some(ToolPart {
        id: call.id,
        name: call.name.clone(),
        title: tool_title_for_activity(activity),
        status: tool_status(activity.status),
        detail,
        output: activity_output(activity)
            .map(|output| bounded(&humanize_tool_output(&call.name, output))),
        elapsed_ms: activity.elapsed_ms,
        approval_pending: activity.status == AgentActivityStatus::AwaitingApproval,
    })
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

impl TimelineView {
    fn new(parent: Entity<LoomView>, scroller: Entity<MessageScrollerState>) -> Self {
        Self {
            parent,
            scroller,
            parent_subscription: None,
            scroller_subscription: None,
            session_id: None,
            timeline_revision: (0, 0),
        }
    }

    fn sync_list(
        &mut self,
        item_count: usize,
        timeline_revision: (usize, usize),
        session_changed: bool,
        cx: &mut Context<Self>,
    ) {
        let content_changed = self.timeline_revision != timeline_revision;
        self.timeline_revision = timeline_revision;
        let current = self.scroller.read(cx).item_count();
        if session_changed || item_count < current {
            self.scroller
                .update(cx, |state, cx| state.reset(item_count, cx));
        } else if item_count > current {
            self.scroller
                .update(cx, |state, cx| state.append(item_count - current, cx));
        } else if content_changed {
            self.scroller.update(cx, |state, cx| state.remeasure(cx));
        }
    }
}

impl Render for TimelineView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.parent_subscription.is_none() {
            let parent = self.parent.clone();
            self.parent_subscription = Some(cx.observe(&parent, |_, _, cx| cx.notify()));
        }
        if self.scroller_subscription.is_none() {
            let scroller = self.scroller.clone();
            self.scroller_subscription = Some(cx.observe(&scroller, |_, _, cx| cx.notify()));
        }

        let (item_count, session_changed, timeline_revision) = {
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
            (item_count, session_changed, timeline_revision)
        };
        self.sync_list(item_count, timeline_revision, session_changed, cx);
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
                                )
                                .child(
                                    div()
                                        .mt_3()
                                        .flex()
                                        .gap_3()
                                        .text_size(gpui_kit::rems(13. / BASE_FONT_SIZE))
                                        .text_color(rgb(0x64748b))
                                        .child("/ commands")
                                        .child("@ files")
                                        .child("⌘K palette"),
                                ),
                        )
                );
        }

        let (transcript_has_older, transcript_loading) = {
            let parent_state = self.parent.read(cx);
            (
                parent_state.transcript_has_older,
                parent_state.transcript_loading,
            )
        };
        let parent = self.parent.clone();
        let parent_for_rows = parent.clone();
        let row_style = gpui_kit::StyleRefinement {
            padding: gpui_kit::EdgesRefinement {
                top: Some(px(0.).into()),
                right: Some(px(0.).into()),
                bottom: Some(px(0.).into()),
                left: Some(px(0.).into()),
            },
            ..Default::default()
        };
        let timeline = MessageScroller::new(
            "timeline-scroller",
            self.scroller.clone(),
            move |index, _window, cx| {
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
                    .into_any_element()
            },
        )
        .with_row_style(row_style)
        .with_jump_button_label("Jump to latest")
        .with_bottom_fade(gpui_kit::Hsla::from(rgb(0x111318)));
        let content = if transcript_has_older || transcript_loading {
            let parent_for_page = parent.clone();
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div().w_full().flex().justify_center().p_2().child(
                        Button::new("load-older-transcript")
                            .label(if transcript_loading {
                                "Loading older messages…"
                            } else {
                                "Load older messages"
                            })
                            .small()
                            .disabled(transcript_loading)
                            .on_click(move |_, _, cx| {
                                parent_for_page.update(cx, |view, cx| {
                                    view.begin_transcript_page(view.transcript_before_ordinal, cx);
                                });
                            }),
                    ),
                )
                .child(div().flex_1().min_h_0().child(timeline))
                .into_any()
        } else {
            timeline.into_any_element()
        };
        div().size_full().relative().child(content)
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
                |view, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => view.refresh_composer_completion(cx),
                    InputEvent::PressEnter { shift: false, .. } => {
                        if view.composer_completion.is_some() {
                            view.pending_completion_accept = true;
                            cx.notify();
                        } else {
                            view.submit_composer(cx);
                        }
                    }
                    _ => {}
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
        if self.pending_completion_accept {
            self.pending_completion_accept = false;
            self.accept_composer_completion(window, cx);
        }
        if self.command_palette_open && self.command_palette_input.is_none() {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Type a command or search…"));
            input.update(cx, |state, cx| state.focus(window, cx));
            self.input_subscriptions.push(cx.subscribe(
                &input,
                |view, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter { .. } => view.confirm_command_palette(cx),
                    _ => {}
                },
            ));
            self.command_palette_input = Some(input);
        } else if !self.command_palette_open && self.command_palette_input.is_some() {
            if let Some(input) = self.command_palette_input.as_ref() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.command_palette_input = None;
        }
        if self.session_filter_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter projects…"));
            self.input_subscriptions
                .push(cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }));
            self.session_filter_input = Some(input);
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
                                        .label("Projects")
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
                                                .child(
                                                    self.review
                                                        .vcs
                                                        .as_ref()
                                                        .map(|status| {
                                                            format!(
                                                                "{}  ·  {}",
                                                                status
                                                                    .branch
                                                                    .as_deref()
                                                                    .unwrap_or("detached"),
                                                                if status.clean {
                                                                    "clean"
                                                                } else {
                                                                    "modified"
                                                                }
                                                            )
                                                        })
                                                        .unwrap_or_else(|| {
                                                            let sources = self
                                                                .session_directories
                                                                .len()
                                                                + self.session_repositories.len();
                                                            match sources {
                                                                0 => "No source attached".to_owned(),
                                                                1 => "1 source".to_owned(),
                                                                count => format!("{count} sources"),
                                                            }
                                                        }),
                                                ),
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
                            .accessibility_label("Session sources")
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
                            .accessibility_label("Toggle side panel")
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
                        .gap_4()
                        .bg(rgb(0x111318))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(AssetIconName::Sparkles)
                                        .size_8()
                                        .text_color(rgb(0x93c5fd)),
                                )
                                .child(
                                    div()
                                        .text_size(gpui_kit::rems(1.25))
                                        .text_color(rgb(0xf3f4f6))
                                        .child("Work with agents, keep the trail"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(0x8f98a6))
                                        .child("Start a session over a repository or folder."),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .text_xs()
                                .text_color(rgb(0x64748b))
                                .child("/ commands")
                                .child("@ files")
                                .child("⌘K command palette"),
                        )
                        .child(
                            Button::new("start-first-session")
                                .label("Create a project")
                                .primary()
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
            .on_key_down(cx.listener(|this, event: &gpui_kit::KeyDownEvent, _, cx| {
                let modifiers = event.keystroke.modifiers;
                if (modifiers.platform || modifiers.control) && event.keystroke.key == "k" {
                    this.toggle_command_palette(cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "escape" && this.command_palette_open {
                    this.close_command_palette(cx);
                    cx.stop_propagation();
                }
            }))
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
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Loom"))
                                    .child(
                                        div().text_xs().text_color(rgb(0x64748b)).child(
                                            if self.demo_workspace { "Demo" } else { "Local" },
                                        ),
                                    ),
                            ),
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
            .when(self.command_palette_open, |element| {
                element.child(self.render_command_palette(cx))
            });
        content.into_any()
    }
}
mod composer;
mod lifecycle;
mod project;
mod providers;
mod render;
mod review;
mod runs;
mod sessions;
mod source;
mod workers;

#[cfg(test)]
mod tests;
