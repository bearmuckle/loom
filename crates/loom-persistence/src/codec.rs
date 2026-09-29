use super::*;

pub(crate) fn message_role_name(role: loom_model::MessageRole) -> &'static str {
    match role {
        loom_model::MessageRole::System => "system",
        loom_model::MessageRole::User => "user",
        loom_model::MessageRole::Assistant => "assistant",
        loom_model::MessageRole::Tool => "tool",
    }
}

pub(crate) fn parse_message_role(role: &str) -> Result<loom_model::MessageRole> {
    match role {
        "system" => Ok(loom_model::MessageRole::System),
        "user" => Ok(loom_model::MessageRole::User),
        "assistant" => Ok(loom_model::MessageRole::Assistant),
        "tool" => Ok(loom_model::MessageRole::Tool),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted message has an unknown role",
            false,
        )),
    }
}

pub(crate) fn activity_kind_name(kind: AgentActivityKind) -> &'static str {
    match kind {
        AgentActivityKind::ModelTurn => "model_turn",
        AgentActivityKind::ToolCall => "tool_call",
        AgentActivityKind::File => "file",
        AgentActivityKind::Search => "search",
        AgentActivityKind::Command => "command",
    }
}

pub(crate) fn parse_activity_kind(kind: &str) -> Result<AgentActivityKind> {
    match kind {
        "model_turn" => Ok(AgentActivityKind::ModelTurn),
        "tool_call" => Ok(AgentActivityKind::ToolCall),
        "file" => Ok(AgentActivityKind::File),
        "search" => Ok(AgentActivityKind::Search),
        "command" => Ok(AgentActivityKind::Command),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted activity has an unknown kind",
            false,
        )),
    }
}

pub(crate) fn activity_status_name(status: AgentActivityStatus) -> &'static str {
    match status {
        AgentActivityStatus::Started => "started",
        AgentActivityStatus::Completed => "completed",
        AgentActivityStatus::Failed => "failed",
        AgentActivityStatus::AwaitingApproval => "awaiting_approval",
        AgentActivityStatus::AwaitingInput => "awaiting_input",
        AgentActivityStatus::Cancelled => "cancelled",
    }
}

pub(crate) fn tool_attempt_state_name(state: AgentToolAttemptState) -> &'static str {
    match state {
        AgentToolAttemptState::Queued => "queued",
        AgentToolAttemptState::Running => "running",
        AgentToolAttemptState::AwaitingApproval => "awaiting_approval",
        AgentToolAttemptState::AwaitingInput => "awaiting_input",
        AgentToolAttemptState::Completed => "completed",
        AgentToolAttemptState::Failed => "failed",
        AgentToolAttemptState::Cancelled => "cancelled",
        AgentToolAttemptState::OutcomeUnknown => "outcome_unknown",
    }
}

pub(crate) fn parse_tool_attempt_state(state: &str) -> Result<AgentToolAttemptState> {
    match state {
        "queued" => Ok(AgentToolAttemptState::Queued),
        "running" => Ok(AgentToolAttemptState::Running),
        "awaiting_approval" => Ok(AgentToolAttemptState::AwaitingApproval),
        "awaiting_input" => Ok(AgentToolAttemptState::AwaitingInput),
        "completed" => Ok(AgentToolAttemptState::Completed),
        "failed" => Ok(AgentToolAttemptState::Failed),
        "cancelled" => Ok(AgentToolAttemptState::Cancelled),
        "outcome_unknown" => Ok(AgentToolAttemptState::OutcomeUnknown),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted tool-attempt state '{state}' is invalid"),
            false,
        )),
    }
}

pub(crate) fn parse_interaction_kind(kind: &str) -> Result<AgentInteractionKind> {
    match kind {
        "tool_approval" => Ok(AgentInteractionKind::ToolApproval),
        "user_input" => Ok(AgentInteractionKind::UserInput),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted interaction kind '{kind}' is invalid"),
            false,
        )),
    }
}

pub(crate) fn parse_interaction_status(status: &str) -> Result<AgentInteractionStatus> {
    match status {
        "pending" => Ok(AgentInteractionStatus::Pending),
        "approved" => Ok(AgentInteractionStatus::Approved),
        "rejected" => Ok(AgentInteractionStatus::Rejected),
        "answered" => Ok(AgentInteractionStatus::Answered),
        "abandoned" => Ok(AgentInteractionStatus::Abandoned),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted interaction status '{status}' is invalid"),
            false,
        )),
    }
}

pub(crate) fn parse_approval_decision(decision: &str) -> Result<ApprovalDecision> {
    match decision {
        "approved" => Ok(ApprovalDecision::Approved),
        "rejected" => Ok(ApprovalDecision::Rejected),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted approval decision '{decision}' is invalid"),
            false,
        )),
    }
}

pub(crate) fn interaction_kind_name(kind: AgentInteractionKind) -> &'static str {
    match kind {
        AgentInteractionKind::ToolApproval => "tool_approval",
        AgentInteractionKind::UserInput => "user_input",
    }
}

pub(crate) fn interaction_status_name(status: AgentInteractionStatus) -> &'static str {
    match status {
        AgentInteractionStatus::Pending => "pending",
        AgentInteractionStatus::Approved => "approved",
        AgentInteractionStatus::Rejected => "rejected",
        AgentInteractionStatus::Answered => "answered",
        AgentInteractionStatus::Abandoned => "abandoned",
    }
}

pub(crate) fn approval_decision_name(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Approved => "approved",
        ApprovalDecision::Rejected => "rejected",
    }
}

pub(crate) fn parse_activity_status(status: &str) -> Result<AgentActivityStatus> {
    match status {
        "started" => Ok(AgentActivityStatus::Started),
        "completed" => Ok(AgentActivityStatus::Completed),
        "failed" => Ok(AgentActivityStatus::Failed),
        "awaiting_approval" => Ok(AgentActivityStatus::AwaitingApproval),
        "awaiting_input" => Ok(AgentActivityStatus::AwaitingInput),
        "cancelled" => Ok(AgentActivityStatus::Cancelled),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted activity has an unknown status",
            false,
        )),
    }
}

pub(crate) fn activity_data_tool_call_id(
    data: &AgentActivityData,
) -> Option<loom_core::ToolCallId> {
    match data {
        AgentActivityData::ModelTurn { .. } => None,
        AgentActivityData::ToolCall { call, .. }
        | AgentActivityData::File { call, .. }
        | AgentActivityData::Search { call, .. }
        | AgentActivityData::Command { call, .. } => Some(call.id),
    }
}

pub(crate) fn activity_data_kind(data: &AgentActivityData) -> AgentActivityKind {
    match data {
        AgentActivityData::ModelTurn { .. } => AgentActivityKind::ModelTurn,
        AgentActivityData::ToolCall { .. } => AgentActivityKind::ToolCall,
        AgentActivityData::File { .. } => AgentActivityKind::File,
        AgentActivityData::Search { .. } => AgentActivityKind::Search,
        AgentActivityData::Command { .. } => AgentActivityKind::Command,
    }
}

pub(crate) fn decode_optional_tool_call_id(
    id: Option<Vec<u8>>,
) -> Result<Option<loom_core::ToolCallId>> {
    id.map(|id| {
        Uuid::from_slice(&id)
            .map(loom_core::ToolCallId::from_uuid)
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted tool call id is malformed: {error}"),
                    false,
                )
            })
    })
    .transpose()
}

pub(crate) fn decode_optional_activity_id(id: Option<Vec<u8>>) -> Result<Option<ActivityId>> {
    id.map(|id| decode_uuid(&id, "parent activity id").map(ActivityId::from_uuid))
        .transpose()
}

pub(crate) fn decode_optional_step_id(id: Option<Vec<u8>>) -> Result<Option<StepId>> {
    id.map(|id| decode_uuid(&id, "activity step id").map(StepId::from_uuid))
        .transpose()
}

pub(crate) fn session_state_name(state: AgentSessionState) -> &'static str {
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

pub(crate) fn run_state_name(state: AgentRunState) -> &'static str {
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

pub(crate) fn parse_run_state(state: &str) -> Result<AgentRunState> {
    match state {
        "planning" => Ok(AgentRunState::Planning),
        "executing" => Ok(AgentRunState::Executing),
        "awaiting_approval" => Ok(AgentRunState::AwaitingApproval),
        "paused" => Ok(AgentRunState::Paused),
        "needs_input" => Ok(AgentRunState::NeedsInput),
        "evaluating" => Ok(AgentRunState::Evaluating),
        "completed" => Ok(AgentRunState::Completed),
        "failed" => Ok(AgentRunState::Failed),
        "cancelled" => Ok(AgentRunState::Cancelled),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted run attempt state is unknown",
            false,
        )),
    }
}

pub(crate) fn workspace_control_name(control: WorkspaceControl) -> &'static str {
    match control {
        WorkspaceControl::Agent => "agent",
        WorkspaceControl::User => "user",
    }
}

pub(crate) fn parse_workspace_control(control: &str) -> Result<WorkspaceControl> {
    match control {
        "agent" => Ok(WorkspaceControl::Agent),
        "user" => Ok(WorkspaceControl::User),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted filesystem has an unknown control state",
            false,
        )),
    }
}

pub(crate) fn workspace_change_kind_name(kind: WorkspaceChangeKind) -> &'static str {
    match kind {
        WorkspaceChangeKind::Created => "created",
        WorkspaceChangeKind::Modified => "modified",
        WorkspaceChangeKind::Deleted => "deleted",
    }
}

pub(crate) fn parse_workspace_change_kind(kind: &str) -> Result<WorkspaceChangeKind> {
    match kind {
        "created" => Ok(WorkspaceChangeKind::Created),
        "modified" => Ok(WorkspaceChangeKind::Modified),
        "deleted" => Ok(WorkspaceChangeKind::Deleted),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted filesystem change has an unknown kind",
            false,
        )),
    }
}

pub(crate) fn parse_session_state(state: &str) -> Result<AgentSessionState> {
    match state {
        "idle" => Ok(AgentSessionState::Idle),
        "queued" => Ok(AgentSessionState::Queued),
        "planning" => Ok(AgentSessionState::Planning),
        "awaiting_approval" => Ok(AgentSessionState::AwaitingApproval),
        "paused" => Ok(AgentSessionState::Paused),
        "executing" => Ok(AgentSessionState::Executing),
        "evaluating" => Ok(AgentSessionState::Evaluating),
        "needs_input" => Ok(AgentSessionState::NeedsInput),
        "completed" => Ok(AgentSessionState::Completed),
        "failed" => Ok(AgentSessionState::Failed),
        "cancelled" => Ok(AgentSessionState::Cancelled),
        "archived" => Ok(AgentSessionState::Archived),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted session has an unknown state '{state}'"),
            false,
        )),
    }
}

pub(crate) fn encode_timestamp(timestamp: Timestamp) -> Result<i64> {
    i64::try_from(timestamp.as_unix_millis()).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "timestamp exceeds SQLite's integer range",
            false,
        )
    })
}

pub(crate) fn encode_counter(counter: u64, field: &str) -> Result<i64> {
    i64::try_from(counter).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            format!("{field} exceeds SQLite's integer range"),
            false,
        )
    })
}

pub(crate) fn decode_counter(counter: i64, field: &str) -> Result<u64> {
    u64::try_from(counter).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {field} is negative"),
            false,
        )
    })
}

pub(crate) fn decode_optional_u64(counter: Option<i64>, field: &str) -> Result<Option<u64>> {
    counter
        .map(|value| decode_counter(value, field))
        .transpose()
}

pub(crate) fn encode_optional_counter(counter: Option<u64>, field: &str) -> Result<Option<i64>> {
    counter
        .map(|value| encode_counter(value, field))
        .transpose()
}

pub(crate) fn encode_policy_decision(decision: PolicyDecision) -> &'static str {
    match decision {
        PolicyDecision::Allow => "allow",
        PolicyDecision::RequireApproval => "require_approval",
        PolicyDecision::Deny => "deny",
    }
}

pub(crate) fn decode_policy_decision(value: &str, action: &str) -> Result<PolicyDecision> {
    match value {
        "allow" => Ok(PolicyDecision::Allow),
        "require_approval" => Ok(PolicyDecision::RequireApproval),
        "deny" => Ok(PolicyDecision::Deny),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {action} approval decision is invalid"),
            false,
        )),
    }
}

pub(crate) fn decode_timestamp(timestamp: i64) -> Result<Timestamp> {
    u64::try_from(timestamp)
        .map(Timestamp::from_unix_millis)
        .map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted timestamp is negative",
                false,
            )
        })
}

pub(crate) fn decode_uuid(bytes: &[u8], field: &str) -> Result<Uuid> {
    Uuid::from_slice(bytes).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {field} is invalid: {error}"),
            false,
        )
    })
}

pub(crate) fn persistence_error(message: String, retryable: bool) -> LoomError {
    LoomError::new(ErrorCode::Persistence, message, retryable)
}

pub(crate) fn decode_json<T: DeserializeOwned>(payload: &str, field: &str) -> Result<T> {
    serde_json::from_str(payload).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {field} is malformed: {error}"),
            false,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_and_session_states_round_trip() {
        for state in [
            AgentRunState::Planning,
            AgentRunState::Executing,
            AgentRunState::Completed,
            AgentRunState::Failed,
            AgentRunState::Cancelled,
        ] {
            assert_eq!(parse_run_state(run_state_name(state)).unwrap(), state);
        }
        for state in [
            AgentSessionState::Idle,
            AgentSessionState::Executing,
            AgentSessionState::Completed,
            AgentSessionState::Failed,
        ] {
            assert_eq!(
                parse_session_state(session_state_name(state)).unwrap(),
                state
            );
        }
        assert!(parse_run_state("bogus").is_err());
        assert!(parse_session_state("bogus").is_err());
    }

    #[test]
    fn activity_and_interaction_codecs_round_trip() {
        for kind in [
            AgentActivityKind::ModelTurn,
            AgentActivityKind::ToolCall,
            AgentActivityKind::File,
            AgentActivityKind::Search,
            AgentActivityKind::Command,
        ] {
            assert_eq!(parse_activity_kind(activity_kind_name(kind)).unwrap(), kind);
        }
        for status in [
            AgentActivityStatus::Started,
            AgentActivityStatus::Completed,
            AgentActivityStatus::Failed,
            AgentActivityStatus::Cancelled,
        ] {
            assert_eq!(
                parse_activity_status(activity_status_name(status)).unwrap(),
                status
            );
        }
        for state in [
            AgentToolAttemptState::Queued,
            AgentToolAttemptState::Running,
            AgentToolAttemptState::Completed,
            AgentToolAttemptState::OutcomeUnknown,
        ] {
            assert_eq!(
                parse_tool_attempt_state(tool_attempt_state_name(state)).unwrap(),
                state
            );
        }
        for kind in [
            AgentInteractionKind::ToolApproval,
            AgentInteractionKind::UserInput,
        ] {
            assert_eq!(
                parse_interaction_kind(interaction_kind_name(kind)).unwrap(),
                kind
            );
        }
        for status in [
            AgentInteractionStatus::Pending,
            AgentInteractionStatus::Answered,
            AgentInteractionStatus::Abandoned,
        ] {
            assert_eq!(
                parse_interaction_status(interaction_status_name(status)).unwrap(),
                status
            );
        }
        for decision in [ApprovalDecision::Approved, ApprovalDecision::Rejected] {
            assert_eq!(
                parse_approval_decision(approval_decision_name(decision)).unwrap(),
                decision
            );
        }
        for control in [WorkspaceControl::Agent, WorkspaceControl::User] {
            assert_eq!(
                parse_workspace_control(workspace_control_name(control)).unwrap(),
                control
            );
        }
        for kind in [
            WorkspaceChangeKind::Created,
            WorkspaceChangeKind::Modified,
            WorkspaceChangeKind::Deleted,
        ] {
            assert_eq!(
                parse_workspace_change_kind(workspace_change_kind_name(kind)).unwrap(),
                kind
            );
        }
        assert!(parse_activity_kind("bogus").is_err());
        assert!(parse_interaction_status("bogus").is_err());
        assert!(parse_approval_decision("bogus").is_err());
        assert!(parse_workspace_control("bogus").is_err());
        assert!(parse_workspace_change_kind("bogus").is_err());
    }

    #[test]
    fn counters_and_timestamps_validate_ranges() {
        assert_eq!(encode_counter(42, "test").unwrap(), 42);
        assert_eq!(decode_counter(42, "test").unwrap(), 42);
        assert!(encode_counter(i64::MAX as u64 + 1, "test").is_err());
        assert!(decode_counter(-1, "test").is_err());
        assert_eq!(decode_optional_u64(Some(7), "test").unwrap(), Some(7));
        assert_eq!(decode_optional_u64(None, "test").unwrap(), None);
        assert!(decode_optional_u64(Some(-1), "test").is_err());
        assert_eq!(encode_optional_counter(Some(7), "test").unwrap(), Some(7));
        assert_eq!(encode_optional_counter(None, "test").unwrap(), None);

        let timestamp = Timestamp::from_unix_millis(1_700_000_000_123);
        let encoded = encode_timestamp(timestamp).unwrap();
        assert_eq!(decode_timestamp(encoded).unwrap(), timestamp);
    }

    #[test]
    fn uuid_and_json_decoding_reject_bad_input() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(decode_uuid(id.as_bytes(), "id").unwrap(), id);
        assert!(decode_uuid(&[0u8; 4], "id").is_err());

        let decoded: serde_json::Value = decode_json("{\"a\":1}", "payload").unwrap();
        assert_eq!(decoded["a"], 1);
        assert!(decode_json::<serde_json::Value>("not json", "payload").is_err());
    }
}
