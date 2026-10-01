use super::*;

pub(crate) fn responsive_layout(width: Pixels) -> ResponsiveLayout {
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

/// The recommended minimum touch target for phone controls. Phone layouts use
/// this for navigation rows, header actions, and the composer so the interface
/// stays usable with a finger instead of a pointer.
pub(crate) const PHONE_TOUCH_TARGET: Pixels = px(44.);

impl ResponsiveLayout {
    /// The interactive control height for this layout.
    pub(crate) fn control_size(self) -> Pixels {
        if self.phone {
            PHONE_TOUCH_TARGET
        } else {
            px(30.)
        }
    }

    /// The vertical padding for a navigation list row.
    pub(crate) fn nav_row_padding(self) -> Pixels {
        if self.phone { px(14.) } else { px(8.) }
    }

    /// The font size for navigation list labels.
    pub(crate) fn nav_row_font_size(self) -> gpui_kit::Rems {
        if self.phone {
            gpui_kit::rems(16. / BASE_FONT_SIZE)
        } else {
            gpui_kit::rems(0.8125)
        }
    }
}

/// The part of the layout viewport hidden behind an on-screen keyboard or other
/// platform overlay. The browser keeps the layout viewport full height while
/// only the visual viewport shrinks, so the composer must reserve this space to
/// stay above the keyboard.
pub(crate) fn bottom_occlusion(window: &Window) -> Pixels {
    let visible = window.fully_visible_bounds();
    (window.viewport_size().height - visible.bottom()).max(px(0.))
}

pub(crate) fn commands_matching(query: &str) -> Vec<&'static CommandSpec> {
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
pub(crate) fn completion_for_value(value: &str) -> Option<ComposerCompletion> {
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
pub(crate) fn replace_command_token(value: &str, name: &str) -> String {
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
pub(crate) fn replace_last_token(value: &str, replacement: &str) -> String {
    let start = value
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    format!("{}{}", &value[..start], replacement)
}

/// A compact relative time for a session, e.g. `2h ago`.
pub(crate) fn relative_time(millis: u64, now: u64) -> String {
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

/// A rough auto-grow height for the composer, in pixels. Phone layouts use a
/// taller minimum so the field stays a comfortable touch target without
/// changing the line growth on larger screens.
pub(crate) fn composer_height(value: &str, phone: bool) -> f32 {
    let lines = value.lines().count().clamp(1, 8);
    let min_height = if phone { 44. } else { 28. };
    min_height + (lines as f32 - 1.) * 20.
}

/// The accent color for a run state.
pub(crate) fn run_state_color(state: AgentRunState) -> gpui_kit::Rgba {
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
pub(crate) fn session_is_active(state: AgentSessionState) -> bool {
    matches!(
        state,
        AgentSessionState::Planning
            | AgentSessionState::Executing
            | AgentSessionState::AwaitingApproval
            | AgentSessionState::NeedsInput
            | AgentSessionState::Evaluating
    )
}

pub(crate) fn review_panel_is_visible(
    layout: ResponsiveLayout,
    review_open: bool,
    session_count: usize,
    settings_open: bool,
    github_login_open: bool,
) -> bool {
    !layout.phone && review_open && session_count > 0 && !settings_open && !github_login_open
}

pub(crate) fn session_header_title() -> gpui_kit::Div {
    div().flex().flex_1().min_w(px(0.)).flex_col()
}

pub(crate) fn session_header_actions() -> gpui_kit::Div {
    div().flex().flex_shrink_0().items_center().gap_1()
}

pub(crate) fn empty_session_snapshot(workspace_id: WorkspaceId) -> AgentSessionSnapshot {
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

pub(crate) fn header_tooltip(
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

pub(crate) fn format_bytes(value: Option<u64>) -> String {
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

pub(crate) fn format_percentage(value: Option<u8>) -> String {
    value
        .filter(|value| *value <= 100)
        .map_or_else(|| "n/a".to_owned(), |value| format!("{value}%"))
}

pub(crate) fn format_worker_node_resources(resources: &WorkerNodeResources) -> String {
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

pub(crate) fn worker_node_for_id<'a>(
    nodes: &'a [WorkerNodeEntry],
    node_id: Option<&str>,
) -> Option<&'a WorkerNodeEntry> {
    let node_id = node_id?;
    nodes.iter().find(|node| node.status.node_id == node_id)
}

pub(crate) fn worker_node_name_for_id(
    nodes: &[WorkerNodeEntry],
    node_names: &BTreeMap<String, String>,
    node_id: Option<&str>,
) -> String {
    worker_node_for_id(nodes, node_id)
        .map(worker_node_display_name)
        .or_else(|| node_id.and_then(|node_id| node_names.get(node_id).cloned()))
        .unwrap_or_else(|| "Worker node unavailable".to_owned())
}

pub(crate) fn worker_node_display_name(node: &WorkerNodeEntry) -> String {
    let role = if node.is_local {
        "Local backend"
    } else {
        "External worker"
    };
    format!("{role} · {}", node.status.name)
}

pub(crate) fn format_session_resource_percentages(status: Option<&WorkerNodeStatus>) -> String {
    let resources = status.map(|status| &status.resources);
    format!(
        "CPU {} · RAM {}",
        format_percentage(resources.and_then(|resources| resources.cpu_usage_percent)),
        format_percentage(resources.and_then(|resources| resources.memory_usage_percent)),
    )
}

pub(crate) fn session_owner_status<'a>(
    nodes: &'a [WorkerNodeEntry],
    session_node_ids: &BTreeMap<AgentSessionId, String>,
    session_id: AgentSessionId,
) -> Option<&'a WorkerNodeEntry> {
    worker_node_for_id(nodes, session_node_ids.get(&session_id).map(String::as_str))
}

pub(crate) fn session_node_pulse(
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

pub(crate) fn next_severe_load_streak(current: u8, resources: &WorkerNodeResources) -> u8 {
    match (resources.cpu_usage_percent, resources.memory_usage_percent) {
        (Some(cpu), Some(memory)) if cpu > 90 && cpu <= 100 && memory > 90 && memory <= 100 => {
            current.saturating_add(1)
        }
        _ => 0,
    }
}

pub(crate) fn adjusted_cpu_pulse_threshold(current: u8, delta: i8) -> u8 {
    (i16::from(current.min(100)) + i16::from(delta)).clamp(0, 100) as u8
}

pub(crate) fn adjusted_project_agent_concurrency(current: u8, delta: i8) -> u8 {
    (i16::from(current) + i16::from(delta)).clamp(
        i16::from(loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY),
        i16::from(loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY),
    ) as u8
}

pub(crate) fn session_node_indicator_state(
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
pub(crate) fn order_session_nodes(
    mut nodes: Vec<(String, String)>,
    default_node_id: &str,
) -> Vec<(String, String)> {
    nodes.sort_by_key(|(node_id, _)| node_id != default_node_id);
    nodes
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn load_transcript_page_sync(
    connection: &ClientConnection,
    run_id: RunId,
    before_ordinal: Option<u64>,
) -> Result<TranscriptPage, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunTranscriptPage {
            run_id,
            before_ordinal,
            limit: MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE,
        },
    )));
    let (messages, next_before, has_older) = match response.result? {
        ServerResponse::Run(RunResponse::AgentRunTranscriptPage {
            run_id: response_run_id,
            messages,
            next_before,
            has_older,
        }) if response_run_id == run_id => (messages, next_before, has_older),
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

pub(crate) fn timeline_items_from_messages(
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
    // A hidden project message still ends the previous agent turn, so the reply
    // that follows reads as its own entry instead of merging into the turn that
    // preceded the message.
    let mut project_boundary = false;
    for (_, entry) in entries {
        match entry {
            Entry::Message(message) => match message.role {
                // Inter-agent project messages are orchestration traffic. The
                // receiving agent decides whether the user needs to act on
                // anything, so the raw message stays out of the transcript.
                MessageRole::User if message.name.as_deref() == Some("loom_project_message") => {
                    project_boundary = true;
                }
                MessageRole::User => timeline.push(TimelineItem::User(message.content)),
                MessageRole::Assistant => {
                    let reasoning = message
                        .reasoning_content
                        .as_deref()
                        .filter(|reasoning| !reasoning.is_empty());
                    let has_content = !message.content.is_empty();
                    if project_boundary
                        && (reasoning.is_some() || has_content || !message.tool_calls.is_empty())
                    {
                        timeline.push(TimelineItem::Assistant(AssistantTurn::default()));
                    }
                    project_boundary = false;
                    if reasoning.is_some() || has_content {
                        // Merge into the trailing assistant turn for the whole
                        // tool-using response: text, reasoning, and tool cycles
                        // stay under one agent entry. Another role starts a new
                        // turn.
                        let merges = matches!(
                            timeline.last(),
                            Some(TimelineItem::Assistant(turn))
                                if matches!(
                                    turn.parts.last(),
                                    None | Some(AssistantPart::Reasoning(_))
                                        | Some(AssistantPart::Text(_))
                                        | Some(AssistantPart::Tool(_))
                                )
                        );
                        if !merges && reasoning.is_none() {
                            // A restored text-only turn uses the standard shape
                            // so consecutive plain text still reads as one turn.
                            timeline.push(TimelineItem::Assistant(AssistantTurn::text(
                                message.content.clone(),
                            )));
                        } else {
                            if !merges {
                                timeline.push(TimelineItem::Assistant(AssistantTurn::default()));
                            }
                            if let Some(TimelineItem::Assistant(turn)) = timeline.last_mut() {
                                if let Some(reasoning) = reasoning {
                                    match turn.parts.last_mut() {
                                        Some(AssistantPart::Reasoning(existing)) => {
                                            existing.push_str(reasoning);
                                        }
                                        _ => turn
                                            .parts
                                            .push(AssistantPart::Reasoning(reasoning.to_owned())),
                                    }
                                }
                                if has_content {
                                    match turn.parts.last_mut() {
                                        Some(AssistantPart::Text(existing)) => {
                                            if !existing.is_empty() {
                                                existing.push_str("\n\n");
                                            }
                                            existing.push_str(&message.content);
                                        }
                                        _ => turn
                                            .parts
                                            .push(AssistantPart::Text(message.content.clone())),
                                    }
                                }
                            }
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
                    // A transcript message records only the result, never the
                    // call arguments, so any title it produces is a fallback.
                    // When the activity already described the call, keep that
                    // richer description instead of the argument-less one.
                    if has_tool_part(&timeline, call.id) {
                        part.title.clear();
                        part.detail = None;
                    }
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

pub(crate) fn unseen_transcript_messages(
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

pub(crate) fn merge_node_sessions(
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

pub(crate) fn session_id_for_request(
    request: &ClientRequest,
    active_session_id: AgentSessionId,
) -> Option<AgentSessionId> {
    match request {
        ClientRequest::Session(SessionRequest::GetAgentSession { session_id })
        | ClientRequest::Session(SessionRequest::GetAgentSessionSnapshot { session_id })
        | ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata { session_id })
        | ClientRequest::Session(SessionRequest::GetAgentSessionInitialState { session_id })
        | ClientRequest::Session(SessionRequest::RenameAgentSession { session_id, .. })
        | ClientRequest::Session(SessionRequest::ArchiveAgentSession { session_id })
        | ClientRequest::Events(EventsRequest::GetRecentSessionEvents { session_id, .. })
        | ClientRequest::Run(RunRequest::StartSessionAgentRun { session_id, .. })
        | ClientRequest::Run(RunRequest::StartSessionAgentRunWithOptions { session_id, .. })
        | ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
            session_id,
            ..
        })
        | ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory {
            session_id, ..
        })
        | ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories { session_id })
        | ClientRequest::Filesystem(FilesystemRequest::DetachSessionDirectory {
            session_id, ..
        })
        | ClientRequest::Repository(RepositoryRequest::ListSessionRepositories { session_id })
        | ClientRequest::Repository(RepositoryRequest::DetachSessionRepository {
            session_id,
            ..
        })
        | ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemSnapshot {
            session_id,
        })
        | ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemChanges {
            session_id,
            ..
        })
        | ClientRequest::Filesystem(FilesystemRequest::ReadSessionFile { session_id, .. })
        | ClientRequest::Filesystem(FilesystemRequest::ApplySessionFilesystemEdit {
            session_id,
            ..
        })
        | ClientRequest::Filesystem(FilesystemRequest::TakeSessionFilesystemControl {
            session_id,
            ..
        })
        | ClientRequest::Filesystem(FilesystemRequest::CreateSessionCheckpoint {
            session_id,
            ..
        })
        | ClientRequest::Filesystem(FilesystemRequest::RevertSessionCheckpoint {
            session_id,
            ..
        })
        | ClientRequest::Filesystem(FilesystemRequest::UndoSessionEdit { session_id })
        | ClientRequest::Filesystem(FilesystemRequest::GetSessionContextFiles { session_id })
        | ClientRequest::Repository(RepositoryRequest::GetSessionVcsStatus {
            session_id, ..
        })
        | ClientRequest::Repository(RepositoryRequest::GetSessionVcsDiff { session_id, .. })
        | ClientRequest::Repository(RepositoryRequest::GetSessionVcsBranches {
            session_id, ..
        })
        | ClientRequest::Repository(RepositoryRequest::GetSessionVcsConflicts {
            session_id, ..
        })
        | ClientRequest::Terminal(TerminalRequest::OpenSessionTerminal { session_id, .. })
        | ClientRequest::Terminal(TerminalRequest::WriteSessionTerminalInput {
            session_id, ..
        })
        | ClientRequest::Terminal(TerminalRequest::ResizeSessionTerminal { session_id, .. })
        | ClientRequest::Terminal(TerminalRequest::GetSessionTerminalEvents {
            session_id, ..
        })
        | ClientRequest::Terminal(TerminalRequest::CancelSessionTerminal { session_id, .. })
        | ClientRequest::Task(TaskRequest::StartSessionTask { session_id, .. })
        | ClientRequest::Task(TaskRequest::ListSessionTasks { session_id })
        | ClientRequest::Task(TaskRequest::GetSessionTask { session_id, .. })
        | ClientRequest::Task(TaskRequest::GetSessionTaskEvents { session_id, .. })
        | ClientRequest::Task(TaskRequest::CancelSessionTask { session_id, .. })
        | ClientRequest::Task(TaskRequest::GetSessionTaskEvidence { session_id, .. })
        | ClientRequest::Session(SessionRequest::SetSessionApprovalPolicy { session_id, .. })
        | ClientRequest::Session(SessionRequest::ForkAgentSession { session_id, .. })
        | ClientRequest::Usage(UsageRequest::GetSessionUsage { session_id }) => Some(*session_id),
        ClientRequest::Project(ProjectRequest::ControlProjectChild {
            manager_session_id, ..
        })
        | ClientRequest::Project(ProjectRequest::GetProjectChildReview {
            manager_session_id,
            ..
        })
        | ClientRequest::Project(ProjectRequest::IntegrateProjectChild {
            manager_session_id,
            ..
        })
        | ClientRequest::Project(ProjectRequest::CleanupProjectChildWorktree {
            manager_session_id,
            ..
        }) => Some(*manager_session_id),
        ClientRequest::Events(EventsRequest::GetSessionEvents { session_id, .. }) => {
            Some(session_id.unwrap_or(active_session_id))
        }
        ClientRequest::Run(RunRequest::GetAgentRun { .. })
        | ClientRequest::Run(RunRequest::GetAgentRunSnapshot { .. })
        | ClientRequest::Run(RunRequest::GetRunCheckpoint { .. })
        | ClientRequest::Run(RunRequest::ApproveAgentAction { .. })
        | ClientRequest::Run(RunRequest::RejectAgentAction { .. })
        | ClientRequest::Run(RunRequest::SendAgentMessage { .. })
        | ClientRequest::Run(RunRequest::InterruptAgentRun { .. })
        | ClientRequest::Run(RunRequest::RetryAgentStep { .. })
        | ClientRequest::Run(RunRequest::PauseAgentRun { .. })
        | ClientRequest::Run(RunRequest::ResumeAgentRun { .. })
        | ClientRequest::Run(RunRequest::RetryAgentFromCheckpoint { .. })
        | ClientRequest::Usage(UsageRequest::GetRunUsage { .. })
        | ClientRequest::Context(ContextRequest::InspectAgentContext { .. })
        | ClientRequest::Run(RunRequest::AttachRunEvidence { .. }) => Some(active_session_id),
        _ => None,
    }
}

pub(crate) fn assigned_node_id(
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

pub(crate) fn validate_model_for_node(
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

pub(crate) fn model_choice_labels(
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
pub(crate) fn open_external_url(url: &str) -> Result<(), std::io::Error> {
    open::that(url)
}

#[cfg(target_family = "wasm")]
pub(crate) fn open_external_url(url: &str) -> Result<(), std::io::Error> {
    let opened = web_sys::window().and_then(|window| window.open_with_url(url).ok().flatten());
    if opened.is_some() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "the browser blocked opening a new tab",
        ))
    }
}

pub(crate) fn format_duration(elapsed_ms: u64) -> String {
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

pub(crate) fn run_state_label(state: Option<AgentRunState>) -> &'static str {
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

pub(crate) fn change_kind_label(kind: loom_protocol::WorkspaceChangeKind) -> &'static str {
    match kind {
        loom_protocol::WorkspaceChangeKind::Created => "New",
        loom_protocol::WorkspaceChangeKind::Deleted => "Removed",
        loom_protocol::WorkspaceChangeKind::Modified => "Updated",
    }
}

pub(crate) fn belongs_to_repository(path: &str, repositories: &[SessionRepository]) -> bool {
    repositories.iter().any(|repository| {
        path == repository.path || path.starts_with(&format!("{}/", repository.path))
    })
}

/// A readable, filesystem-safe mount path for an attached source, so workspace
/// listings show `sources/my-repo/...` instead of an opaque id. A numeric
/// suffix is added when the name is already mounted.
pub(crate) fn source_mount_path(prefix: &str, source: &str, existing: &[String]) -> String {
    let name = source
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim_end_matches(".git");
    let mut slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(['-', '.'])
        .to_ascii_lowercase();
    if slug.is_empty() {
        slug = "source".to_owned();
    }
    let base = format!("{prefix}/{slug}");
    if !existing.iter().any(|path| path == &base) {
        return base;
    }
    let mut counter = 2;
    loop {
        let candidate = format!("{base}-{counter}");
        if !existing.iter().any(|path| path == &candidate) {
            return candidate;
        }
        counter += 1;
    }
}

pub(crate) fn tool_element_id(index: usize, part_index: usize) -> u64 {
    ((index as u64) << 32) | part_index as u64
}

pub(crate) fn reasoning_preview(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview = normalized.chars().take(96).collect::<String>();
    if normalized.chars().count() > 96 {
        preview.push('…');
    }
    preview
}

pub(crate) fn streaming_caret(index: usize) -> gpui_kit::AnyElement {
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

pub(crate) fn assistant_turn_matches(
    timeline: &[TimelineItem],
    predicate: impl Fn(&AssistantPart) -> bool,
) -> bool {
    timeline.iter().any(
        |item| matches!(item, TimelineItem::Assistant(turn) if turn.parts.iter().any(&predicate)),
    )
}

pub(crate) fn render_timeline_text(id: String, text: String, color: u32) -> gpui_kit::AnyElement {
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
pub(crate) fn render_code_block(
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
pub(crate) fn render_patch_block(
    id: impl Into<gpui_kit::ElementId>,
    patch: &str,
) -> gpui_kit::AnyElement {
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
pub(crate) fn tool_output_language(part: &ToolPart) -> Language {
    match part.name.as_str() {
        "run_command" => Language::Bash,
        "glob" | "search_text" | "web_search" => Language::Text,
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
pub(crate) fn tool_icon(name: &str) -> AssetIconName {
    match name {
        "read_file" => AssetIconName::FileText,
        "write_file" | "apply_patch" => AssetIconName::Pencil,
        "list_files" => AssetIconName::FolderOpen,
        "glob" => AssetIconName::FolderOpen,
        "search_text" => AssetIconName::Search,
        "github_list_pull_requests" => AssetIconName::List,
        "github_get_pull_request" => AssetIconName::GitBranch,
        "github_create_pull_request" => AssetIconName::GitMerge,
        "github_push_branch" => AssetIconName::ArrowUp,
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
pub(crate) fn tool_group_label(name: &str, count: usize) -> String {
    match name {
        "read_file" => format!("Read {count} files"),
        "write_file" | "apply_patch" => format!("Edited {count} files"),
        "list_files" => format!("Listed {count} directories"),
        "glob" => format!("Matched {count} globs"),
        "search_text" => format!("Searched {count} times"),
        "github_list_pull_requests" => format!("Listed pull requests {count} times"),
        "github_get_pull_request" => format!("Read {count} pull requests"),
        "github_create_pull_request" => format!("Opened {count} pull requests"),
        "github_push_branch" => format!("Pushed {count} branches"),
        "run_command" => format!("Ran {count} commands"),
        "web_search" => format!("Searched the web {count} times"),
        "propose_plan" => format!("Proposed {count} plans"),
        "ask_user" => format!("Asked the user {count} times"),
        "delegate_project_task" => format!("Delegated {count} sub-agents"),
        "delegate_project_code_task" => format!("Delegated {count} code sub-agents"),
        "wait_for_project_children" => format!("Waited on sub-agents {count} times"),
        "control_project_child" => format!("Controlled sub-agents {count} times"),
        "send_project_agent_message" => format!("Messaged sub-agents {count} times"),
        "list_project_message_recipients" => {
            format!("Listed message recipients {count} times")
        }
        "list_project_children" => format!("Listed sub-agents {count} times"),
        "review_project_child" => format!("Reviewed {count} sub-agents"),
        "integrate_project_child" => format!("Integrated {count} sub-agents"),
        other => format!("{other} × {count}"),
    }
}

/// A compact noun for one tool type, used by the transcript's usage summary.
pub(crate) fn tool_usage_label(name: &str) -> &str {
    match name {
        "read_file" => "Read",
        "write_file" | "apply_patch" => "Edit",
        "list_files" => "List",
        "glob" => "Glob",
        "search_text" => "Search",
        "github_list_pull_requests" => "List PRs",
        "github_get_pull_request" => "Read PR",
        "github_create_pull_request" => "Open PR",
        "github_push_branch" => "Push",
        "run_command" => "Run",
        "web_search" => "Web search",
        "propose_plan" => "Plan",
        "ask_user" => "Ask",
        "delegate_project_task" => "Delegate",
        "delegate_project_code_task" => "Delegate code",
        "wait_for_project_children" => "Wait",
        "control_project_child" => "Control",
        "send_project_agent_message" => "Message",
        "list_project_message_recipients" => "List recipients",
        "list_project_children" => "List agents",
        "review_project_child" => "Review",
        "integrate_project_child" => "Integrate",
        other => other,
    }
}

/// Summarizes a run of tool calls by type and invocation count, keeping the
/// order in which each type first appears. The transcript shows this as the
/// single collapsed line above the detailed tool presentation.
pub(crate) fn tool_usage_summary(tools: &[&ToolPart]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for tool in tools {
        let name = tool.name.as_str();
        if let Some(entry) = counts.iter_mut().find(|(existing, _)| *existing == name) {
            entry.1 += 1;
        } else {
            counts.push((name, 1));
        }
    }
    counts
        .into_iter()
        .map(|(name, count)| format!("{} ×{count}", tool_usage_label(name)))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The aggregate status for a collapsed group of tool calls. Active work and
/// pending calls keep the group from reading as done until every call has
/// settled. At that point a failure only dominates when nothing succeeded: a
/// response that recovered from a failed call reads as done, with the failures
/// called out by `tool_failure_count` instead of condemning the whole run.
pub(crate) fn tool_group_status(tools: &[&ToolPart]) -> ToolPartStatus {
    let has = |status| tools.iter().any(|tool| tool.status == status);
    let active = tools.iter().any(|tool| {
        matches!(
            tool.status,
            ToolPartStatus::Running
                | ToolPartStatus::AwaitingApproval
                | ToolPartStatus::AwaitingInput
        )
    });
    if active {
        ToolPartStatus::Running
    } else if has(ToolPartStatus::Queued) {
        ToolPartStatus::Queued
    } else if has(ToolPartStatus::Failed) && !has(ToolPartStatus::Completed) {
        ToolPartStatus::Failed
    } else if has(ToolPartStatus::Cancelled) && !has(ToolPartStatus::Completed) {
        ToolPartStatus::Cancelled
    } else {
        ToolPartStatus::Completed
    }
}

/// How many calls in a run failed. Paired with an aggregate status of `Completed`
/// this keeps individual failures visible without reading the run as failed.
pub(crate) fn tool_failure_count(tools: &[&ToolPart]) -> usize {
    tools
        .iter()
        .filter(|tool| tool.status == ToolPartStatus::Failed)
        .count()
}

/// Renders a tool result as a patch when it looks like one, otherwise as code.
pub(crate) fn render_tool_output(
    id: impl Into<gpui_kit::ElementId>,
    part: &ToolPart,
) -> gpui_kit::AnyElement {
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
/// transcript never renders a bare `null`. Known tools get a readable summary
/// instead of their raw JSON arguments; only unknown/extension tools fall back
/// to serialized JSON.
pub(crate) fn tool_detail(call: &loom_model::ToolCall) -> Option<String> {
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
        // Core tools summarize their arguments instead of dumping raw JSON, and
        // omit the detail when the title already carries the same information.
        "read_file" => {
            let start = call
                .arguments
                .get("line_start")
                .and_then(serde_json::Value::as_u64);
            let end = call
                .arguments
                .get("line_end")
                .and_then(serde_json::Value::as_u64);
            match (start, end) {
                (Some(start), Some(end)) => Some(format!("Lines {start}–{end}")),
                (Some(start), None) => Some(format!("From line {start}")),
                (None, Some(end)) => Some(format!("Through line {end}")),
                (None, None) => None,
            }
        }
        "list_files" => {
            let mut parts = Vec::new();
            if let Some(glob) = string_argument(&call.arguments, "glob") {
                parts.push(format!("glob {glob}"));
            }
            if let Some(depth) = call
                .arguments
                .get("depth")
                .and_then(serde_json::Value::as_u64)
            {
                parts.push(format!("depth {depth}"));
            }
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        "search_text" => {
            let mut parts = Vec::new();
            if let Some(path) = string_argument(&call.arguments, "path") {
                parts.push(format!("in {path}"));
            }
            if let Some(glob) = string_argument(&call.arguments, "glob") {
                parts.push(format!("glob {glob}"));
            }
            if call
                .arguments
                .get("regex")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                parts.push("regex".to_owned());
            }
            if call
                .arguments
                .get("case_sensitive")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                parts.push("case-sensitive".to_owned());
            }
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        "web_search" => {
            let domains = call
                .arguments
                .get("domains")
                .and_then(serde_json::Value::as_array)
                .map(|domains| {
                    domains
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            (!domains.is_empty()).then(|| format!("Domains: {domains}"))
        }
        "propose_plan" => {
            let steps = call
                .arguments
                .get("steps")
                .and_then(serde_json::Value::as_array)?;
            if steps.is_empty() {
                return None;
            }
            let detail = steps
                .iter()
                .enumerate()
                .map(|(index, step)| {
                    format!("{}. {}", index + 1, step.as_str().unwrap_or_default())
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(bounded_to(&detail, 600))
        }
        "ask_user" => string_argument(&call.arguments, "prompt")
            .map(|prompt| bounded_to(prompt.trim(), 600))
            .filter(|prompt| !prompt.is_empty()),
        "apply_patch" => {
            let edits = call
                .arguments
                .get("edits")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            (edits > 0).then(|| format!("{edits} edit{}", if edits == 1 { "" } else { "s" }))
        }
        "run_command" => {
            let command = string_argument(&call.arguments, "command")?;
            let args = call
                .arguments
                .get("args")
                .and_then(serde_json::Value::as_array)
                .map(|args| {
                    args.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut detail = command_line(&command, &args);
            if let Some(cwd) = string_argument(&call.arguments, "cwd") {
                detail.push_str(&format!("\nDirectory: {cwd}"));
            }
            Some(detail)
        }
        // These arguments are already summarized in the title.
        "glob"
        | "github_list_pull_requests"
        | "github_get_pull_request"
        | "github_create_pull_request"
        | "github_push_branch" => None,
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
pub(crate) fn humanize_tool_output(name: &str, output: &str) -> String {
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

/// The detail lines shown under a tool block. A command's own line is already
/// the row title, so only its extra lines (a working directory) remain.
pub(crate) fn tool_display_detail(part: &ToolPart) -> Option<String> {
    let detail = part.detail.as_deref()?;
    if part.name != "run_command" {
        return Some(detail.to_owned());
    }
    let rest = detail.split_once('\n').map_or("", |(_, rest)| rest);
    (!rest.trim().is_empty()).then(|| rest.to_owned())
}

/// Whether expanding a tool block would reveal anything. A block whose result is
/// only the action already shown on the row has nothing to expand, so it offers
/// no disclosure control.
/// Whether a successful result that exists nowhere else is shown. Failed output
/// is never shown: the row reports the failed status, and whether the failure
/// matters is the agent's judgement, reported in its answer.
pub(crate) fn tool_shows_output(part: &ToolPart) -> bool {
    part.status != ToolPartStatus::Failed
        && part
            .output
            .as_deref()
            .is_some_and(|output| !output.is_empty())
        && loom_protocol::tool_result_kind(&part.name).keeps_body()
}

/// Whether expanding a tool block would reveal anything. A block whose result is
/// only the action already shown on the row has nothing to expand, so it offers
/// no disclosure control.
pub(crate) fn tool_has_body(part: &ToolPart) -> bool {
    tool_display_detail(part).is_some() || tool_shows_output(part)
}

/// A long command is cut to keep the tool row on one line.
const COMMAND_TITLE_LIMIT: usize = 80;

/// The single-line label shown for a tool block. A search is summarized as the
/// query plus how many hits it found, and a command as the command itself,
/// because the bare action does not say whether anything matched or which
/// program ran.
pub(crate) fn tool_display_title(part: &ToolPart) -> String {
    match part.name.as_str() {
        "search_text" => search_display_title(part),
        "run_command" => command_display_title(part),
        _ => part.title.clone(),
    }
}

fn search_display_title(part: &ToolPart) -> String {
    let Some(query) = search_query(part) else {
        return part.title.clone();
    };
    let Some(output) = part.output.as_deref() else {
        return format!("Search \"{query}\"");
    };
    match search_hit_count(output) {
        0 => format!("Search \"{query}\" · no hits"),
        1 => format!("Search \"{query}\" · 1 hit"),
        hits => format!("Search \"{query}\" · {hits} hits"),
    }
}

/// Shows the command on one line; the full command and working directory stay in
/// the expanded detail.
fn command_display_title(part: &ToolPart) -> String {
    let command = part
        .detail
        .as_deref()
        .and_then(|detail| detail.lines().next())
        .map(str::trim)
        .filter(|command| !command.is_empty());
    match command {
        Some(command) => compact_activity_text(command, COMMAND_TITLE_LIMIT),
        None => part.title.clone(),
    }
}

/// Recovers the search query from a tool block. The title carries it when the
/// call arguments are known; after a restore the query survives in the detail
/// line instead.
fn search_query(part: &ToolPart) -> Option<String> {
    let from_quoted = |text: &str| {
        let rest = text.strip_prefix('"')?;
        let end = rest.find('"')?;
        Some(rest[..end].to_owned())
    };
    if let Some(rest) = part.title.strip_prefix("Search ")
        && let Some(query) = from_quoted(rest)
    {
        return Some(query);
    }
    part.detail.as_deref().and_then(from_quoted)
}

/// Counts the matching lines in a `search_text` result. Matches render as
/// `path:line:content`; context lines use `-` in place of the colons.
pub(crate) fn search_hit_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            let Some((_path, rest)) = line.split_once(':') else {
                return false;
            };
            let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            digits > 0 && rest[digits..].starts_with(':')
        })
        .count()
}

pub(crate) fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

pub(crate) fn string_argument(arguments: &serde_json::Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// A short human intent for a tool call, used as the tool block title.
pub(crate) fn tool_title(name: &str, arguments: &serde_json::Value) -> String {
    let path = || string_argument(arguments, "path");
    match name {
        "read_file" => path().map_or_else(|| "Read file".to_owned(), |p| format!("Read {p}")),
        "write_file" => path().map_or_else(|| "Edit file".to_owned(), |p| format!("Edit {p}")),
        "list_files" => path().map_or_else(|| "List files".to_owned(), |p| format!("List {p}")),
        "glob" => {
            let Some(pattern) = string_argument(arguments, "pattern") else {
                return "Find files".to_owned();
            };
            let pattern = compact_activity_text(&pattern, 40);
            string_argument(arguments, "path").map_or_else(
                || format!("Find \"{pattern}\""),
                |path| format!("Find \"{pattern}\" in {path}"),
            )
        }
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
        "github_list_pull_requests" => string_argument(arguments, "repository").map_or_else(
            || "List pull requests".to_owned(),
            |repository| format!("List pull requests in {repository}"),
        ),
        "github_get_pull_request" => {
            let repository = string_argument(arguments, "repository");
            let number = arguments.get("number").and_then(serde_json::Value::as_u64);
            match (repository, number) {
                (Some(repository), Some(number)) => format!("Read {repository}#{number}"),
                (Some(repository), None) => format!("Read pull request in {repository}"),
                _ => "Read pull request".to_owned(),
            }
        }
        "github_create_pull_request" => match (
            string_argument(arguments, "repository"),
            string_argument(arguments, "head"),
            string_argument(arguments, "base"),
        ) {
            (Some(repository), Some(head), Some(base)) => {
                format!("Open {head} → {base} in {repository}")
            }
            (Some(repository), _, _) => format!("Open pull request in {repository}"),
            _ => "Open pull request".to_owned(),
        },
        "github_push_branch" => match (
            string_argument(arguments, "repository"),
            string_argument(arguments, "branch"),
        ) {
            (Some(repository), Some(branch)) => format!("Push {branch} to {repository}"),
            (Some(repository), None) => format!("Push branch to {repository}"),
            (None, Some(branch)) => format!("Push {branch}"),
            (None, None) => "Push branch".to_owned(),
        },
        other => other.to_owned(),
    }
}

pub(crate) fn tool_title_for_activity(activity: &AgentActivityRecord) -> String {
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

pub(crate) fn tool_status(status: AgentActivityStatus) -> ToolPartStatus {
    match status {
        AgentActivityStatus::Started => ToolPartStatus::Running,
        AgentActivityStatus::Completed => ToolPartStatus::Completed,
        AgentActivityStatus::Failed => ToolPartStatus::Failed,
        AgentActivityStatus::AwaitingApproval => ToolPartStatus::AwaitingApproval,
        AgentActivityStatus::AwaitingInput => ToolPartStatus::AwaitingInput,
        AgentActivityStatus::Cancelled => ToolPartStatus::Cancelled,
    }
}

pub(crate) fn tool_part_from_call(call: &loom_model::ToolCall, status: ToolPartStatus) -> ToolPart {
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

pub(crate) fn tool_part_from_activity(activity: &AgentActivityRecord) -> Option<ToolPart> {
    let call = match &activity.data {
        AgentActivityData::ModelTurn { .. } => return None,
        AgentActivityData::ToolCall { call, .. }
        | AgentActivityData::File { call, .. }
        | AgentActivityData::Search { call, .. }
        | AgentActivityData::Command { call, .. } => call,
    };
    let detail = match &activity.data {
        // These activity shapes carry the call, which already summarizes the
        // arguments; the title names the target, so the detail adds whatever the
        // title omits (a line range, a glob, a depth) and nothing more.
        AgentActivityData::ToolCall { .. }
        | AgentActivityData::File { .. }
        | AgentActivityData::Search { .. } => tool_detail(call),
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

pub(crate) fn activity_output(activity: &AgentActivityRecord) -> Option<&str> {
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

pub(crate) fn compact_activity_text(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let mut label = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        label.push('…');
    }
    label
}

pub(crate) fn command_line(command: &str, args: &[String]) -> String {
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

pub(crate) fn command_purpose(command: &str, args: &[String]) -> String {
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

pub(crate) fn is_redundant_completion_summary(summary: &str) -> bool {
    summary.starts_with("Completed task:")
}

pub(crate) fn worker_connection_failure_detail(
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

pub(crate) fn connection_placeholder(
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

pub(crate) fn transition_worker_connection_to_connecting(
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

pub(crate) fn mark_worker_connection_failed(node: &mut WorkerNodeEntry, detail: String) -> bool {
    let cleanup_failed = node
        .connection
        .take()
        .is_some_and(|connection| connection.close().is_err());
    node.status.online = false;
    node.connection_state = WorkerConnectionState::Failed;
    node.connection_detail = Some(detail);
    cleanup_failed
}

pub(crate) fn safe_worker_url_label(url: &str) -> String {
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

pub(crate) fn worker_url_embeds_credential(url: &str) -> bool {
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

pub(crate) fn remove_worker_node_entry(
    nodes: &mut Vec<WorkerNodeEntry>,
    id: u64,
) -> Option<WorkerNodeEntry> {
    let index = nodes.iter().position(|node| node.id == id)?;
    if nodes[index].is_local() {
        return None;
    }
    Some(nodes.remove(index))
}

pub(crate) fn update_worker_node_status(
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

pub(crate) fn initial_worker_nodes(
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

#[cfg(test)]
mod tests {
    use super::*;
    use loom_protocol::WorkspaceChangeKind;

    #[test]
    fn relative_time_buckets() {
        let now = 1_000_000_000u64;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - 60_000, now), "1m ago");
        assert_eq!(relative_time(now - 120_000, now), "2m ago");
        assert_eq!(relative_time(now - 3_600_000, now), "1h ago");
        assert_eq!(relative_time(now - 86_400_000, now), "1d ago");
        assert_eq!(relative_time(now - 604_800_000, now), "1w ago");
    }

    #[test]
    fn sizing_and_formatting_helpers() {
        assert_eq!(composer_height("", false), 28.0);
        assert_eq!(composer_height("a\nb", false), 48.0);
        assert_eq!(composer_height("", true), 44.0);
        assert_eq!(composer_height("a\nb", true), 64.0);
        assert_eq!(composer_height(&"x\n".repeat(20), true), 184.0);
        assert_eq!(format_bytes(None), "n/a");
        assert_eq!(format_bytes(Some(1024)), "1.0 KiB");
        assert_eq!(format_bytes(Some(1 << 20)), "1.0 MiB");
        assert_eq!(format_bytes(Some(1 << 30)), "1.0 GiB");
        assert_eq!(format_percentage(None), "n/a");
        assert_eq!(format_percentage(Some(50)), "50%");
        assert_eq!(format_percentage(Some(101)), "n/a");
        assert_eq!(format_duration(500), "500ms");
        assert_eq!(format_duration(1_500), "1.5s");
        assert_eq!(format_duration(65_000), "1m 5s");
    }

    #[test]
    fn labels_and_pulse_adjustments() {
        assert_eq!(run_state_label(None), "Ready");
        assert_eq!(run_state_label(Some(AgentRunState::Executing)), "Working");
        assert_eq!(run_state_label(Some(AgentRunState::Completed)), "Complete");
        assert_eq!(change_kind_label(WorkspaceChangeKind::Created), "New");
        assert_eq!(change_kind_label(WorkspaceChangeKind::Deleted), "Removed");
        assert_eq!(change_kind_label(WorkspaceChangeKind::Modified), "Updated");
        assert_eq!(adjusted_cpu_pulse_threshold(50, 100), 100);
        assert_eq!(adjusted_cpu_pulse_threshold(10, -100), 0);
    }

    #[test]
    fn composer_token_replacement() {
        assert_eq!(replace_command_token("/old rest", "new"), "/new rest");
        assert_eq!(replace_command_token("/old", "new"), "/new ");
        assert_eq!(replace_last_token("hello wo", "world"), "hello world");
        assert_eq!(replace_last_token("", "world"), "world");
    }

    #[test]
    fn tool_helpers() {
        assert_eq!(tool_element_id(1, 2), (1u64 << 32) | 2);
        assert_eq!(tool_group_label("read_file", 3), "Read 3 files");
        assert_eq!(tool_group_label("unknown", 2), "unknown × 2");
        assert_eq!(reasoning_preview("  a   b  "), "a b");
        let long = "x".repeat(120);
        assert!(reasoning_preview(&long).ends_with('…'));
        assert_eq!(compact_activity_text("abcdef", 3), "abc…");
        assert_eq!(compact_activity_text("ab", 3), "ab");
    }

    #[test]
    fn search_hits_are_counted_and_shown_without_expanding() {
        let output =
            "src/lib.rs:12:let needle = 1;\nsrc/lib.rs-13-context\nsrc/main.rs:4:needle()\n";
        assert_eq!(search_hit_count(output), 2);
        // Context lines and content containing colons are not matches.
        assert_eq!(search_hit_count("src/lib.rs-13-let x: u32\n"), 0);
        assert_eq!(search_hit_count(""), 0);

        let part = |title: &str, detail: Option<&str>, output: Option<&str>| ToolPart {
            id: ToolCallId::new(),
            name: "search_text".to_owned(),
            title: title.to_owned(),
            status: ToolPartStatus::Completed,
            detail: detail.map(str::to_owned),
            output: output.map(str::to_owned),
            elapsed_ms: None,
            approval_pending: false,
        };

        assert_eq!(
            tool_display_title(&part("Search \"needle\"", None, Some(output))),
            "Search \"needle\" · 2 hits"
        );
        // A restored title is generic; the query survives in the detail line.
        assert_eq!(
            tool_display_title(&part("Search text", Some("\"needle\""), Some(output))),
            "Search \"needle\" · 2 hits"
        );
        assert_eq!(
            tool_display_title(&part("Search \"needle\"", None, Some(""))),
            "Search \"needle\" · no hits"
        );
        // Without a result body the query still shows, with no count to invent.
        assert_eq!(
            tool_display_title(&part("Search \"needle\"", None, None)),
            "Search \"needle\""
        );
        // A query-less search has nothing to summarize and keeps its title.
        assert_eq!(
            tool_display_title(&part("Search text", None, None)),
            "Search text"
        );
    }

    #[test]
    fn command_display_is_one_truncated_line() {
        let part = |detail: Option<&str>| ToolPart {
            id: ToolCallId::new(),
            name: "run_command".to_owned(),
            title: "Run tests".to_owned(),
            status: ToolPartStatus::Completed,
            detail: detail.map(str::to_owned),
            output: Some("test result: ok".to_owned()),
            elapsed_ms: None,
            approval_pending: false,
        };

        assert_eq!(
            tool_display_title(&part(Some("cargo test --workspace\nDirectory: repo"))),
            "cargo test --workspace"
        );
        // Long commands are cut to keep the row on one line.
        let long = format!("mytool {}", "x".repeat(200));
        let title = tool_display_title(&part(Some(&long)));
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), COMMAND_TITLE_LIMIT + 1);
        // Without a command line the human title is the fallback.
        assert_eq!(tool_display_title(&part(None)), "Run tests");
        // The command is the row title, so expansion only adds the rest.
        assert_eq!(
            tool_display_detail(&part(Some("cargo test --workspace\nDirectory: repo"))).as_deref(),
            Some("Directory: repo")
        );
        assert_eq!(tool_display_detail(&part(Some("cargo test"))), None);
        // Other tools keep their detail untouched.
        let read = ToolPart {
            id: ToolCallId::new(),
            name: "read_file".to_owned(),
            title: "Read src/lib.rs".to_owned(),
            status: ToolPartStatus::Completed,
            detail: Some("Lines 10–20".to_owned()),
            output: None,
            elapsed_ms: None,
            approval_pending: false,
        };
        assert_eq!(tool_display_detail(&read).as_deref(), Some("Lines 10–20"));
    }

    #[test]
    fn only_tools_with_something_to_reveal_offer_a_disclosure() {
        let part =
            |name: &str, status: ToolPartStatus, detail: Option<&str>, output: Option<&str>| {
                ToolPart {
                    id: ToolCallId::new(),
                    name: name.to_owned(),
                    title: "row".to_owned(),
                    status,
                    detail: detail.map(str::to_owned),
                    output: output.map(str::to_owned),
                    elapsed_ms: None,
                    approval_pending: false,
                }
            };

        // A successful read hides its output and has no range: nothing to expand.
        assert!(!tool_has_body(&part(
            "read_file",
            ToolPartStatus::Completed,
            None,
            Some("fn main() {}")
        )));
        // A range is worth expanding.
        assert!(tool_has_body(&part(
            "read_file",
            ToolPartStatus::Completed,
            Some("Lines 1–2"),
            Some("fn main() {}")
        )));
        // A failure is never shown, even for a result that exists nowhere else;
        // the row reports the failed status and the agent reports the detail.
        assert!(!tool_has_body(&part(
            "read_file",
            ToolPartStatus::Failed,
            None,
            Some("No such file or directory")
        )));
        assert!(!tool_has_body(&part(
            "web_search",
            ToolPartStatus::Failed,
            None,
            Some("request failed")
        )));
        // A command without a working directory is only its own row title.
        assert!(!tool_has_body(&part(
            "run_command",
            ToolPartStatus::Completed,
            Some("cargo test"),
            Some("test result: ok")
        )));
        assert!(tool_has_body(&part(
            "run_command",
            ToolPartStatus::Completed,
            Some("cargo test\nDirectory: repo"),
            Some("test result: ok")
        )));
        // A result that exists nowhere else is always kept.
        assert!(tool_has_body(&part(
            "web_search",
            ToolPartStatus::Completed,
            None,
            Some(r#"{"results":[]}"#)
        )));
        assert!(!tool_has_body(&part(
            "web_search",
            ToolPartStatus::Completed,
            None,
            Some("")
        )));
    }

    #[test]
    fn command_formatting() {
        assert_eq!(command_line("cargo", &["test".to_owned()]), "cargo test");
        assert_eq!(
            command_line("echo", &["hello world".to_owned()]),
            "echo 'hello world'"
        );
        assert_eq!(command_purpose("cargo", &["test".to_owned()]), "Run tests");
        assert_eq!(
            command_purpose("git", &["status".to_owned()]),
            "Inspect repository changes"
        );
        assert_eq!(
            command_purpose("sh", &["-c".to_owned(), "cargo test".to_owned()]),
            "Run tests"
        );
        assert!(command_purpose("mystery", &["sub".to_owned()]).starts_with("Run mystery"));
        assert!(is_redundant_completion_summary("Completed task: x"));
        assert!(!is_redundant_completion_summary("Did x"));
    }

    #[test]
    fn worker_url_safety() {
        assert_eq!(
            safe_worker_url_label("wss://user:pass@host/ws?token=x"),
            "wss://host/ws"
        );
        assert!(worker_url_embeds_credential("wss://user:pass@host/ws"));
        assert!(worker_url_embeds_credential(
            "wss://host/ws?access_token=secret"
        ));
        assert!(!worker_url_embeds_credential("wss://host/ws"));
        assert!(!worker_url_embeds_credential("wss://host/ws?keep=1"));
    }

    #[test]
    fn json_field_extraction() {
        let value = serde_json::json!({ "a": "x", "b": 2 });
        assert_eq!(string_field(&value, "a"), Some("x".to_owned()));
        assert_eq!(string_field(&value, "b"), None);
        assert_eq!(string_argument(&value, "a"), Some("x".to_owned()));
        assert_eq!(string_argument(&value, "missing"), None);
    }
}
