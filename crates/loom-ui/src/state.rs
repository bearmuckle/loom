//! Client-side projections of backend state.
//!
//! Everything here is derived from protocol responses and events; the backend
//! stays authoritative.

use std::collections::BTreeSet;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::{ListAlignment, ListState, px};
use loom_core::{
    AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, ToolCallId, UsageSnapshot,
};
use loom_model::ProviderUsageSummary;
use loom_protocol::{
    AgentPlanProgress, AgentRunState, GitDiff, GitDiffLine, GitRepositoryStatus,
    SessionFilesystemChange, SessionFilesystemFile, WorkspaceEntry,
};

use crate::MAX_TIMELINE_OUTPUT;

/// The active pane in the right-hand inspector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InspectorTab {
    Changes,
    Agent,
    Context,
    Files,
}

impl InspectorTab {
    pub(crate) const ALL: [Self; 4] = [Self::Changes, Self::Agent, Self::Context, Self::Files];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::Agent => "Agent",
            Self::Context => "Context",
            Self::Files => "Files",
        }
    }

    pub(crate) fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0)
    }
}

/// Token and cost accounting surfaced by the Context and Agent tabs. Session
/// and run figures are kept separately because only one may be current.
#[derive(Clone, Debug, Default)]
pub(crate) struct UsageState {
    pub(crate) session: Option<UsageSnapshot>,
    pub(crate) run: Option<UsageSnapshot>,
    pub(crate) session_provider: Option<ProviderUsageSummary>,
    pub(crate) run_provider: Option<ProviderUsageSummary>,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
}

impl UsageState {
    /// The snapshot to show for the active run, falling back to the session.
    pub(crate) fn total(&self) -> Option<&UsageSnapshot> {
        self.run.as_ref().or(self.session.as_ref())
    }

    pub(crate) fn provider(&self) -> Option<&ProviderUsageSummary> {
        self.run_provider
            .as_ref()
            .or(self.session_provider.as_ref())
    }
}

/// The read-only file browser shown in the Files tab.
#[derive(Clone, Debug, Default)]
pub(crate) struct FilesState {
    pub(crate) entries: Vec<WorkspaceEntry>,
    pub(crate) loading: bool,
    pub(crate) loaded: bool,
    pub(crate) error: Option<String>,
    pub(crate) selected_path: Option<String>,
    pub(crate) selected_file: Option<SessionFilesystemFile>,
    pub(crate) file_loading: bool,
    pub(crate) file_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentMode {
    Ask,
    Edit,
    Agent,
    AutoApprove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThemeChoice {
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub(crate) const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

impl AgentMode {
    pub(crate) const ALL: [Self; 4] = [Self::Ask, Self::Edit, Self::Agent, Self::AutoApprove];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
            Self::Edit => "Edit",
            Self::Agent => "Agent",
            Self::AutoApprove => "Auto approve",
        }
    }

    pub(crate) fn approval_policy(self, auto_approve_actions: bool) -> ApprovalPolicy {
        match self {
            Self::AutoApprove => ApprovalPolicy::auto_approve(),
            Self::Edit | Self::Agent if auto_approve_actions => ApprovalPolicy::auto_approve(),
            Self::Ask | Self::Edit | Self::Agent => ApprovalPolicy::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReviewState {
    pub(crate) open: bool,
    pub(crate) tab: InspectorTab,
    pub(crate) changes: Vec<SessionFilesystemChange>,
    pub(crate) vcs: Option<GitRepositoryStatus>,
    pub(crate) repositories_loaded: bool,
    pub(crate) selected_file: Option<SessionFilesystemFile>,
    pub(crate) selected_path: Option<String>,
    pub(crate) selected_staged: bool,
    pub(crate) selection_revision: u64,
    pub(crate) selected_diff: Option<GitDiff>,
    pub(crate) diff_error: Option<String>,
    pub(crate) loading_diff: bool,
    pub(crate) wrap_lines: bool,
    pub(crate) rows: Vec<ReviewRow>,
    pub(crate) hunk_rows: Vec<usize>,
    pub(crate) collapsed_hunks: BTreeSet<usize>,
    pub(crate) selected_hunk: usize,
    pub(crate) list_state: ListState,
    pub(crate) usage: UsageState,
    pub(crate) files: FilesState,
}

#[derive(Clone, Debug)]
pub(crate) enum ReviewRow {
    Hunk {
        old_start: u32,
        old_lines: u32,
        new_start: u32,
        new_lines: u32,
    },
    Line(GitDiffLine),
}

impl ReviewState {
    pub(crate) fn show_diff(&mut self, diff: GitDiff) {
        self.collapsed_hunks.clear();
        self.selected_diff = Some(diff);
        self.selected_hunk = 0;
        self.loading_diff = false;
        self.diff_error = None;
        self.rebuild_rows();
    }

    /// Rebuilds the virtual rows from the selected diff, hiding the lines of
    /// any collapsed hunk.
    pub(crate) fn rebuild_rows(&mut self) {
        self.rows.clear();
        self.hunk_rows.clear();
        let Some(diff) = self.selected_diff.as_ref() else {
            self.list_state.reset(0);
            return;
        };
        for (hunk_index, hunk) in diff.hunks.iter().enumerate() {
            self.hunk_rows.push(self.rows.len());
            self.rows.push(ReviewRow::Hunk {
                old_start: hunk.old_start,
                old_lines: hunk.old_lines,
                new_start: hunk.new_start,
                new_lines: hunk.new_lines,
            });
            if !self.collapsed_hunks.contains(&hunk_index) {
                self.rows
                    .extend(hunk.lines.iter().cloned().map(ReviewRow::Line));
            }
        }
        self.list_state.reset(self.rows.len());
    }

    /// Toggles the collapse state of the hunk whose header is at `row`.
    pub(crate) fn toggle_hunk(&mut self, row: usize) -> bool {
        let Some(hunk_index) = self
            .hunk_rows
            .iter()
            .position(|candidate| *candidate == row)
        else {
            return false;
        };
        if !self.collapsed_hunks.remove(&hunk_index) {
            self.collapsed_hunks.insert(hunk_index);
        }
        self.rebuild_rows();
        true
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RenameDialogState {
    pub(crate) session: AgentSessionSnapshot,
    pub(crate) input: String,
    pub(crate) is_project: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum GitHubLoginState {
    Starting,
    Awaiting {
        verification_uri: String,
        user_code: String,
        expires_in: u64,
    },
    #[cfg(not(target_family = "wasm"))]
    Completing,
    Success,
    Error(String),
}

/// Which GitHub credential the active device-login flow is acquiring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitHubLoginKind {
    /// Copilot model access, obtained from the Copilot app.
    Copilot,
    /// Repository access for clone, push, and pull requests, obtained from the
    /// GitHub CLI OAuth app.
    Repository,
}

impl GitHubLoginKind {
    /// The error shown when the worker connection is not secure enough to send
    /// a GitHub token over it.
    pub(crate) fn secure_connection_message(self) -> &'static str {
        match self {
            Self::Copilot => {
                "GitHub Copilot sign-in requires a secure worker connection (wss:// or loopback ws://)."
            }
            Self::Repository => {
                "GitHub repository sign-in requires a secure worker connection (wss:// or loopback ws://)."
            }
        }
    }
}

impl Default for ReviewState {
    fn default() -> Self {
        Self {
            open: false,
            tab: InspectorTab::Changes,
            changes: Vec::new(),
            vcs: None,
            repositories_loaded: false,
            selected_file: None,
            selected_path: None,
            selected_staged: false,
            selection_revision: 0,
            selected_diff: None,
            diff_error: None,
            loading_diff: false,
            wrap_lines: false,
            rows: Vec::new(),
            hunk_rows: Vec::new(),
            collapsed_hunks: BTreeSet::new(),
            selected_hunk: 0,
            list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            usage: UsageState::default(),
            files: FilesState::default(),
        }
    }
}

/// Client-side transcript model. The backend's run/activity state is projected
/// into a conversation: each assistant response is a turn made of ordered
/// parts, and each tool call is exactly one part rather than a lifecycle of
/// separate rows.
#[derive(Clone, Debug)]
pub(crate) enum TimelineItem {
    User(String),
    Assistant(AssistantTurn),
    System(SystemNote),
}

/// Live progress for the active run's plan.
///
/// A plan is run-scoped status rather than a conversation message, so it is
/// kept out of the timeline and rendered once as a banner pinned above the
/// transcript, with a compact summary in the inspector.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PlanState {
    pub(crate) steps: Vec<String>,
    pub(crate) completed: BTreeSet<u32>,
    pub(crate) active: Option<u32>,
}

impl PlanState {
    pub(crate) fn new(steps: Vec<String>) -> Self {
        Self {
            steps,
            completed: BTreeSet::new(),
            active: None,
        }
    }

    /// Rebuilds plan progress sent by the backend, which derives it from the
    /// run's persisted step events. Returns `None` when the run has no plan.
    pub(crate) fn from_projection(
        steps: Vec<String>,
        progress: &AgentPlanProgress,
    ) -> Option<Self> {
        if steps.is_empty() {
            return None;
        }
        Some(Self {
            steps,
            completed: progress.completed.iter().copied().collect(),
            active: progress.active,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub(crate) fn total(&self) -> usize {
        self.steps.len()
    }

    pub(crate) fn done_count(&self) -> usize {
        let total = self.steps.len();
        self.completed
            .iter()
            .filter(|index| usize::try_from(**index).is_ok_and(|index| index < total))
            .count()
    }

    pub(crate) fn status(&self, index: u32) -> PlanStepStatus {
        if self.completed.contains(&index) {
            PlanStepStatus::Done
        } else if self.active == Some(index) {
            PlanStepStatus::Active
        } else {
            PlanStepStatus::Pending
        }
    }

    pub(crate) fn active_step(&self) -> Option<&str> {
        self.active
            .and_then(|index| self.steps.get(index as usize))
            .map(String::as_str)
    }

    /// The completed fraction in `0.0..=1.0`, used by the progress bars.
    pub(crate) fn fraction(&self) -> f32 {
        if self.steps.is_empty() {
            0.0
        } else {
            self.done_count() as f32 / self.steps.len() as f32
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlanStepStatus {
    Done,
    Active,
    Pending,
}

impl PlanStepStatus {
    /// The icon shown beside a plan step for its status.
    ///
    /// Status is drawn with icons rather than Unicode glyphs because the
    /// browser build's text system cannot fall back to a font that covers
    /// geometric marks, so `○`/`▶`/`✓` would render blank in wasm.
    pub(crate) fn icon(self) -> AssetIconName {
        match self {
            Self::Done => AssetIconName::Check,
            Self::Active => AssetIconName::Play,
            Self::Pending => AssetIconName::Circle,
        }
    }
}

/// One assistant response, rendered as an ordered list of parts. A response is
/// usually text followed by one or more tool calls; the next response begins
/// once the tool results return.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AssistantTurn {
    pub(crate) parts: Vec<AssistantPart>,
    pub(crate) streaming: bool,
}

impl AssistantTurn {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self {
            parts: vec![AssistantPart::Text(text.into())],
            streaming: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AssistantPart {
    Reasoning(String),
    Text(String),
    Tool(Box<ToolPart>),
    Evidence(Vec<EvidenceText>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EvidenceText {
    pub(crate) label: String,
    pub(crate) uri: String,
}

/// A single tool invocation presented as one collapsible block.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolPart {
    pub(crate) id: ToolCallId,
    pub(crate) name: String,
    pub(crate) title: String,
    pub(crate) status: ToolPartStatus,
    pub(crate) detail: Option<String>,
    pub(crate) output: Option<String>,
    pub(crate) elapsed_ms: Option<u64>,
    pub(crate) approval_pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolPartStatus {
    Queued,
    Running,
    AwaitingApproval,
    AwaitingInput,
    Completed,
    Failed,
    Cancelled,
}

impl ToolPartStatus {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::AwaitingApproval => "approval required",
            Self::AwaitingInput => "waiting for input",
            Self::Completed => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemTone {
    Neutral,
    Error,
    Input,
}

/// A non-conversational note: status, an error, or a request for input.
#[derive(Clone, Debug)]
pub(crate) struct SystemNote {
    pub(crate) tone: SystemTone,
    pub(crate) heading: Option<String>,
    pub(crate) text: String,
    pub(crate) retryable: bool,
}

impl SystemNote {
    pub(crate) fn status(text: impl Into<String>) -> Self {
        Self {
            tone: SystemTone::Neutral,
            heading: None,
            text: text.into(),
            retryable: false,
        }
    }
}

/// Appends streamed assistant text. The model's tool-using cycles belong to one
/// agent response, so text after a tool call continues the same turn; only a
/// different timeline item (a user message, plan, or system note) starts a new
/// one.
pub(crate) fn push_assistant_text(timeline: &mut Vec<TimelineItem>, text: &str) {
    if text.is_empty() {
        return;
    }
    let starts_new_turn = !matches!(
        timeline.last(),
        Some(TimelineItem::Assistant(turn))
            if matches!(
                turn.parts.last(),
                None | Some(AssistantPart::Text(_))
                    | Some(AssistantPart::Reasoning(_))
                    | Some(AssistantPart::Tool(_))
            )
    );
    if starts_new_turn {
        timeline.push(TimelineItem::Assistant(AssistantTurn::default()));
    }
    let Some(TimelineItem::Assistant(turn)) = timeline.last_mut() else {
        return;
    };
    turn.streaming = true;
    match turn.parts.last_mut() {
        Some(AssistantPart::Text(existing)) => existing.push_str(text),
        _ => turn.parts.push(AssistantPart::Text(text.to_owned())),
    }
}

/// Appends streamed reasoning text to the active assistant turn. Reasoning is
/// presentation-only and never becomes part of the provider transcript.
pub(crate) fn push_assistant_reasoning(timeline: &mut Vec<TimelineItem>, text: &str) {
    if text.is_empty() {
        return;
    }
    let starts_new_turn = !matches!(
        timeline.last(),
        Some(TimelineItem::Assistant(turn))
            if matches!(
                turn.parts.last(),
                None | Some(AssistantPart::Text(_))
                    | Some(AssistantPart::Reasoning(_))
                    | Some(AssistantPart::Tool(_))
            )
    );
    if starts_new_turn {
        timeline.push(TimelineItem::Assistant(AssistantTurn::default()));
    }
    let Some(TimelineItem::Assistant(turn)) = timeline.last_mut() else {
        return;
    };
    turn.streaming = true;
    match turn.parts.last_mut() {
        Some(AssistantPart::Reasoning(existing)) => existing.push_str(text),
        _ => turn.parts.push(AssistantPart::Reasoning(text.to_owned())),
    }
}

pub(crate) fn finish_assistant_turn(timeline: &mut [TimelineItem]) {
    if let Some(TimelineItem::Assistant(turn)) = timeline.last_mut() {
        turn.streaming = false;
    }
}

pub(crate) fn push_assistant_evidence(
    timeline: &mut Vec<TimelineItem>,
    evidence: Vec<EvidenceText>,
) {
    if evidence.is_empty() {
        return;
    }
    if !matches!(timeline.last(), Some(TimelineItem::Assistant(_))) {
        timeline.push(TimelineItem::Assistant(AssistantTurn::default()));
    }
    if let Some(TimelineItem::Assistant(turn)) = timeline.last_mut() {
        turn.streaming = false;
        turn.parts.push(AssistantPart::Evidence(evidence));
    }
}

/// Inserts or updates a tool part, keyed by tool call ID. Updates find the
/// existing block wherever it landed so late results stay in place.
pub(crate) fn upsert_tool_part(timeline: &mut Vec<TimelineItem>, part: ToolPart) {
    for item in timeline.iter_mut().rev() {
        if let TimelineItem::Assistant(turn) = item
            && let Some(AssistantPart::Tool(existing)) = turn.parts.iter_mut().find(
                |candidate| matches!(candidate, AssistantPart::Tool(tool) if tool.id == part.id),
            )
        {
            merge_tool_part(existing, part);
            turn.streaming = false;
            return;
        }
    }
    if !matches!(timeline.last(), Some(TimelineItem::Assistant(_))) {
        timeline.push(TimelineItem::Assistant(AssistantTurn::default()));
    }
    if let Some(TimelineItem::Assistant(turn)) = timeline.last_mut() {
        turn.streaming = false;
        turn.parts.push(AssistantPart::Tool(Box::new(part)));
    }
}

/// Whether a tool block for `id` is already present in the timeline.
pub(crate) fn has_tool_part(timeline: &[TimelineItem], id: ToolCallId) -> bool {
    timeline.iter().any(|item| {
        matches!(
            item,
            TimelineItem::Assistant(turn)
                if turn.parts.iter().any(
                    |part| matches!(part, AssistantPart::Tool(tool) if tool.id == id)
                )
        )
    })
}

fn merge_tool_part(target: &mut ToolPart, update: ToolPart) {
    if !update.title.is_empty() {
        target.title = update.title;
    }
    if !update.name.is_empty() {
        target.name = update.name;
    }
    target.status = update.status;
    if update.detail.is_some() {
        target.detail = update.detail;
    }
    if update.output.is_some() {
        target.output = update.output;
    }
    if update.elapsed_ms.is_some() {
        target.elapsed_ms = update.elapsed_ms;
    }
    target.approval_pending = update.approval_pending;
}

pub(crate) fn bounded(value: &str) -> String {
    bounded_to(value, MAX_TIMELINE_OUTPUT)
}

pub(crate) fn session_title_from_task(task: &str) -> String {
    let normalized = task.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = normalized.chars().take(56).collect::<String>();
    if normalized.chars().count() > 56 {
        title.push('…');
    }
    if title.is_empty() {
        "New session".to_owned()
    } else {
        title
    }
}

pub(crate) fn bounded_to(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .map(|(index, character)| (index, index + character.len_utf8()))
        .take_while(|(_, end)| *end <= limit)
        .map(|(_, end)| end)
        .last()
        .unwrap_or_default();
    let mut result = value[..end].to_owned();
    result.push_str("\n...[output truncated]");
    result
}

pub(crate) fn session_state_for_run(state: AgentRunState) -> AgentSessionState {
    match state {
        AgentRunState::Planning => AgentSessionState::Planning,
        AgentRunState::Executing => AgentSessionState::Executing,
        AgentRunState::AwaitingApproval => AgentSessionState::AwaitingApproval,
        AgentRunState::Paused => AgentSessionState::Paused,
        AgentRunState::NeedsInput => AgentSessionState::NeedsInput,
        AgentRunState::Evaluating => AgentSessionState::Evaluating,
        AgentRunState::Completed => AgentSessionState::Completed,
        AgentRunState::Failed => AgentSessionState::Failed,
        AgentRunState::Cancelled => AgentSessionState::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::ToolCallId;
    use loom_protocol::{GitDiffHunk, GitDiffLineKind};

    #[test]
    fn inspector_tabs_report_stable_labels_and_indexes() {
        assert_eq!(
            InspectorTab::ALL.map(InspectorTab::label),
            ["Changes", "Agent", "Context", "Files"]
        );
        for (index, tab) in InspectorTab::ALL.into_iter().enumerate() {
            assert_eq!(tab.index(), index);
        }
    }

    #[test]
    fn plan_state_tracks_step_status_and_progress() {
        let mut plan = PlanState::new(vec![
            "Inspect".to_owned(),
            "Edit".to_owned(),
            "Verify".to_owned(),
        ]);
        assert!(!plan.is_empty());
        assert_eq!(plan.total(), 3);
        assert_eq!(plan.done_count(), 0);
        assert_eq!(plan.fraction(), 0.0);
        assert_eq!(plan.active_step(), None);
        assert_eq!(plan.status(0), PlanStepStatus::Pending);

        plan.active = Some(1);
        assert_eq!(plan.active_step(), Some("Edit"));
        assert_eq!(plan.status(1), PlanStepStatus::Active);
        assert_eq!(plan.status(1).icon(), AssetIconName::Play);

        plan.completed.insert(0);
        plan.completed.insert(1);
        plan.active = None;
        assert_eq!(plan.done_count(), 2);
        assert!((plan.fraction() - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(plan.status(0), PlanStepStatus::Done);
        assert_eq!(plan.status(0).icon(), AssetIconName::Check);
        assert_eq!(plan.status(2), PlanStepStatus::Pending);
        assert_eq!(plan.status(2).icon(), AssetIconName::Circle);

        let empty = PlanState::default();
        assert!(empty.is_empty());
        assert_eq!(empty.total(), 0);
        assert_eq!(empty.fraction(), 0.0);
        assert_eq!(empty.active_step(), None);
    }

    #[test]
    fn usage_state_prefers_run_over_session() {
        let mut usage = UsageState::default();
        assert!(usage.total().is_none());
        assert!(usage.provider().is_none());
        usage.session = Some(UsageSnapshot {
            input_tokens: 10,
            ..UsageSnapshot::default()
        });
        usage.session_provider = Some(ProviderUsageSummary {
            requests: 1,
            ..ProviderUsageSummary::default()
        });
        assert_eq!(usage.total().unwrap().input_tokens, 10);
        assert_eq!(usage.provider().unwrap().requests, 1);
        usage.run = Some(UsageSnapshot {
            input_tokens: 20,
            ..UsageSnapshot::default()
        });
        usage.run_provider = Some(ProviderUsageSummary {
            requests: 2,
            ..ProviderUsageSummary::default()
        });
        assert_eq!(usage.total().unwrap().input_tokens, 20);
        assert_eq!(usage.provider().unwrap().requests, 2);
    }

    #[test]
    fn review_hunks_map_to_virtual_rows() {
        let mut review = ReviewState::default();
        review.show_diff(GitDiff {
            path: Some("src/lib.rs".to_owned()),
            staged: false,
            patch: String::new(),
            binary: false,
            truncated: false,
            hunks: vec![
                GitDiffHunk {
                    old_start: 2,
                    old_lines: 1,
                    new_start: 2,
                    new_lines: 1,
                    lines: vec![GitDiffLine {
                        kind: GitDiffLineKind::Removed,
                        old_line: Some(2),
                        new_line: None,
                        content: "old".to_owned(),
                    }],
                },
                GitDiffHunk {
                    old_start: 20,
                    old_lines: 1,
                    new_start: 20,
                    new_lines: 1,
                    lines: vec![GitDiffLine {
                        kind: GitDiffLineKind::Added,
                        old_line: None,
                        new_line: Some(20),
                        content: "new".to_owned(),
                    }],
                },
            ],
        });
        assert_eq!(review.hunk_rows, vec![0, 2]);
        assert_eq!(review.list_state.item_count(), 4);
    }

    #[test]
    fn collapsed_hunks_hide_their_lines_until_expanded() {
        let mut review = ReviewState::default();
        review.show_diff(GitDiff {
            path: Some("src/lib.rs".to_owned()),
            staged: false,
            patch: String::new(),
            binary: false,
            truncated: false,
            hunks: vec![
                GitDiffHunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec![GitDiffLine {
                        kind: GitDiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                        content: "old".to_owned(),
                    }],
                },
                GitDiffHunk {
                    old_start: 9,
                    old_lines: 1,
                    new_start: 9,
                    new_lines: 1,
                    lines: vec![GitDiffLine {
                        kind: GitDiffLineKind::Added,
                        old_line: None,
                        new_line: Some(9),
                        content: "new".to_owned(),
                    }],
                },
            ],
        });
        assert_eq!(review.rows.len(), 4);
        assert!(review.toggle_hunk(0));
        assert!(review.collapsed_hunks.contains(&0));
        assert_eq!(review.rows.len(), 3);
        // The remaining hunk header keeps a valid row index.
        assert_eq!(review.hunk_rows, vec![0, 1]);
        assert_eq!(review.list_state.item_count(), 3);
        assert!(review.toggle_hunk(0));
        assert_eq!(review.rows.len(), 4);
        assert!(!review.toggle_hunk(999));
    }

    #[test]
    fn bounded_projection_is_explicit() {
        let value = bounded_to("abcdef", 3);
        assert_eq!(value, "abc\n...[output truncated]");
        assert!(bounded_to("😀😀", 4).starts_with('😀'));
    }

    #[test]
    fn agent_and_edit_modes_default_to_safe_auto_approval() {
        assert_eq!(
            AgentMode::Agent.approval_policy(true),
            ApprovalPolicy::auto_approve()
        );
        assert_eq!(
            AgentMode::Edit.approval_policy(true),
            ApprovalPolicy::auto_approve()
        );
        assert_eq!(
            AgentMode::Agent.approval_policy(false),
            ApprovalPolicy::default()
        );
        assert_eq!(
            AgentMode::Edit.approval_policy(false),
            ApprovalPolicy::default()
        );
        assert_eq!(
            AgentMode::Ask.approval_policy(true),
            ApprovalPolicy::default()
        );
        assert_eq!(
            AgentMode::AutoApprove.approval_policy(false),
            ApprovalPolicy::auto_approve()
        );
    }

    fn tool(id: ToolCallId, name: &str, status: ToolPartStatus) -> ToolPart {
        ToolPart {
            id,
            name: name.to_owned(),
            title: name.to_owned(),
            status,
            detail: None,
            output: None,
            elapsed_ms: None,
            approval_pending: false,
        }
    }

    #[test]
    fn assistant_deltas_merge_across_a_tool_part() {
        let mut timeline = Vec::new();
        push_assistant_text(&mut timeline, "Hello ");
        push_assistant_text(&mut timeline, "world");
        assert_eq!(timeline.len(), 1);
        {
            let TimelineItem::Assistant(turn) = &timeline[0] else {
                panic!("expected assistant turn");
            };
            assert!(turn.streaming);
            assert_eq!(
                turn.parts,
                vec![AssistantPart::Text("Hello world".to_owned())]
            );
        }

        upsert_tool_part(
            &mut timeline,
            tool(ToolCallId::new(), "read_file", ToolPartStatus::Queued),
        );
        push_assistant_text(&mut timeline, "Next response");
        // A tool-using response stays one agent entry, so text after a tool
        // continues the same turn instead of opening a new one.
        assert_eq!(timeline.len(), 1);
        let TimelineItem::Assistant(turn) = &timeline[0] else {
            panic!("expected one assistant turn");
        };
        assert_eq!(turn.parts.len(), 3);
        assert!(matches!(turn.parts[0], AssistantPart::Text(ref text) if text == "Hello world"));
        assert!(matches!(turn.parts[1], AssistantPart::Tool(_)));
        assert!(matches!(turn.parts[2], AssistantPart::Text(ref text) if text == "Next response"));
        assert!(turn.streaming);
    }

    #[test]
    fn tool_updates_find_the_existing_part_within_one_turn() {
        let id = ToolCallId::new();
        let mut timeline = Vec::new();
        upsert_tool_part(&mut timeline, tool(id, "read_file", ToolPartStatus::Queued));
        push_assistant_text(&mut timeline, "Working on it");
        let mut completed = tool(id, "read_file", ToolPartStatus::Completed);
        completed.output = Some("file contents".to_owned());
        completed.elapsed_ms = Some(12);
        upsert_tool_part(&mut timeline, completed.clone());
        assert_eq!(timeline.len(), 1);

        let parts = timeline
            .iter()
            .flat_map(|item| match item {
                TimelineItem::Assistant(turn) => turn.parts.as_slice(),
                _ => &[],
            })
            .collect::<Vec<_>>();
        assert_eq!(parts.len(), 2);
        let AssistantPart::Tool(part) = parts[0] else {
            panic!("expected tool part first");
        };
        assert_eq!(part.status, ToolPartStatus::Completed);
        assert_eq!(part.output.as_deref(), Some("file contents"));
        assert_eq!(part.elapsed_ms, Some(12));
        assert!(matches!(parts[1], AssistantPart::Text(text) if text == "Working on it"));
    }

    #[test]
    fn reasoning_deltas_merge_into_one_part_before_text() {
        let mut timeline = Vec::new();
        push_assistant_reasoning(&mut timeline, "Considering ");
        push_assistant_reasoning(&mut timeline, "options.");
        push_assistant_text(&mut timeline, "Here is the answer.");
        assert_eq!(timeline.len(), 1);
        let TimelineItem::Assistant(turn) = &timeline[0] else {
            panic!("expected assistant turn");
        };
        assert_eq!(
            turn.parts,
            vec![
                AssistantPart::Reasoning("Considering options.".to_owned()),
                AssistantPart::Text("Here is the answer.".to_owned()),
            ]
        );
    }

    #[test]
    fn reasoning_after_a_tool_continues_the_same_turn() {
        let mut timeline = Vec::new();
        push_assistant_reasoning(&mut timeline, "first thought");
        upsert_tool_part(
            &mut timeline,
            tool(ToolCallId::new(), "read_file", ToolPartStatus::Completed),
        );
        push_assistant_reasoning(&mut timeline, "second thought");
        assert_eq!(timeline.len(), 1);
        let TimelineItem::Assistant(turn) = &timeline[0] else {
            panic!("expected one assistant turn");
        };
        assert_eq!(turn.parts.len(), 3);
        assert!(
            matches!(turn.parts[0], AssistantPart::Reasoning(ref text) if text == "first thought")
        );
        assert!(matches!(turn.parts[1], AssistantPart::Tool(_)));
        assert!(
            matches!(turn.parts[2], AssistantPart::Reasoning(ref text) if text == "second thought")
        );
    }

    #[test]
    fn evidence_appends_to_the_last_assistant_turn() {
        let mut timeline = Vec::new();
        push_assistant_text(&mut timeline, "Done");
        push_assistant_evidence(
            &mut timeline,
            vec![EvidenceText {
                label: "PR".to_owned(),
                uri: "https://example.com/pr/1".to_owned(),
            }],
        );
        let TimelineItem::Assistant(turn) = &timeline[0] else {
            panic!("expected assistant turn");
        };
        assert!(!turn.streaming);
        assert_eq!(turn.parts.len(), 2);
        assert!(matches!(turn.parts[1], AssistantPart::Evidence(ref links) if links.len() == 1));
    }

    #[test]
    fn tool_part_status_labels_cover_every_state() {
        for (status, label) in [
            (ToolPartStatus::Queued, "queued"),
            (ToolPartStatus::Running, "running"),
            (ToolPartStatus::AwaitingApproval, "approval required"),
            (ToolPartStatus::AwaitingInput, "waiting for input"),
            (ToolPartStatus::Completed, "done"),
            (ToolPartStatus::Failed, "failed"),
            (ToolPartStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(status.label(), label);
        }
    }
}
