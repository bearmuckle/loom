//! Client-side projections of backend state.
//!
//! Everything here is derived from protocol responses and events; the backend
//! stays authoritative.

use std::collections::BTreeSet;

use gpui_kit::{ListAlignment, ListState, px};
use loom_core::{AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, LoomError};
use loom_protocol::{
    AgentActivityRecord, AgentActivityStatus, AgentRunState, GitDiff, GitDiffLine,
    GitRepositoryStatus, SessionFilesystemChange, SessionFilesystemFile,
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

#[derive(Clone, Debug)]
pub(crate) enum TimelineItem {
    User(String),
    Assistant(String),
    ActivitySection {
        activities: Vec<AgentActivityRecord>,
    },
    Plan {
        steps: Vec<String>,
        completed: BTreeSet<u32>,
        active: Option<u32>,
    },
    ToolRequested {
        name: String,
        arguments: String,
    },
    Approval {
        name: String,
        active: bool,
    },
    ToolStarted(String),
    ToolOutput(String),
    ToolCompleted {
        name: String,
        success: bool,
    },
    Status(String),
    Error {
        operation: String,
        error: LoomError,
    },
    NeedsInput(String),
    Summary {
        text: String,
        evidence: Vec<String>,
    },
}

/// Project consecutive activities from one run into one section. Transcript messages,
/// status items, and run boundaries break a group. Updates replace records in place so
/// late results never reorder the transcript.
pub(crate) fn upsert_activity(timeline: &mut Vec<TimelineItem>, activity: AgentActivityRecord) {
    for item in timeline.iter_mut() {
        if let TimelineItem::ActivitySection { activities } = item
            && let Some(existing) = activities.iter_mut().find(|item| item.id == activity.id)
        {
            *existing = activity;
            return;
        }
    }

    if let Some(TimelineItem::ActivitySection { activities }) = timeline.last_mut() {
        let same_run = activities.iter().all(|item| item.run_id == activity.run_id);
        if same_run {
            activities.push(activity);
            return;
        }
    }
    timeline.push(TimelineItem::ActivitySection {
        activities: vec![activity],
    });
}

pub(crate) fn activity_status_label(status: AgentActivityStatus) -> &'static str {
    match status {
        AgentActivityStatus::Started => "running",
        AgentActivityStatus::Completed => "done",
        AgentActivityStatus::Failed => "failed",
        AgentActivityStatus::AwaitingApproval => "approval required",
        AgentActivityStatus::AwaitingInput => "waiting for input",
        AgentActivityStatus::Cancelled => "cancelled",
    }
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
    use loom_core::{ActivityId, RunId, Timestamp, ToolCallId};
    use loom_model::{ModelId, ToolCall};
    use loom_protocol::{
        AgentActivityData, AgentActivityKind, AgentActivityStatus, GitDiffHunk, GitDiffLineKind,
    };

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

    fn turn(run_id: RunId) -> AgentActivityRecord {
        AgentActivityRecord {
            id: ActivityId::new(),
            run_id,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ModelTurn,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(1),
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::ModelTurn {
                model: ModelId::new("test/model"),
            },
        }
    }

    fn command(parent: &AgentActivityRecord) -> AgentActivityRecord {
        AgentActivityRecord {
            id: ActivityId::new(),
            parent_id: Some(parent.id),
            kind: AgentActivityKind::Command,
            data: AgentActivityData::Command {
                call: ToolCall {
                    id: ToolCallId::new(),
                    name: "run_command".to_owned(),
                    arguments: serde_json::json!({}),
                },
                command: "cargo".to_owned(),
                args: vec!["test".to_owned()],
                cwd: None,
                result: None,
            },
            ..parent.clone()
        }
    }

    #[test]
    fn consecutive_commands_span_turns_and_keep_late_results_in_place() {
        let run_id = RunId::new();
        let first_turn = turn(run_id);
        let first = command(&first_turn);
        let second_turn = turn(run_id);
        let second = command(&second_turn);
        let records = [first_turn, first.clone(), second_turn, second.clone()];
        let mut timeline = Vec::new();
        for record in &records {
            upsert_activity(&mut timeline, record.clone());
        }
        assert_eq!(timeline.len(), 1);
        timeline.push(TimelineItem::Assistant("Finished checks".to_owned()));
        let mut finished = first.clone();
        finished.status = AgentActivityStatus::Failed;
        if let AgentActivityData::Command { call, result, .. } = &mut finished.data {
            *result = Some(loom_protocol::ToolResult::failure(call, "test failed"));
        }
        upsert_activity(&mut timeline, finished.clone());
        let TimelineItem::ActivitySection { activities } = &timeline[0] else {
            panic!("expected group")
        };
        assert_eq!(
            activities,
            &[records[0].clone(), finished, records[2].clone(), second]
        );
        assert_eq!(timeline.len(), 2);

        // Snapshot replay takes the same path as streamed activity records.
        let mut restored = Vec::new();
        for record in records {
            upsert_activity(&mut restored, record);
        }
        assert_eq!(restored.len(), 1);
    }

    #[test]
    fn activities_do_not_cross_messages_or_run_boundaries() {
        for separator in [
            TimelineItem::Assistant("Checking another area".to_owned()),
            TimelineItem::User("Next task".to_owned()),
            TimelineItem::Status("Paused".to_owned()),
        ] {
            let parent = turn(RunId::new());
            let mut timeline = Vec::new();
            upsert_activity(&mut timeline, parent.clone());
            upsert_activity(&mut timeline, command(&parent));
            timeline.push(separator);
            upsert_activity(&mut timeline, command(&parent));
            assert_eq!(timeline.len(), 3);
        }
        let parent = turn(RunId::new());
        let mut timeline = Vec::new();
        upsert_activity(&mut timeline, parent.clone());
        upsert_activity(&mut timeline, command(&parent));
        let other_tool = AgentActivityRecord {
            id: ActivityId::new(),
            parent_id: Some(parent.id),
            kind: AgentActivityKind::ToolCall,
            data: AgentActivityData::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(),
                    name: "read_file".to_owned(),
                    arguments: serde_json::json!({}),
                },
                result: None,
            },
            ..parent.clone()
        };
        upsert_activity(&mut timeline, other_tool.clone());
        upsert_activity(&mut timeline, command(&parent));
        assert_eq!(timeline.len(), 1);
        let other_run = turn(RunId::new());
        upsert_activity(&mut timeline, other_run.clone());
        upsert_activity(&mut timeline, command(&other_run));
        assert_eq!(timeline.len(), 2);
        upsert_activity(&mut timeline, other_tool);
        assert_eq!(timeline.len(), 2);
    }

    #[test]
    fn activity_updates_are_grouped_by_parent_and_id() {
        let run_id = RunId::new();
        let turn_id = ActivityId::new();
        let turn = AgentActivityRecord {
            id: turn_id,
            run_id,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ModelTurn,
            status: AgentActivityStatus::Started,
            started_at: Timestamp::from_unix_millis(1),
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::ModelTurn {
                model: ModelId::new("test/model"),
            },
        };
        let mut timeline = Vec::new();
        upsert_activity(&mut timeline, turn.clone());
        upsert_activity(
            &mut timeline,
            AgentActivityRecord {
                id: ActivityId::new(),
                run_id,
                parent_id: Some(turn_id),
                step_id: None,
                kind: AgentActivityKind::ToolCall,
                status: AgentActivityStatus::Completed,
                started_at: Timestamp::from_unix_millis(2),
                completed_at: Some(Timestamp::from_unix_millis(3)),
                elapsed_ms: Some(1),
                data: AgentActivityData::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new(),
                        name: "read_file".to_owned(),
                        arguments: serde_json::json!({"path": "README.md"}),
                    },
                    result: None,
                },
            },
        );
        upsert_activity(
            &mut timeline,
            AgentActivityRecord {
                status: AgentActivityStatus::Completed,
                completed_at: Some(Timestamp::from_unix_millis(4)),
                elapsed_ms: Some(3),
                ..turn
            },
        );

        assert_eq!(
            timeline
                .iter()
                .filter(|item| matches!(item, TimelineItem::ActivitySection { .. }))
                .count(),
            1
        );
        let TimelineItem::ActivitySection { activities } = &timeline[0] else {
            panic!("expected activity section");
        };
        assert_eq!(activities.len(), 2);
        assert_eq!(activities[0].status, AgentActivityStatus::Completed);
    }
}
