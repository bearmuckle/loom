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

    fn rebuild_project_message_timeline(&mut self) {
        self.timeline
            .retain(|item| !matches!(item, TimelineItem::ProjectMessage(_)));
        if self
            .project_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.root_session_id == self.active_session.id)
        {
            remove_project_message_context_duplicates(&mut self.timeline, &self.project_messages);
            self.timeline.extend(
                self.project_messages
                    .iter()
                    .cloned()
                    .map(TimelineItem::ProjectMessage),
            );
        }
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
                let mut reload_sessions = false;
                match response.result {
                    Ok(ServerResponse::ProjectSnapshot(snapshot)) => {
                        reload_sessions =
                            project_snapshot_has_unloaded_agent_sessions(&snapshot, &view.sessions);
                        view.project_tree_snapshots
                            .retain(|known| known.project_id != snapshot.project_id);
                        view.project_tree_snapshots.push(snapshot.clone());
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
                if reload_sessions {
                    view.reload_sessions(cx);
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
            let mut first_failure = None;
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
                            if first_failure.is_none() {
                                first_failure = Some(error);
                            }
                            break;
                        }
                        Ok(response) => {
                            if first_failure.is_none() {
                                first_failure =
                                    Some(unexpected_response("project messages", response));
                            }
                            break;
                        }
                    }
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
                view.project_messages_loading = false;
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
                let had_failure = first_failure.is_some();
                if let Some(error) = first_failure {
                    view.project_messages_stale = true;
                    view.record_backend_error("load project messages", error);
                }
                if !had_failure && view.project_messages_stale {
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
        expected_child_revision: String,
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
                Ok(ServerResponse::ProjectChildReview { status, .. }) if status.clean
                    && status.head.as_deref() == Some(expected_child_revision.as_str()) =>
                {
                    view.submit_project_child_integration(
                        manager_session_id,
                        project_id,
                        task_id,
                        expected_parent_revision,
                        cx,
                    );
                }
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
                    view.record_status(
                        "Child checkout changed since review or has uncommitted edits. Review the current diff before integrating.",
                    );
                    view.refresh_active_project_snapshot(cx);
                }
                Err(error) => view.record_backend_error("check child review before integration", error),
                Ok(response) => view.record_backend_error(
                    "check child review before integration",
                    unexpected_response("project child review", response),
                ),
            },
        );
    }

    fn submit_project_child_integration(
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
            self.project_tree_snapshots
                .retain(|known| known.project_id != snapshot.project_id);
            self.project_tree_snapshots.push(snapshot.clone());
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
    /// Whether the session event stream should keep being polled. A run that is
    /// parked on a durable project join reports `Paused`, but the server
    /// resumes it when its children finish, so polling must continue past the
    /// active states or the wait never appears to complete.
    fn run_should_poll(&self) -> bool {
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
            self.timeline
                .push(TimelineItem::Assistant(AssistantTurn::text(
                    "This is demo mode. The browser client needs to connect to a backend to work.",
                )));
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

    /// Whether the active run can still be interrupted.
    fn run_can_interrupt(&self) -> bool {
        self.active_run_id.is_some()
            && !matches!(
                self.run_state,
                Some(AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled)
            )
    }

    fn interrupt_active_run(&mut self, cx: &mut Context<Self>) {
        let Some(run_id) = self.active_run_id else {
            return;
        };
        self.dispatch(
            cx,
            ClientRequest::InterruptAgentRun { run_id },
            |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::AgentRun(run))
                    | Ok(ServerResponse::AgentRunStarted(run)) => {
                        view.active_run_id = Some(run.id);
                        view.active_run = Some(run.clone());
                        view.run_state = Some(run.state);
                        view.session_state = session_state_for_run(run.state);
                        view.active_session.state = view.session_state;
                    }
                    Err(error) => view.record_backend_error("interrupt run", error),
                    Ok(response) => view.record_backend_error(
                        "interrupt run",
                        unexpected_response("interrupt run", response),
                    ),
                }
                cx.notify();
            },
        );
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
        let mut parts = text.split_whitespace();
        let command = parts.next().unwrap_or_default();
        let name = command.trim_start_matches('/');
        let argument = parts.next();
        self.run_command(name, argument, cx);
    }

    /// Runs a command by name, shared by slash commands and the palette.
    fn run_command(&mut self, name: &str, argument: Option<&str>, cx: &mut Context<Self>) {
        match name {
            "" | "help" => self.record_status(
                "Commands: /new, /repo, /review, /stop, /providers, /settings, /help",
            ),
            "new" => self.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx),
            "repo" | "repository" => {
                self.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx)
            }
            "review" => self.toggle_review_pane(cx),
            "stop" => {
                if self.run_can_interrupt() {
                    self.interrupt_active_run(cx);
                } else {
                    self.record_status("No active run to stop.");
                }
            }
            "providers" => self.open_providers_from_menu(cx),
            "settings" => self.open_settings_from_menu(cx),
            "model" => match argument {
                Some(model) => {
                    self.record_status(format!("Use the model picker to switch to '{model}'."))
                }
                None => self.record_status(format!("Current model: {}", self.model.as_str())),
            },
            command => self.record_status(format!("Unknown command '{command}'. Try /help.")),
        }
        cx.notify();
    }

    /// Recomputes the composer's inline completion from the current text.
    fn refresh_composer_completion(&mut self, cx: &mut Context<Self>) {
        if self.suppress_completion_once {
            self.suppress_completion_once = false;
            self.composer_completion = None;
            return;
        }
        let value = self
            .composer_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        self.composer_completion = completion_for_value(&value);
        cx.notify();
    }

    /// Applies the highlighted completion to the composer text.
    fn accept_composer_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(completion) = self.composer_completion.take() else {
            return;
        };
        let Some(input) = self.composer_input.clone() else {
            return;
        };
        let value = input.read(cx).value().to_string();
        let replacement = match completion.kind {
            CompletionKind::Command => {
                let matches = commands_matching(&completion.query);
                let Some(command) = matches.get(completion.selected).copied() else {
                    return;
                };
                replace_command_token(&value, command.name)
            }
            CompletionKind::File => {
                let matches = self.file_completion_candidates();
                let filtered = matches
                    .iter()
                    .filter(|path| path.contains(&completion.query))
                    .collect::<Vec<_>>();
                let Some(path) = filtered.get(completion.selected) else {
                    return;
                };
                replace_last_token(&value, &format!("@{path} "))
            }
        };
        self.suppress_completion_once = true;
        input.update(cx, |state, cx| state.set_value(replacement, window, cx));
        input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Paths offered for `@` completion: changed files and attached sources.
    fn file_completion_candidates(&self) -> Vec<String> {
        let mut candidates = self
            .review
            .vcs
            .as_ref()
            .map(|status| {
                status
                    .files
                    .iter()
                    .map(|file| file.path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        candidates.extend(self.review.changes.iter().map(|change| change.path.clone()));
        candidates.extend(
            self.session_directories
                .iter()
                .map(|directory| directory.path.clone()),
        );
        candidates.sort();
        candidates.dedup();
        candidates
    }

    fn toggle_command_palette(&mut self, cx: &mut Context<Self>) {
        self.command_palette_open = !self.command_palette_open;
        self.command_palette_selection = 0;
        cx.notify();
    }

    fn close_command_palette(&mut self, cx: &mut Context<Self>) {
        self.command_palette_open = false;
        self.command_palette_selection = 0;
        cx.notify();
    }

    /// Runs the highlighted palette command and closes the palette.
    fn confirm_command_palette(&mut self, cx: &mut Context<Self>) {
        let query = self
            .command_palette_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let matches = commands_matching(&query);
        let name = matches
            .get(self.command_palette_selection)
            .or_else(|| matches.first())
            .map(|command| command.name);
        self.command_palette_open = false;
        self.command_palette_selection = 0;
        if let Some(name) = name {
            self.run_command(name, None, cx);
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
        messages: Vec<(u64, u64, ModelMessage)>,
        next_before: Option<u64>,
        has_older: bool,
    ) {
        if self.active_run_id != Some(run_id) {
            return;
        }
        if before_ordinal.is_none() {
            self.transcript_loaded_ordinals.clear();
            self.transcript_messages.clear();
        }
        let messages = unseen_transcript_messages(messages, &mut self.transcript_loaded_ordinals);
        for (ordinal, timeline_ordinal, message) in messages {
            self.transcript_messages
                .insert(ordinal, (timeline_ordinal, message));
        }
        self.timeline.retain(|item| {
            !matches!(
                item,
                TimelineItem::User(_)
                    | TimelineItem::Assistant(_)
                    | TimelineItem::ProjectMessageContext(_)
            )
        });
        let ordered_items = timeline_items_from_messages(
            self.transcript_messages
                .iter()
                .map(|(ordinal, (timeline_ordinal, message))| {
                    (*ordinal, *timeline_ordinal, message.clone())
                })
                .collect(),
            self.activity_records.values().cloned().collect(),
        );
        let insertion_index = self.transcript_insertion_index();
        self.timeline
            .splice(insertion_index..insertion_index, ordered_items);
        self.rebuild_project_message_timeline();
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
                    || format!("Project {}", self.sessions.len().saturating_add(1)),
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
            .tooltip("New project")
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
                format!("Creating project and cloning {}…", repository.full_name)
            }
            Some(SessionCreationSource::LocalDirectory(_)) => {
                "Creating project and attaching directory…".to_owned()
            }
            None => "Creating project…".to_owned(),
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
                    view.record_status("Project created successfully");
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
        is_project: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_session(session, cx);
        self.rename_dialog = Some(RenameDialogState {
            session: self.active_session.clone(),
            input: self.active_session.name.clone(),
            is_project,
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

    fn build_project_session_context_menu(
        mut menu: PopupMenu,
        session: AgentSessionSnapshot,
        menu_project: Option<loom_core::ProjectSnapshot>,
        menu_view: Entity<LoomView>,
    ) -> PopupMenu {
        let rename_view = menu_view.clone();
        let archive_view = menu_view.clone();
        let rename_session = session.clone();
        let is_project = menu_project
            .as_ref()
            .is_none_or(|project| project.root_session_id == session.id);
        let archive_session = session.clone();
        let archive_label = menu_project
            .as_ref()
            .filter(|project| project.root_session_id == session.id)
            .map_or("Archive", |_| "Archive project");
        menu = menu
            .item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                let rename_session = rename_session.clone();
                rename_view.update(cx, |view, cx| {
                    view.begin_session_rename(rename_session, is_project, window, cx);
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
            let manager_permissions = if manager_session_id == project.root_session_id {
                Some((true, true, true))
            } else {
                project
                    .tasks
                    .iter()
                    .find(|manager_task| manager_task.target_session_id == manager_session_id)
                    .map(|manager_task| {
                        (
                            manager_task.permissions.child_control,
                            manager_task.permissions.review,
                            manager_task.permissions.integration,
                        )
                    })
            };
            let (can_control, can_review, can_integrate) = manager_permissions.unwrap_or_default();
            if can_control {
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
                if can_review
                    && !matches!(
                        worktree.status,
                        loom_core::ProjectWorktreeStatus::CleanupPending
                            | loom_core::ProjectWorktreeStatus::Removed
                    )
                {
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
                if can_integrate
                    && terminal
                    && task.status == loom_core::DelegatedTaskStatus::Completed
                    && worktree.status == loom_core::ProjectWorktreeStatus::Ready
                    && let Some(expected_child_revision) = worktree.result_revision.clone()
                {
                    let integrate_view = menu_view.clone();
                    let expected_parent_revision = worktree.base_revision.clone();
                    menu = menu.item(PopupMenuItem::new("Fast-forward child changes").on_click(
                        move |_, _, cx| {
                            integrate_view.update(cx, |view, cx| {
                                view.integrate_project_child_from_ui(
                                    manager_session_id,
                                    project_id,
                                    task_id,
                                    expected_parent_revision.clone(),
                                    expected_child_revision.clone(),
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
                    menu = menu.item(PopupMenuItem::new("Remove clean child checkout").on_click(
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
    }

    pub(crate) fn render_session_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self
            .session_filter_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let sessions = self.sessions.clone();
        let mut tree_projects = self.project_tree_snapshots.iter().collect::<Vec<_>>();
        if let Some(active_project) = self.project_snapshot.as_ref()
            && !tree_projects
                .iter()
                .any(|known| known.project_id == active_project.project_id)
        {
            tree_projects.push(active_project);
        }
        let projection = project_session_list_projection_for_projects(
            &sessions,
            self.active_session.id,
            tree_projects,
        );
        let tree_nodes = if filter.is_empty() {
            projection.tree
        } else {
            filter_session_tree(projection.tree, &filter)
        };
        let session_tasks = self
            .project_tree_snapshots
            .iter()
            .flat_map(|project| project.tasks.iter())
            .map(|task| (task.target_session_id, task.status))
            .collect::<BTreeMap<_, _>>();
        let descendant_counts = tree_nodes
            .iter()
            .map(|node| (node.session_id, session_tree_descendant_count(node)))
            .collect::<BTreeMap<_, _>>();
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
            let depth = entry.depth();
            let is_root = entry.is_root();
            let tree_indicator = if entry.is_folder() {
                if entry.is_expanded() { "⌄" } else { "›" }
            } else {
                " "
            };
            let node_indicator = is_root.then(|| {
                view.read(app)
                    .render_session_node_indicator(session.id, index)
            });
            let updated = is_root.then(|| {
                relative_time(
                    session.updated_at.as_unix_millis(),
                    Timestamp::now().as_unix_millis(),
                )
            });
            let descendant_count = descendant_counts.get(&session.id).copied().unwrap_or(0);
            let count_badge =
                (is_root && descendant_count > 0).then(|| descendant_count.to_string());
            let root_active = is_root && session_is_active(session.state);
            let pill = (!is_root).then(|| {
                session_status_pill(session.state, session_tasks.get(&session.id).copied())
            });
            let icon = if is_root {
                if entry.is_folder() {
                    AssetIconName::Workflow
                } else {
                    AssetIconName::MessageSquare
                }
            } else {
                AssetIconName::BotMessageSquare
            };
            let icon_color = if is_root || selected {
                rgb(0x93c5fd)
            } else {
                rgb(0x8f98a6)
            };
            let label_color = if is_root {
                rgb(0xe5e7eb)
            } else {
                rgb(0xb7c0d0)
            };
            let click_view = view.clone();
            let click_session = session.clone();
            ListItem::new(("session-tree-root", index))
                .selected(selected)
                .px_2()
                .py_2()
                .text_size(gpui_kit::rems(0.8125))
                .child(
                    div()
                        .pl(px(depth as f32 * 14.))
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
                        .child(Icon::new(icon).size_4().text_color(icon_color))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(label_color)
                                .when(is_root, |element| element.font_weight(FontWeight::SEMIBOLD))
                                .child(label),
                        )
                        .when_some(count_badge, |element, count| {
                            element.child(
                                div()
                                    .flex_shrink_0()
                                    .px(px(6.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(if root_active {
                                        rgb(0x2563eb)
                                    } else {
                                        rgb(0x30343f)
                                    })
                                    .text_xs()
                                    .text_color(if root_active {
                                        rgb(0x93c5fd)
                                    } else {
                                        rgb(0x64748b)
                                    })
                                    .child(count),
                            )
                        })
                        .when_some(updated, |element, updated| {
                            element.child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(rgb(0x64748b))
                                    .child(updated),
                            )
                        })
                        .when_some(pill, |element, pill| {
                            element.child(
                                div()
                                    .flex_shrink_0()
                                    .px(px(7.))
                                    .py(px(1.))
                                    .rounded_full()
                                    .bg(rgb(pill.background))
                                    .text_xs()
                                    .text_color(rgb(pill.foreground))
                                    .child(pill.label),
                            )
                        })
                        .when_some(node_indicator, |element, indicator| {
                            element.child(indicator)
                        }),
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
            Self::build_project_session_context_menu(
                menu,
                session,
                menu_project.clone(),
                menu_view.clone(),
            )
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
        let mut picker = div().flex().items_center();
        if let Some(state) = &self.model_select {
            picker = picker.child(
                div()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .hover(|style| style.bg(rgb(0x293244)))
                    .child(
                        Select::new(state)
                            .id("session-model-select")
                            .max_w(if phone { px(150.) } else { px(220.) })
                            .menu_width(px(320.))
                            .small()
                            .appearance(false)
                            .accessibility_label("Model for this session")
                            .placeholder("No model is configured")
                            .search_placeholder("Search models"),
                    ),
            );
        }
        picker
    }

    pub(crate) fn render_agent_mode_picker(&self, phone: bool) -> impl IntoElement {
        let mut picker = div().flex().items_center();
        if let Some(state) = &self.agent_mode_select {
            picker = picker.child(
                div()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .hover(|style| style.bg(rgb(0x293244)))
                    .child(
                        Select::new(state)
                            .id("agent-mode-select")
                            .max_w(if phone { px(110.) } else { px(140.) })
                            .small()
                            .appearance(false)
                            .accessibility_label("Agent mode")
                            .placeholder("Select agent mode"),
                    ),
            );
        }
        picker
    }

    fn render_assistant_turn(
        &self,
        turn: &AssistantTurn,
        index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let role = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(3.)).h(px(13.)).rounded_full().bg(rgb(0x9ad7bd)))
            .child(div().text_xs().text_color(rgb(0x9ad7bd)).child("Agent"));
        let mut body = div().px_3().py_2().flex().flex_col().gap_1().child(role);
        let mut part_index = 0;
        while part_index < turn.parts.len() {
            match &turn.parts[part_index] {
                AssistantPart::Reasoning(text) => {
                    if !text.trim().is_empty() {
                        let key = tool_element_id(index, part_index);
                        let expanded = self.expanded_reasoning.contains(&key);
                        let parent_for_toggle = parent.clone();
                        let mut block = div()
                            .id(("reasoning", key))
                            .test_support()
                            .flex()
                            .flex_col()
                            .pl_2()
                            .border_l_2()
                            .border_color(rgb(0x3b4555))
                            .child(
                                Button::new(("reasoning-header", key))
                                    .ghost()
                                    .small()
                                    .w_full()
                                    .accessibility_label("Reasoning")
                                    .on_click(move |_, _, cx| {
                                        parent_for_toggle
                                            .update(cx, |this, cx| this.toggle_reasoning(key, cx));
                                    })
                                    .child(
                                        div()
                                            .w_full()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .text_xs()
                                            .child(
                                                div()
                                                    .text_color(rgb(0x64748b))
                                                    .child(if expanded { "⌄" } else { "›" }),
                                            )
                                            .child(
                                                div().text_color(rgb(0x94a3b8)).child("Reasoning"),
                                            )
                                            .when(!expanded, |element| {
                                                element.child(
                                                    div()
                                                        .flex_1()
                                                        .min_w(px(0.))
                                                        .truncate()
                                                        .text_color(rgb(0x64748b))
                                                        .child(reasoning_preview(text)),
                                                )
                                            }),
                                    ),
                            );
                        if expanded {
                            block = block.child(div().mt_1().child(render_timeline_text(
                                format!("transcript-reasoning-{index}-{part_index}"),
                                text.clone(),
                                0x94a3b8,
                            )));
                        }
                        body = body.child(block);
                    }
                    part_index += 1;
                }
                AssistantPart::Text(text) => {
                    if !text.trim().is_empty() {
                        let mut display = text.clone();
                        if turn.streaming && part_index + 1 == turn.parts.len() {
                            display.push('▍');
                        }
                        body = body.child(render_timeline_text(
                            format!("transcript-assistant-{index}-{part_index}"),
                            display,
                            0xf3f4f6,
                        ));
                    }
                    part_index += 1;
                }
                AssistantPart::Tool(part) => {
                    let mut end = part_index + 1;
                    while end < turn.parts.len() {
                        match &turn.parts[end] {
                            AssistantPart::Tool(next) if next.name == part.name => end += 1,
                            _ => break,
                        }
                    }
                    if end - part_index >= TOOL_GROUP_THRESHOLD {
                        body = body.child(self.render_tool_group(
                            &turn.parts[part_index..end],
                            index,
                            part_index,
                            parent,
                        ));
                    } else {
                        for offset in part_index..end {
                            if let Some(AssistantPart::Tool(part)) = turn.parts.get(offset) {
                                body =
                                    body.child(self.render_tool_part(part, index, offset, parent));
                            }
                        }
                    }
                    part_index = end;
                }
                AssistantPart::Evidence(links) => {
                    body = body.child(Self::render_evidence(links, index, part_index));
                    part_index += 1;
                }
            }
        }
        let last_is_text = matches!(
            turn.parts.last(),
            Some(AssistantPart::Text(text)) if !text.trim().is_empty()
        );
        if turn.streaming && !last_is_text {
            body = body.child(streaming_caret(index));
        }
        body.into_any()
    }

    /// One collapsed row for a run of same-kind tool calls, expandable to the
    /// individual blocks.
    fn render_tool_group(
        &self,
        parts: &[AssistantPart],
        index: usize,
        first_part_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let tools = parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Tool(tool) => Some(tool.as_ref()),
                _ => None,
            })
            .collect::<Vec<&ToolPart>>();
        let Some(first) = tools.first() else {
            return div().into_any();
        };
        let failed = tools
            .iter()
            .any(|tool| tool.status == ToolPartStatus::Failed);
        let running = tools.iter().any(|tool| {
            matches!(
                tool.status,
                ToolPartStatus::Running
                    | ToolPartStatus::AwaitingApproval
                    | ToolPartStatus::AwaitingInput
            )
        });
        let status_color = if failed {
            rgb(0xfca5a5)
        } else if running {
            rgb(0x93c5fd)
        } else {
            rgb(0x9ad7bd)
        };
        let status_label = if failed {
            "failed"
        } else if running {
            "running"
        } else {
            "done"
        };
        let key = tool_element_id(index, first_part_index);
        let expanded = running || failed || self.expanded_tool_groups.contains(&key);
        let total_ms = tools.iter().filter_map(|tool| tool.elapsed_ms).sum::<u64>();
        let duration = (total_ms > 0).then(|| format_duration(total_ms));
        let label = tool_group_label(&first.name, tools.len());
        let parent_for_toggle = parent.clone();
        let mut group = div()
            .id(("tool-group", key))
            .test_support()
            .w_full()
            .pl_2()
            .border_l_2()
            .border_color(status_color.opacity(0.4))
            .flex()
            .flex_col()
            .child(
                Button::new(("tool-group-header", key))
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label(format!("{label}, {status_label}"))
                    .on_click(move |_, _, cx| {
                        parent_for_toggle.update(cx, |this, cx| this.toggle_tool_group(key, cx));
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(
                                Icon::new(tool_icon(&first.name))
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(status_color),
                            )
                            .child(div().text_color(rgb(0x64748b)).child(if expanded {
                                "⌄"
                            } else {
                                "›"
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(rgb(0xdbeafe))
                                    .child(label),
                            )
                            .child(div().text_color(status_color).child(status_label))
                            .when_some(duration, |element, duration| {
                                element.child(div().text_color(rgb(0x64748b)).child(duration))
                            }),
                    ),
            );
        if expanded {
            for offset in first_part_index..(first_part_index + tools.len()) {
                if let Some(AssistantPart::Tool(part)) = parts.get(offset - first_part_index) {
                    group = group.child(self.render_tool_part(part, index, offset, parent));
                }
            }
        }
        group.into_any()
    }

    fn render_evidence(
        links: &[EvidenceText],
        index: usize,
        part_index: usize,
    ) -> gpui_kit::AnyElement {
        let mut list = div().flex().flex_col().gap_1().pt_1();
        for (link_index, link) in links.iter().enumerate() {
            let uri = link.uri.clone();
            let label = if link.label.trim().is_empty() {
                link.uri.clone()
            } else {
                link.label.clone()
            };
            list = list.child(
                Button::new((
                    "evidence",
                    ((index as u64) << 32) | ((part_index as u64) << 16) | link_index as u64,
                ))
                .ghost()
                .small()
                .accessibility_label(format!("Open evidence: {label}"))
                .on_click(move |_, _, _| {
                    let _ = open_external_url(&uri);
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .text_color(rgb(0x93c5fd))
                        .child(
                            Icon::new(AssetIconName::ExternalLink)
                                .size_3()
                                .text_color(rgb(0x93c5fd)),
                        )
                        .child(label),
                ),
            );
        }
        list.into_any()
    }

    fn render_tool_part(
        &self,
        part: &ToolPart,
        index: usize,
        part_index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        let status_color = match part.status {
            ToolPartStatus::Failed => rgb(0xfca5a5),
            ToolPartStatus::Completed => rgb(0x9ad7bd),
            ToolPartStatus::AwaitingApproval | ToolPartStatus::AwaitingInput => rgb(0xfef3c7),
            ToolPartStatus::Running => rgb(0x93c5fd),
            ToolPartStatus::Queued | ToolPartStatus::Cancelled => rgb(0x94a3b8),
        };
        let duration = part.elapsed_ms.map(format_duration);
        // Active, failed, and approval-gated work stays open; finished successes
        // collapse so the transcript stays scannable.
        let expanded = self.expanded_tools.contains(&part.id)
            || matches!(
                part.status,
                ToolPartStatus::Running
                    | ToolPartStatus::AwaitingApproval
                    | ToolPartStatus::AwaitingInput
                    | ToolPartStatus::Failed
            );
        let call_id = part.id;
        let parent_for_toggle = parent.clone();
        let awaiting = part.status == ToolPartStatus::AwaitingApproval
            && self
                .pending_approval
                .as_ref()
                .is_some_and(|call| call.id == part.id);
        let patch_summary = part.output.as_deref().and_then(syntax::patch_summary);
        let mut block = div()
            .id(("tool", tool_element_id(index, part_index)))
            .test_support()
            .w_full()
            .pl_2()
            .border_l_2()
            .border_color(status_color.opacity(0.4))
            .flex()
            .flex_col()
            .child(
                Button::new(("tool-header", tool_element_id(index, part_index)))
                    .ghost()
                    .small()
                    .w_full()
                    .accessibility_label(format!("{}: {}", part.title, part.status.label()))
                    .on_click(move |_, _, cx| {
                        parent_for_toggle.update(cx, |this, cx| this.toggle_tool(call_id, cx));
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(
                                Icon::new(tool_icon(&part.name))
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(status_color),
                            )
                            .child(div().text_color(rgb(0x64748b)).child(if expanded {
                                "⌄"
                            } else {
                                "›"
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_family(mono_font())
                                    .text_color(rgb(0xdbeafe))
                                    .child(part.title.clone()),
                            )
                            .when_some(patch_summary, |element, summary| {
                                element.child(
                                    div()
                                        .font_family(mono_font())
                                        .text_color(rgb(0x64748b))
                                        .child(summary),
                                )
                            })
                            .child(div().text_color(status_color).child(part.status.label()))
                            .when_some(duration, |element, duration| {
                                element.child(div().text_color(rgb(0x64748b)).child(duration))
                            }),
                    ),
            );
        if expanded {
            if let Some(detail) = &part.detail {
                block = block.child(
                    div()
                        .ml(px(20.))
                        .pt_1()
                        .font_family(mono_font())
                        .text_size(gpui_kit::rems(mono_size() / BASE_FONT_SIZE))
                        .text_color(rgb(0x94a3b8))
                        .child(detail.clone()),
                );
            }
            if part.output.is_some() {
                let output_id = tool_element_id(index, part_index);
                block = block.child(
                    div()
                        .ml(px(20.))
                        .mt_1()
                        .w_full()
                        .min_w(px(0.))
                        .border_l_1()
                        .border_color(rgb(0x30343f))
                        .pl_2()
                        .text_color(rgb(0x8f98a6))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .py_1()
                                .child(
                                    div()
                                        .font_family(mono_font())
                                        .text_xs()
                                        .text_color(rgb(0x64748b))
                                        .child(tool_output_language(part).label()),
                                )
                                .child(
                                    Button::new(("copy-tool-output", output_id))
                                        .icon(Icon::new(AssetIconName::Copy))
                                        .ghost()
                                        .xsmall()
                                        .accessibility_label("Copy tool output")
                                        .tooltip("Copy output")
                                        .on_click({
                                            let output = part.output.clone().unwrap_or_default();
                                            move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    output.clone(),
                                                ));
                                            }
                                        }),
                                ),
                        )
                        .child(render_tool_output(("tool-output", output_id), part)),
                );
            }
        }
        if awaiting && !self.approval_request_in_flight {
            let parent_for_approve = parent.clone();
            let parent_for_reject = parent.clone();
            block = block.child(
                div()
                    .ml(px(20.))
                    .mt_1()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new(("approve-tool", tool_element_id(index, part_index)))
                            .label("Approve")
                            .success()
                            .small()
                            .on_click(move |_, _, cx| {
                                parent_for_approve
                                    .update(cx, |this, cx| this.approve_pending_action(cx));
                            }),
                    )
                    .child(
                        Button::new(("reject-tool", tool_element_id(index, part_index)))
                            .label("Reject")
                            .danger()
                            .small()
                            .on_click(move |_, _, cx| {
                                parent_for_reject
                                    .update(cx, |this, cx| this.reject_pending_action(cx));
                            }),
                    ),
            );
        } else if awaiting && self.approval_request_in_flight {
            block = block.child(
                div()
                    .ml(px(20.))
                    .mt_1()
                    .text_xs()
                    .text_color(rgb(0x94a3b8))
                    .child("Submitting approval..."),
            );
        }
        block.into_any()
    }

    fn toggle_tool(&mut self, tool_id: ToolCallId, cx: &mut Context<Self>) {
        if !self.expanded_tools.remove(&tool_id) {
            self.expanded_tools.insert(tool_id);
        }
        cx.notify();
    }

    fn toggle_tool_group(&mut self, key: u64, cx: &mut Context<Self>) {
        if !self.expanded_tool_groups.remove(&key) {
            self.expanded_tool_groups.insert(key);
        }
        cx.notify();
    }

    fn toggle_reasoning(&mut self, key: u64, cx: &mut Context<Self>) {
        if !self.expanded_reasoning.remove(&key) {
            self.expanded_reasoning.insert(key);
        }
        cx.notify();
    }

    pub(crate) fn render_timeline_item(
        &self,
        item: &TimelineItem,
        index: usize,
        parent: &Entity<LoomView>,
    ) -> gpui_kit::AnyElement {
        match item {
            TimelineItem::User(text) => div()
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(3.)).h(px(13.)).rounded_full().bg(rgb(0x60a5fa)))
                        .child(div().text_xs().text_color(rgb(0xbfdbfe)).child("You")),
                )
                .child(div().mt_1().child(render_timeline_text(
                    format!("transcript-user-{index}"),
                    text.clone(),
                    0xf3f4f6,
                )))
                .into_any(),
            TimelineItem::Assistant(turn) => self.render_assistant_turn(turn, index, parent),
            TimelineItem::System(note) => {
                let (surface, accent) = match note.tone {
                    SystemTone::Error => (rgb(ERROR_CARD_SURFACE), rgb(ERROR_CARD_ACCENT)),
                    SystemTone::Input => (rgb(0x241f3b), rgb(0xc4b5fd)),
                    SystemTone::Neutral => (rgb(0x191c22), rgb(0x64748b)),
                };
                let text_color = match note.tone {
                    SystemTone::Error => ERROR_CARD_FOREGROUND,
                    SystemTone::Input => 0xe9d5ff,
                    SystemTone::Neutral => 0x94a3b8,
                };
                let mut card = div()
                    .px_3()
                    .py_2()
                    .rounded_sm()
                    .bg(surface)
                    .text_color(rgb(text_color));
                if let Some(heading) = &note.heading {
                    card = card.child(div().text_xs().text_color(accent).child(heading.clone()));
                }
                card = card.child(render_timeline_text(
                    format!("timeline-system-{index}"),
                    note.text.clone(),
                    text_color,
                ));
                if note.retryable {
                    card = card.child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(accent)
                            .child("This operation can be retried."),
                    );
                }
                card.into_any()
            }
            TimelineItem::ProjectMessageContext(text) => div()
                .mx_3()
                .my_1()
                .px_3()
                .py_2()
                .border_l_2()
                .border_color(rgb(0x3b4555))
                .text_color(rgb(0xcbd5e1))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f98a6))
                        .child("Project agent context"),
                )
                .child(render_timeline_text(
                    format!("transcript-project-context-{index}"),
                    text.clone(),
                    0xcbd5e1,
                ))
                .into_any(),
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
                let mut card = div()
                    .mx_3()
                    .my_1()
                    .px_3()
                    .py_1()
                    .border_l_2()
                    .border_color(accent.opacity(0.45))
                    .text_color(rgb(0xb7c0d0))
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(format!(
                        "{kind} · {} → {} · #{}",
                        participant(message.sender_session_id),
                        participant(message.target_session_id),
                        message.project_sequence
                    )));
                if !message.body.trim().is_empty() {
                    card = card.child(render_timeline_text(
                        format!("project-message-{}", message.message_id),
                        message.body.clone(),
                        0xb7c0d0,
                    ));
                }
                card.into_any()
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
        }
    }

    fn timeline_entity(&mut self, cx: &mut Context<Self>) -> Entity<TimelineView> {
        if let Some(timeline_view) = &self.timeline_view {
            return timeline_view.clone();
        }

        let parent = cx.entity();
        let timeline_view = cx.new(|cx| {
            let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
            TimelineView::new(parent, scroller)
        });
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
                .bg(rgb(0x293244))
                .font_family(mono_font())
                .text_xs()
                .text_color(rgb(0x93c5fd))
                .child(format!(
                    "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@"
                )),
            ReviewRow::Line(line) => {
                let (marker, background, foreground) = match line.kind {
                    GitDiffLineKind::Added => ("+", 0x24543d, 0xbbf7d0),
                    GitDiffLineKind::Removed => ("−", 0x542936, 0xfecaca),
                    GitDiffLineKind::Context => (" ", 0x17191f, 0xcbd5e1),
                };
                div()
                    .w_full()
                    .min_h(px(22.))
                    .flex()
                    .items_start()
                    .bg(rgb(background))
                    .font_family(mono_font())
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
            "Describe a task, or type / for commands"
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
        let value = composer.read(cx).value().to_string();
        let height = composer_height(&value);
        let view = cx.entity();
        let completion_rows = match &self.composer_completion {
            Some(completion) if completion.kind == CompletionKind::Command => {
                commands_matching(&completion.query)
                    .into_iter()
                    .map(|command| {
                        (
                            format!("/{}", command.name),
                            command.title.to_owned(),
                            command.description.to_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
            }
            Some(completion) => self
                .file_completion_candidates()
                .into_iter()
                .filter(|path| path.contains(&completion.query))
                .map(|path| (format!("@{path}"), path, String::new()))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        div()
            .w_full()
            .p_3()
            .bg(rgb(0x17191f))
            .border_t_1()
            .border_color(rgb(0x30343f))
            .child(self.render_run_status())
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
            .when_some(self.composer_completion.clone(), |element, completion| {
                element.child(
                    div()
                        .id("composer-completions")
                        .test_support()
                        .w_full()
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .rounded_lg()
                        .bg(rgb(0x10141b))
                        .border_1()
                        .border_color(rgb(0x3b4555))
                        .mb_2()
                        .children(completion_rows.into_iter().enumerate().map(
                            |(index, (insert, title, description))| {
                                let selected = index == completion.selected;
                                let view = view.clone();
                                div()
                                    .id(("completion-row", index))
                                    .px_3()
                                    .py_1()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .cursor_pointer()
                                    .when(selected, |element| element.bg(rgb(0x202b3b)))
                                    .hover(|style| style.bg(rgb(0x202b3b)))
                                    .child(
                                        div()
                                            .font_family(mono_font())
                                            .text_sm()
                                            .text_color(rgb(0x93c5fd))
                                            .child(insert),
                                    )
                                    .child(div().text_xs().text_color(rgb(0xe5e7eb)).child(title))
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x64748b))
                                            .child(description),
                                    )
                                    .on_click(move |_, window, cx| {
                                        view.update(cx, |this, cx| {
                                            if let Some(completion) =
                                                this.composer_completion.as_mut()
                                            {
                                                completion.selected = index;
                                            }
                                            this.accept_composer_completion(window, cx);
                                        });
                                    })
                            },
                        )),
                )
            })
            .child(
                div()
                    .id("composer-input-box")
                    .w_full()
                    .rounded_lg()
                    .bg(rgb(0x10141b))
                    .border_1()
                    .border_color(rgb(0x3b4555))
                    .text_color(rgb(0xe5e7eb))
                    .child(
                        div().p_3().child(
                            Textarea::new(composer)
                                .aria_label(placeholder)
                                .h(px(height))
                                .appearance(false)
                                .bordered(false),
                        ),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .items_center()
                            .justify_between()
                            .border_t_1()
                            .border_color(rgb(0x20242c))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(self.render_agent_mode_picker(layout.phone))
                                    .child(self.render_model_picker(layout.phone))
                                    .child(
                                        Button::new("open-command-palette")
                                            .icon(Icon::new(AssetIconName::Command))
                                            .label("K")
                                            .ghost()
                                            .xsmall()
                                            .tooltip("Open the command palette")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_command_palette(cx);
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .when(self.run_can_interrupt(), |element| {
                                        element.child(
                                            Button::new("interrupt-run")
                                                .label("Stop")
                                                .danger()
                                                .small()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.interrupt_active_run(cx);
                                                })),
                                        )
                                    })
                                    .child(
                                        Button::new("send-message")
                                            .icon(Icon::new(AssetIconName::ArrowUp))
                                            .primary()
                                            .small()
                                            .tooltip("Send (↵)")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.submit_composer(cx);
                                            })),
                                    ),
                            ),
                    ),
            )
    }

    /// The single line that carries run progress: spinner, state, elapsed, and
    /// the interrupt hint. Run state lives here instead of the session header.
    fn render_run_status(&self) -> gpui_kit::Div {
        let running = self.run_is_active();
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .mb_2()
            .min_h(px(16.))
            .text_xs();
        if running || self.sending_message || self.pending_input.is_some() {
            let color = self
                .run_state
                .map(run_state_color)
                .unwrap_or_else(|| rgb(0x93c5fd));
            row = row.child(Spinner::new().small().color(color.into()));
        }
        if let Some(state) = self.run_state {
            let label = if state == AgentRunState::Paused && self.project_has_live_children() {
                "Waiting for sub-agents".to_owned()
            } else {
                run_state_label(Some(state)).to_owned()
            };
            row = row.child(div().text_color(run_state_color(state)).child(label));
        } else if self.sending_message {
            row = row.child(div().text_color(rgb(0x93c5fd)).child("Sending"));
        } else if self.pending_input.is_some() {
            row = row.child(
                div()
                    .text_color(rgb(0xfbbf24))
                    .child("Waiting for your answer"),
            );
        } else {
            row = row.child(div().text_color(rgb(0x64748b)).child("Ready"));
        }
        if let Some(elapsed) = self.run_elapsed_label() {
            row = row.child(
                div()
                    .font_family(mono_font())
                    .text_color(rgb(0x64748b))
                    .child(elapsed),
            );
        }
        row = row.child(div().flex_1());
        if running {
            row = row.child(div().text_color(rgb(0x64748b)).child("esc to interrupt"));
        }
        row
    }

    fn run_elapsed_label(&self) -> Option<String> {
        let run = self.active_run.as_ref()?;
        let end = run.completed_at.unwrap_or_else(Timestamp::now);
        let milliseconds = end
            .as_unix_millis()
            .saturating_sub(run.started_at.as_unix_millis());
        Some(format_duration(milliseconds))
    }

    fn render_command_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self
            .command_palette_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let matches = commands_matching(&query);
        let view = cx.entity();
        let rows = matches
            .into_iter()
            .enumerate()
            .map(|(index, command)| {
                let selected = index == self.command_palette_selection;
                let name = command.name;
                let view = view.clone();
                div()
                    .id(("palette-row", index))
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .cursor_pointer()
                    .when(selected, |element| element.bg(rgb(0x202b3b)))
                    .hover(|style| style.bg(rgb(0x202b3b)))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(0xf3f4f6))
                            .child(command.title),
                    )
                    .when_some(command.shortcut, |element, shortcut| {
                        element.child(div().text_xs().text_color(rgb(0x64748b)).child(shortcut))
                    })
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.command_palette_open = false;
                            this.command_palette_selection = 0;
                            this.run_command(name, None, cx);
                        });
                    })
            })
            .collect::<Vec<_>>();
        div()
            .id("command-palette-backdrop")
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .size_full()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(110.))
            .bg(gpui_kit::hsla(0., 0., 0., 0.5))
            .on_click(cx.listener(|this, _, _, cx| this.close_command_palette(cx)))
            .child(
                div()
                    .id("command-palette")
                    .test_support()
                    .w(px(560.))
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .bg(rgb(0x1b1d24))
                    .border_1()
                    .border_color(rgb(0x3b4555))
                    .shadow_lg()
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .border_b_1()
                            .border_color(rgb(0x293244))
                            .child(
                                Icon::new(AssetIconName::Command)
                                    .size_4()
                                    .text_color(rgb(0x64748b)),
                            )
                            .child(div().flex_1().when_some(
                                self.command_palette_input.as_ref(),
                                |element, input| {
                                    element.child(KitInput::new(input).id("command-palette-input"))
                                },
                            )),
                    )
                    .child(
                        div()
                            .id("command-palette-list")
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .children(rows),
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
                    .child(if dialog.is_project {
                        "Rename project"
                    } else {
                        "Rename session"
                    }),
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
                .child("Start a new project with no files or repositories.");
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
                "New project"
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
                                "Choose what the new project starts with."
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
                                        .label("Empty project")
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
                                        "Create project"
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
        let default_model_body = self.default_model_select.as_ref().map_or_else(
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

        let section = self.settings_section;
        let content: gpui_kit::AnyElement = match section {
            SettingsSection::Agents => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("AGENTS"))
                .child(
                    settings_card()
                        .child(settings_row(
                            "Auto-approve non-destructive actions",
                            "In Agent and Edit, writes, commands, and network won't prompt",
                            Switch::new("session-auto-approve-toggle")
                                .checked(self.auto_approve_actions)
                                .disabled(
                                    !self.is_connected()
                                        || self.approval_settings_request_in_flight,
                                )
                                .accessibility_label("Auto-approve non-destructive actions")
                                .on_change({
                                    let view = cx.entity();
                                    move |_checked, _window, cx| {
                                        view.update(cx, |view, cx| {
                                            view.toggle_auto_approve_actions(cx);
                                        });
                                    }
                                }),
                            true,
                        ))
                        .child(settings_row(
                            "Default model for new sessions",
                            "Used when a session has no model of its own",
                            div().w(px(320.)).child(default_model_body),
                            false,
                        ))
                        .child(settings_row(
                            "Session indicator pulse threshold",
                            "Pulse when CPU usage is above this value",
                            settings_stepper(
                                Button::new("cpu-pulse-threshold-decrease")
                                    .label("-")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.cpu_pulse_threshold_percent
                                                == 0,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_cpu_pulse_threshold(-1, cx);
                                    })),
                                format!(
                                    "{}%",
                                    self.workspace_config.cpu_pulse_threshold_percent.min(100)
                                ),
                                Button::new("cpu-pulse-threshold-increase")
                                    .label("+")
                                    .small()
                                    .disabled(
                                        !self.is_connected()
                                            || self.workspace_config.cpu_pulse_threshold_percent
                                                >= 100,
                                    )
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.adjust_cpu_pulse_threshold(1, cx);
                                    })),
                            ),
                            false,
                        ))
                        .child(settings_row(
                            "Parallel project agents",
                            "Maximum delegated agents running at once",
                            settings_stepper(
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
                                self.workspace_config.project_agent_concurrency.to_string(),
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
                            false,
                        )),
                )
                .into_any_element(),
            SettingsSection::Providers => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("PROVIDERS"))
                .child(
                    settings_card().child(
                        div()
                            .w_full()
                            .flex()
                            .items_start()
                            .gap_3()
                            .px_4()
                            .py_3()
                            .child(
                                div()
                                    .w(px(30.))
                                    .h(px(30.))
                                    .flex_shrink_0()
                                    .rounded_md()
                                    .bg(rgb(0x20242c))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(AssetIconName::Globe)
                                            .size_4()
                                            .text_color(rgb(0x93c5fd)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(div().text_sm().child("GitHub"))
                                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child(
                                        "Browse and clone repositories, and add GitHub \
                                                 Copilot as a model provider.",
                                    )),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap_3()
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
                                    )
                                    .when(
                                        !self.github_connected && self.login_enabled,
                                        |element| {
                                            element.child(
                                                Button::new("connect-github-account")
                                                    .label("Connect GitHub")
                                                    .small()
                                                    .on_click(
                                                        cx.listener(Self::toggle_github_login),
                                                    ),
                                            )
                                        },
                                    ),
                            ),
                    ),
                )
                .into_any_element(),
            SettingsSection::Workers => {
                let mut card = settings_card();
                if self.worker_nodes.is_empty() {
                    card = card.child(
                        div()
                            .px_4()
                            .py_3()
                            .text_xs()
                            .text_color(rgb(0x8f98a6))
                            .child("No worker connected. Add one below to load your sessions."),
                    );
                }
                card = card.children(self.worker_nodes.iter().enumerate().map(|(index, node)| {
                    let id = node.id;
                    let status = &node.status;
                    let node_id = status.node_id.clone();
                    let resources = &status.resources;
                    let connection_label = match node.connection_state {
                        WorkerConnectionState::Disconnected => "not connected",
                        WorkerConnectionState::Connecting => "connecting",
                        WorkerConnectionState::Connected if status.online => "connected · online",
                        WorkerConnectionState::Connected => "connected · offline",
                        WorkerConnectionState::Failed => "connection failed",
                    };
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .py_3()
                        .when(index > 0, |element| {
                            element.border_t_1().border_color(rgb(0x242833))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .child(div().text_sm().text_color(rgb(0xe5e7eb)).child(format!(
                                    "{} · {}",
                                    worker_node_display_name(node),
                                    connection_label,
                                )))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x8f98a6))
                                        .child(format_worker_node_resources(resources)),
                                )
                                .when_some(node.connection_detail.as_deref(), |element, detail| {
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
                                }),
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
                }));

                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_section_heading("WORKERS"))
                    .when_some(self.browser_startup_error.as_deref(), |element, error| {
                        element.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xfca5a5))
                                .child(error.to_owned()),
                        )
                    })
                    .child(card)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div().flex_1().child(
                                    KitInput::new(self.node_input_state.as_ref().expect(
                                        "worker connection input initialized before rendering",
                                    ))
                                    .id("worker-node-connection-input")
                                    .small(),
                                ),
                            )
                            .child(
                                Button::new("connect-worker-node")
                                    .label("Connect")
                                    .small()
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.connect_worker_node(cx)),
                                    ),
                            ),
                    )
                    .child(div().text_xs().text_color(rgb(0x64748b)).child(
                        "Use: ws://host:port/ws token · URLs are shared; access tokens are not",
                    ))
                    .into_any_element()
            }
            SettingsSection::Appearance => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_section_heading("APPEARANCE"))
                .child(
                    settings_card()
                        .child(settings_row(
                            "Theme",
                            "Follow the system or choose a palette",
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .p_1()
                                .rounded_md()
                                .bg(rgb(0x20242c))
                                .children(ThemeChoice::ALL.into_iter().enumerate().map(
                                    |(index, choice)| {
                                        let selected = choice == self.theme_choice;
                                        div()
                                            .id(("theme-choice", index))
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
                                            .child(choice.label())
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.select_theme(choice, window, cx);
                                            }))
                                    },
                                )),
                            true,
                        ))
                        .child(settings_row(
                            "Font size",
                            "Relative to the system display scale",
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Button::new("font-scale-decrease")
                                        .label("−")
                                        .small()
                                        .disabled(self.font_scale_percent <= MIN_FONT_SCALE_PERCENT)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.adjust_font_scale(
                                                -FONT_SCALE_STEP_PERCENT,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    div()
                                        .w(px(52.))
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
                                            this.adjust_font_scale(
                                                FONT_SCALE_STEP_PERCENT,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    Button::new("font-scale-reset")
                                        .label("Reset")
                                        .ghost()
                                        .small()
                                        .disabled(
                                            self.font_scale_percent == DEFAULT_FONT_SCALE_PERCENT,
                                        )
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.set_font_scale_percent(
                                                DEFAULT_FONT_SCALE_PERCENT,
                                                window,
                                                cx,
                                            );
                                        })),
                                ),
                            false,
                        )),
                )
                .into_any_element(),
        };

        div()
            .id("settings-dialog")
            .size_full()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(0x111318))
            .text_color(rgb(0xe5e7eb))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(rgb(0x242833))
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
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(settings_nav(section, cx))
                    .child(
                        div()
                            .id("settings-content")
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .p_6()
                            .overflow_y_scroll()
                            .child(content),
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
                    .child(div().text_xs().text_color(rgb(0x8f98a6)).child("Projects"))
                    .child(self.render_new_session_button(view, cx)),
            )
            .when_some(self.session_filter_input.as_ref(), |element, input| {
                element.child(KitInput::new(input).id("session-filter").small())
            })
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
                            .accessibility_label("Account")
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
                            .accessibility_label("Settings")
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
                                    div().text_sm().text_color(rgb(0xf3f4f6)).child("Projects"),
                                ))
                                .child(
                                    div()
                                        .mt_1()
                                        .text_xs()
                                        .text_color(rgb(0x8f98a6))
                                        .child("Projects"),
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
                                        .child("Connect a worker to load projects."),
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
                                            .child("Connect a worker to create a project."),
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
#[cfg(test)]
mod tests;
