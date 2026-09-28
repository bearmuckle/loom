//! Client-side projections of backend state.
//!
//! Everything here is derived from protocol responses and events; the backend
//! stays authoritative.

use std::collections::BTreeSet;

use gpui_kit::{ListAlignment, ListState, px};
use loom_core::{
    AgentMessageRecord, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, ToolCallId,
};
use loom_protocol::{
    AgentRunState, GitDiff, GitDiffLine, GitRepositoryStatus, SessionFilesystemChange,
    SessionFilesystemFile,
};

use crate::MAX_TIMELINE_OUTPUT;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReviewPanel {
    Changes,
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
    pub(crate) panel: ReviewPanel,
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
    pub(crate) rows: Vec<ReviewRow>,
    pub(crate) hunk_rows: Vec<usize>,
    pub(crate) selected_hunk: usize,
    pub(crate) list_state: ListState,
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
        self.rows.clear();
        self.hunk_rows.clear();
        for hunk in &diff.hunks {
            self.hunk_rows.push(self.rows.len());
            self.rows.push(ReviewRow::Hunk {
                old_start: hunk.old_start,
                old_lines: hunk.old_lines,
                new_start: hunk.new_start,
                new_lines: hunk.new_lines,
            });
            self.rows
                .extend(hunk.lines.iter().cloned().map(ReviewRow::Line));
        }
        self.list_state.reset(self.rows.len());
        self.selected_hunk = 0;
        self.selected_diff = Some(diff);
        self.loading_diff = false;
        self.diff_error = None;
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

impl Default for ReviewState {
    fn default() -> Self {
        Self {
            open: false,
            panel: ReviewPanel::Changes,
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
            rows: Vec::new(),
            hunk_rows: Vec::new(),
            selected_hunk: 0,
            list_state: ListState::new(0, ListAlignment::Top, px(120.)),
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
    Plan {
        steps: Vec<String>,
        completed: BTreeSet<u32>,
        active: Option<u32>,
    },
    ProjectMessage(AgentMessageRecord),
    ProjectMessageContext(String),
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

    pub(crate) const fn marker(self) -> &'static str {
        match self {
            Self::Queued => "○",
            Self::Running => "›",
            Self::AwaitingApproval => "!",
            Self::AwaitingInput => "?",
            Self::Completed => "✓",
            Self::Failed => "×",
            Self::Cancelled => "–",
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

/// Appends streamed assistant text. Once a turn has ended with a tool call,
/// further text starts a fresh turn so each response reads as its own message.
pub(crate) fn push_assistant_text(timeline: &mut Vec<TimelineItem>, text: &str) {
    if text.is_empty() {
        return;
    }
    let starts_new_turn = !matches!(
        timeline.last(),
        Some(TimelineItem::Assistant(turn))
            if matches!(
                turn.parts.last(),
                None | Some(AssistantPart::Text(_)) | Some(AssistantPart::Reasoning(_))
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
                None | Some(AssistantPart::Text(_)) | Some(AssistantPart::Reasoning(_))
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
    fn assistant_deltas_merge_and_restart_after_a_tool_part() {
        let mut timeline = Vec::new();
        push_assistant_text(&mut timeline, "Hello ");
        push_assistant_text(&mut timeline, "world");
        assert_eq!(timeline.len(), 1);
        let TimelineItem::Assistant(turn) = &timeline[0] else {
            panic!("expected assistant turn");
        };
        assert!(turn.streaming);
        assert_eq!(
            turn.parts,
            vec![AssistantPart::Text("Hello world".to_owned())]
        );

        upsert_tool_part(
            &mut timeline,
            tool(ToolCallId::new(), "read_file", ToolPartStatus::Queued),
        );
        push_assistant_text(&mut timeline, "Next response");
        assert_eq!(timeline.len(), 2);
        let TimelineItem::Assistant(second) = &timeline[1] else {
            panic!("expected second assistant turn");
        };
        assert_eq!(
            second.parts,
            vec![AssistantPart::Text("Next response".to_owned())]
        );
    }

    #[test]
    fn tool_updates_find_the_existing_part_across_interleaved_items() {
        let id = ToolCallId::new();
        let mut timeline = Vec::new();
        upsert_tool_part(&mut timeline, tool(id, "read_file", ToolPartStatus::Queued));
        push_assistant_text(&mut timeline, "Working on it");
        let mut completed = tool(id, "read_file", ToolPartStatus::Completed);
        completed.output = Some("file contents".to_owned());
        completed.elapsed_ms = Some(12);
        upsert_tool_part(&mut timeline, completed.clone());
        assert_eq!(timeline.len(), 2);

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
        assert!(!parts.iter().any(|_| false));
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
    fn tool_part_status_markers_cover_every_state() {
        for (status, marker) in [
            (ToolPartStatus::Queued, "○"),
            (ToolPartStatus::Running, "›"),
            (ToolPartStatus::AwaitingApproval, "!"),
            (ToolPartStatus::AwaitingInput, "?"),
            (ToolPartStatus::Completed, "✓"),
            (ToolPartStatus::Failed, "×"),
            (ToolPartStatus::Cancelled, "–"),
        ] {
            assert_eq!(status.marker(), marker);
        }
    }
}
