//! The `loom-text/1` event projection.
//!
//! One event becomes a fixed `kind` word and a `body` string. Text output is
//! `<seq> <kind> <body>\n` with the body escaped so it can never span a line;
//! JSON output reuses the same two fields, so the two formats can never drift.
//! Every kind and every fallback variant name is an explicit match arm spelled
//! exactly like its snake_case serde name: no `Debug` output reaches a user.

use loom_core::{
    AgentSessionState, EventSequence, LimitStatus, ProjectWorktreeRecord, ProjectWorktreeStatus,
};
use loom_model::ToolCall;
use loom_protocol::{AgentEvent, AgentRunState, ServerEvent, ToolResult};
use serde_json::Value;

/// One projected event: the fixed kind word and the unescaped body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Rendered {
    pub kind: &'static str,
    pub body: String,
}

/// Keeps the first and last halves of a string that is over budget, rounded to
/// UTF-8 character boundaries, with the exact byte count of what was dropped in
/// between.
pub(crate) fn bound(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let half = budget / 2;
    let mut head_end = half.min(text.len());
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - (budget - half);
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}[... {omitted} bytes omitted ...]{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// Escapes a body so it cannot contain a raw newline and cannot be mistaken for
/// a second field.
pub(crate) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{0}'..='\u{1f}' | '\u{7f}' => {
                escaped.push_str(&format!("\\x{:02X}", character as u32));
            }
            other => escaped.push(other),
        }
    }
    escaped
}

/// The single physical line of one event.
pub(crate) fn line(sequence: EventSequence, rendered: &Rendered) -> String {
    let body = escape(&rendered.body);
    if body.is_empty() {
        format!("{sequence} {}\n", rendered.kind)
    } else {
        format!("{sequence} {} {body}\n", rendered.kind)
    }
}

/// The JSON object one event becomes in `--format jsonl`.
pub(crate) fn json_line(
    sequence: EventSequence,
    stream_epoch: Option<&str>,
    session_id: &str,
    rendered: &Rendered,
    event: &ServerEvent,
) -> Value {
    serde_json::json!({
        "seq": sequence.value(),
        "stream_epoch": stream_epoch,
        "session_id": session_id,
        "kind": rendered.kind,
        "text": escape(&rendered.body),
        "event": serde_json::to_value(event).unwrap_or(Value::Null),
    })
}

fn body(kind: &'static str, parts: impl IntoIterator<Item = String>) -> Rendered {
    let body = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Rendered { kind, body }
}

fn name(kind: &'static str, value: &str) -> Rendered {
    body(kind, [value.to_owned()])
}

fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn call_arguments(call: &ToolCall, budget: usize) -> String {
    bound(&json_text(&call.arguments), budget)
}

/// Projects one server event with the `--max-bytes` budget for tool payloads.
pub(crate) fn render(event: &ServerEvent, budget: usize) -> Rendered {
    match event {
        ServerEvent::AgentSessionCreated { .. } => body("session_created", []),
        ServerEvent::AgentSessionStateChanged { previous, current } => body(
            "session_state",
            [
                session_state_name(*previous).to_owned(),
                session_state_name(*current).to_owned(),
            ],
        ),
        ServerEvent::AgentSessionRenamed { name: renamed, .. } => name("session_renamed", renamed),
        ServerEvent::AgentSessionGoalChanged { goal, .. } => match goal {
            Some(goal) => name("session_goal", goal),
            None => name("session_goal", "cleared"),
        },
        ServerEvent::AgentSessionTitleSourceChanged { source, .. } => {
            body("session_title_source", [source_name(*source).to_owned()])
        }
        ServerEvent::AgentSessionArchived { .. } => body("session_archived", []),
        ServerEvent::AgentSessionRestored { .. } => body("session_restored", []),
        ServerEvent::Agent { event } => render_agent(event, budget),
        other => name("event", server_event_name(other)),
    }
}

fn render_agent(event: &AgentEvent, budget: usize) -> Rendered {
    match event {
        AgentEvent::RunStarted { snapshot } => body(
            "run_started",
            [
                snapshot.id.to_string(),
                run_state_name(snapshot.state).to_owned(),
            ],
        ),
        AgentEvent::PlanProposed { plan, .. } => body("plan", [plan.steps.len().to_string()]),
        AgentEvent::StepStarted { run_id, index, .. } => {
            body("step_started", [run_id.to_string(), index.to_string()])
        }
        AgentEvent::StepCompleted { run_id, index, .. } => {
            body("step_completed", [run_id.to_string(), index.to_string()])
        }
        AgentEvent::ProviderError { error, .. } => body(
            "error",
            [
                "provider".to_owned(),
                error.code.to_string(),
                error.message.clone(),
            ],
        ),
        AgentEvent::ContextError { error, .. } => body(
            "error",
            [
                "context".to_owned(),
                error.code.to_string(),
                error.message.clone(),
            ],
        ),
        AgentEvent::RecoveryRequired { reason, .. } => {
            body("error", ["recovery".to_owned(), reason.clone()])
        }
        AgentEvent::AssistantMessageDelta { text, .. } => name("assistant", text),
        AgentEvent::ReasoningDelta { text, .. } => name("reasoning", text),
        AgentEvent::UserMessage { text, .. } => name("user", text),
        AgentEvent::NeedsInput { prompt, .. } => name("question", prompt),
        AgentEvent::ToolCallRequested { call, .. } => body(
            "tool_call",
            [call.name.clone(), call_arguments(call, budget)],
        ),
        AgentEvent::ToolApprovalRequired { call, .. } => body(
            "pending_approval",
            [call.name.clone(), call_arguments(call, budget)],
        ),
        AgentEvent::ToolApprovalDecided {
            tool_call_id,
            decision,
            ..
        } => body(
            "approval_decided",
            [
                tool_call_id.to_string(),
                approval_name(*decision).to_owned(),
            ],
        ),
        AgentEvent::ToolCallStarted { call, .. } => {
            body("tool_start", [call.name.clone(), call.id.to_string()])
        }
        AgentEvent::ToolOutputChunk {
            tool_call_id,
            chunk,
            ..
        } => body(
            "tool_output",
            [tool_call_id.to_string(), bound(chunk, budget)],
        ),
        AgentEvent::ToolCallCompleted { result, .. } => tool_done(result, budget),
        AgentEvent::RunLimitReached { run_id, status } => body(
            "limit",
            [run_id.to_string(), json_text(&json_value(status))],
        ),
        AgentEvent::RunStateChanged { run_id, state } => body(
            "run_state",
            [run_id.to_string(), run_state_name(*state).to_owned()],
        ),
        AgentEvent::RunCompleted { snapshot } => body(
            "run_completed",
            [
                snapshot.id.to_string(),
                run_state_name(snapshot.state).to_owned(),
                snapshot.summary.clone().unwrap_or_default(),
            ],
        ),
        other => name("event", agent_event_name(other)),
    }
}

fn tool_done(result: &ToolResult, budget: usize) -> Rendered {
    let outcome = if result.success { "ok" } else { "error" };
    body(
        "tool_done",
        [
            result.tool_call_id.to_string(),
            outcome.to_owned(),
            bound(&result.output, budget),
        ],
    )
}

fn json_value(status: &LimitStatus) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

pub(crate) const fn session_state_name(state: AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Idle => "idle",
        AgentSessionState::Queued => "queued",
        AgentSessionState::Planning => "planning",
        AgentSessionState::AwaitingApproval => "awaiting_approval",
        AgentSessionState::Paused => "paused",
        AgentSessionState::Executing => "executing",
        AgentSessionState::Evaluating => "evaluating",
        AgentSessionState::NeedsInput => "needs_input",
        AgentSessionState::Completed => "completed",
        AgentSessionState::Failed => "failed",
        AgentSessionState::Cancelled => "cancelled",
        AgentSessionState::Archived => "archived",
    }
}

pub(crate) const fn run_state_name(state: AgentRunState) -> &'static str {
    match state {
        AgentRunState::Planning => "planning",
        AgentRunState::Executing => "executing",
        AgentRunState::AwaitingApproval => "awaiting_approval",
        AgentRunState::Paused => "paused",
        AgentRunState::NeedsInput => "needs_input",
        AgentRunState::Evaluating => "evaluating",
        AgentRunState::Completed => "completed",
        AgentRunState::Failed => "failed",
        AgentRunState::Cancelled => "cancelled",
    }
}

/// The status word of a worktree: the protocol's serde spelling, stated
/// explicitly so a wire rename cannot change the text contract.
pub(crate) const fn worktree_status_name(status: ProjectWorktreeStatus) -> &'static str {
    match status {
        ProjectWorktreeStatus::Creating => "creating",
        ProjectWorktreeStatus::Ready => "ready",
        ProjectWorktreeStatus::Stale => "stale",
        ProjectWorktreeStatus::Conflict => "conflict",
        ProjectWorktreeStatus::Integrating => "integrating",
        ProjectWorktreeStatus::Integrated => "integrated",
        ProjectWorktreeStatus::RecoveryRequired => "recovery_required",
        ProjectWorktreeStatus::CleanupPending => "cleanup_pending",
        ProjectWorktreeStatus::Retained => "retained",
        ProjectWorktreeStatus::Removed => "removed",
    }
}

/// The one `worktree` record line `status --tree` and `project worktrees` print,
/// so a record reads the same in either verb.
///
/// The name and path lead, so the `worktree <name> <path>` header of the
/// single-worktree verbs is already a prefix of this line.
pub(crate) fn worktree_line(worktree: &ProjectWorktreeRecord) -> String {
    format!(
        "worktree {} {} {} {} {} {} {}\n",
        escape(&worktree.worktree_name),
        escape(&worktree.relative_path),
        worktree_status_name(worktree.status),
        worktree.task_id,
        worktree.base_revision,
        worktree.result_revision.as_deref().unwrap_or("-"),
        worktree.integrated_revision.as_deref().unwrap_or("-"),
    )
}

fn source_name(source: loom_core::SessionTitleSource) -> &'static str {
    match source {
        loom_core::SessionTitleSource::Auto => "auto",
        loom_core::SessionTitleSource::Manual => "manual",
    }
}

fn approval_name(decision: loom_protocol::ApprovalDecision) -> &'static str {
    match decision {
        loom_protocol::ApprovalDecision::Approved => "approved",
        loom_protocol::ApprovalDecision::Rejected => "rejected",
    }
}

/// The snake_case name of a server event, for the fallback `event` line.
pub(crate) const fn server_event_name(event: &ServerEvent) -> &'static str {
    match event {
        ServerEvent::ProjectChildWorktreeUpdated { .. } => "project_child_worktree_updated",
        ServerEvent::ProjectTaskUpdated { .. } => "project_task_updated",
        ServerEvent::ProjectAgentMessageAccepted { .. } => "project_agent_message_accepted",
        ServerEvent::ProjectAgentCreated { .. } => "project_agent_created",
        ServerEvent::ProjectAgentUpdated { .. } => "project_agent_updated",
        ServerEvent::AgentSessionCreated { .. } => "agent_session_created",
        ServerEvent::AgentSessionStateChanged { .. } => "agent_session_state_changed",
        ServerEvent::AgentSessionForked { .. } => "agent_session_forked",
        ServerEvent::AgentSessionRenamed { .. } => "agent_session_renamed",
        ServerEvent::AgentSessionGoalChanged { .. } => "agent_session_goal_changed",
        ServerEvent::AgentSessionTitleSourceChanged { .. } => "agent_session_title_source_changed",
        ServerEvent::AgentSessionArchived { .. } => "agent_session_archived",
        ServerEvent::AgentSessionRestored { .. } => "agent_session_restored",
        ServerEvent::Agent { .. } => "agent",
        ServerEvent::SessionFilesystemChanged { .. } => "session_filesystem_changed",
        ServerEvent::Terminal { .. } => "terminal",
        ServerEvent::Task { .. } => "task",
        ServerEvent::ProviderHealthChanged { .. } => "provider_health_changed",
        ServerEvent::Collaboration { .. } => "collaboration",
        ServerEvent::SessionRosterChanged { .. } => "session_roster_changed",
        ServerEvent::SessionAccessRequested { .. } => "session_access_requested",
        ServerEvent::SessionAccessRequestResolved { .. } => "session_access_request_resolved",
    }
}

/// The snake_case name of an agent event, for the fallback `event` line.
pub(crate) const fn agent_event_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::RunStarted { .. } => "run_started",
        AgentEvent::PlanProposed { .. } => "plan_proposed",
        AgentEvent::StepStarted { .. } => "step_started",
        AgentEvent::StepCompleted { .. } => "step_completed",
        AgentEvent::ContextInspected { .. } => "context_inspected",
        AgentEvent::ProviderError { .. } => "provider_error",
        AgentEvent::ContextError { .. } => "context_error",
        AgentEvent::AssistantMessageDelta { .. } => "assistant_message_delta",
        AgentEvent::ReasoningDelta { .. } => "reasoning_delta",
        AgentEvent::UserMessage { .. } => "user_message",
        AgentEvent::NeedsInput { .. } => "needs_input",
        AgentEvent::ToolCallRequested { .. } => "tool_call_requested",
        AgentEvent::ToolApprovalRequired { .. } => "tool_approval_required",
        AgentEvent::ToolPolicyEvaluated { .. } => "tool_policy_evaluated",
        AgentEvent::ToolApprovalDecided { .. } => "tool_approval_decided",
        AgentEvent::ToolCallStarted { .. } => "tool_call_started",
        AgentEvent::ToolOutputChunk { .. } => "tool_output_chunk",
        AgentEvent::ToolCallCompleted { .. } => "tool_call_completed",
        AgentEvent::ActivityRecorded { .. } => "activity_recorded",
        AgentEvent::RunUsage { .. } => "run_usage",
        AgentEvent::RunUsageUpdated { .. } => "run_usage_updated",
        AgentEvent::RunLimitReached { .. } => "run_limit_reached",
        AgentEvent::RecoveryRequired { .. } => "recovery_required",
        AgentEvent::RunStateChanged { .. } => "run_state_changed",
        AgentEvent::RunCompleted { .. } => "run_completed",
    }
}
