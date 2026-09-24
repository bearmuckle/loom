//! Client-side projections of backend state.
//!
//! Everything here is derived from protocol responses and events; the backend
//! stays authoritative.

use std::collections::BTreeSet;

use loom_core::{AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, LoomError};
use loom_protocol::{
    AgentActivityRecord, AgentActivityStatus, AgentRunState, GitDiff, GitRepositoryStatus,
    WorkspaceChange, WorkspaceFile,
};

use crate::{MAX_TIMELINE_OUTPUT, text_input::TextBufferState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReviewPanel {
    Changes,
    Diff,
    Evidence,
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

    pub(crate) fn approval_policy(self) -> ApprovalPolicy {
        match self {
            Self::AutoApprove => ApprovalPolicy::auto_approve(),
            Self::Ask | Self::Edit | Self::Agent => ApprovalPolicy::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReviewState {
    pub(crate) open: bool,
    pub(crate) panel: ReviewPanel,
    pub(crate) changes: Vec<WorkspaceChange>,
    pub(crate) diff: Option<GitDiff>,
    pub(crate) diff_path: Option<String>,
    pub(crate) vcs: Option<GitRepositoryStatus>,
    pub(crate) evidence: Vec<String>,
    pub(crate) selected_file: Option<WorkspaceFile>,
}

#[derive(Clone, Debug)]
pub(crate) struct RenameDialogState {
    pub(crate) session: AgentSessionSnapshot,
    pub(crate) input: TextBufferState,
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
            diff: None,
            diff_path: None,
            vcs: None,
            evidence: Vec::new(),
            selected_file: None,
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

pub(crate) fn upsert_activity(timeline: &mut Vec<TimelineItem>, activity: AgentActivityRecord) {
    if let Some((_, section)) = timeline.iter_mut().enumerate().find_map(|(index, item)| {
        let TimelineItem::ActivitySection { activities } = item else {
            return None;
        };
        if let Some(existing) = activities
            .iter_mut()
            .find(|existing| existing.id == activity.id)
        {
            *existing = activity.clone();
            return Some((index, activities));
        }
        if activity
            .parent_id
            .is_some_and(|parent_id| activities.iter().any(|existing| existing.id == parent_id))
        {
            activities.push(activity.clone());
            return Some((index, activities));
        }
        None
    }) {
        let _ = section;
        return;
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
    use loom_protocol::{AgentActivityData, AgentActivityKind, AgentActivityStatus};

    #[test]
    fn bounded_projection_is_explicit() {
        let value = bounded_to("abcdef", 3);
        assert_eq!(value, "abc\n...[output truncated]");
        assert!(bounded_to("😀😀", 4).starts_with('😀'));
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
