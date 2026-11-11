use super::*;

/// What one parked run left behind, as the pause reason is read from it.
///
/// Every field comes from state Loom already keeps: the run snapshot, the run's
/// typed execution state, its interaction log, its attempt history, and its
/// usage against the limits the run was created with. Nothing here is written
/// for the reason, so no park path can leave it stale.
pub(crate) struct ProjectChildParkEvidence {
    pub(crate) run_state: AgentRunState,
    /// What the run recorded when it failed, without the failure prefix.
    pub(crate) failure: Option<String>,
    pub(crate) usage: UsageSnapshot,
    /// The limits this run was created with, when they are still recorded.
    pub(crate) limits: Option<loom_core::SessionLimits>,
    pub(crate) pending_approval: Option<ToolCall>,
    pub(crate) pending_input: Option<String>,
    /// The question a person has to answer, from the pending user-input
    /// interaction the run opened.
    pub(crate) pending_question: Option<loom_core::InteractionId>,
    pub(crate) last_failed_call: Option<ToolCall>,
    /// How many attempts the run has been run for, which says whether a retry
    /// has already been tried.
    pub(crate) attempts: u32,
}

impl ProjectChildParkEvidence {
    /// Reads the evidence a live run holds for itself.
    pub(crate) fn from_runtime_state(state: &AgentRuntimeState) -> Self {
        Self {
            run_state: state.run.state,
            failure: state
                .run
                .summary
                .as_deref()
                .and_then(run_failure_reason)
                .map(str::to_owned),
            usage: state.usage.clone(),
            limits: Some(state.options.limits.clone()),
            pending_approval: state.pending_approval.clone(),
            pending_input: state.pending_input.clone(),
            pending_question: pending_question_id(&state.interactions),
            last_failed_call: state.last_failed_call.clone(),
            attempts: u32::try_from(state.attempts.len()).unwrap_or(u32::MAX),
        }
    }
}

/// The reason a failed run recorded, without the prefix every failure carries.
///
/// A summary that does not carry the prefix is still read as the reason, so a
/// run failed by another writer keeps its explanation.
pub(crate) fn run_failure_reason(summary: &str) -> Option<&str> {
    Some(
        summary
            .strip_prefix(loom_model::AGENT_RUN_FAILURE_SUMMARY_PREFIX)
            .unwrap_or(summary),
    )
    .filter(|reason| !reason.is_empty())
}

/// The pending user-input interaction that owns the question a run waits on.
fn pending_question_id(
    interactions: &[loom_model::AgentInteractionRecord],
) -> Option<loom_core::InteractionId> {
    interactions
        .iter()
        .rev()
        .find(|interaction| {
            interaction.kind == loom_model::AgentInteractionKind::UserInput
                && interaction.status == loom_model::AgentInteractionStatus::Pending
        })
        .map(|interaction| interaction.id)
}

/// Digest of a tool call's arguments, so two waits for the same tool are told
/// apart without the arguments being echoed to the manager.
///
/// The digest is FNV-1a over the JSON of the arguments: stable across processes
/// and restarts, and short enough to read.
pub(crate) fn project_child_input_digest(arguments: &serde_json::Value) -> String {
    let serialized = serde_json::to_string(arguments).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in serialized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Most characters of a recorded failure the reason repeats, so one long
/// provider message cannot grow the tool output without bound.
const MAX_PAUSE_REASON_DETAIL_CHARS: usize = 512;

/// Labels the failure a run recorded, so a provider outage, an empty model
/// stream, and a truncated answer read differently in the result.
///
/// The durable run summary keeps the failure message but not its error code, so
/// the label is classified from the messages the provider layer and the runtime
/// write. An unrecognised message is reported as `unclassified` rather than
/// guessed at.
fn project_child_failure_kind(failure: &str) -> String {
    const KINDS: [(&str, &str); 8] = [
        (
            " rejected the configured credential (HTTP ",
            "provider_authentication",
        ),
        (
            " is rate limited or timed out (HTTP ",
            "provider_rate_limited",
        ),
        (" is unavailable (HTTP ", "provider_unavailable"),
        (" rejected the request (HTTP ", "provider_invalid_response"),
        ("transport failed", "provider_transport"),
        ("output token limit", "output_truncated"),
        ("empty stream", "empty_model_stream"),
        ("undecodable arguments", "undecodable_tool_arguments"),
    ];
    KINDS
        .iter()
        .find(|(phrase, _)| failure.contains(phrase))
        .map(|(_, kind)| (*kind).to_owned())
        .unwrap_or_else(|| "unclassified".to_owned())
}

fn budget_exhausted(usage: &UsageSnapshot) -> ProjectChildPauseReason {
    ProjectChildPauseReason::BudgetExhausted {
        steps: usage.tool_calls,
        tokens: usage.total_tokens(),
    }
}

/// Why a stopped run is parked, or `None` when the run is not parked.
///
/// One reason is reported per run, most specific first: a pending approval or
/// question is what the run waits on, an exhausted budget is what stopped it, a
/// recorded failed call is the step it stopped at, and a failure without one is
/// the provider call that failed. A run whose state explains none of those is
/// reported as `unknown` rather than as nothing.
pub(crate) fn project_child_pause_reason(
    evidence: &ProjectChildParkEvidence,
) -> Option<ProjectChildPauseReason> {
    match evidence.run_state {
        AgentRunState::AwaitingApproval
        | AgentRunState::Paused
        | AgentRunState::NeedsInput
        | AgentRunState::Failed => {}
        AgentRunState::Planning
        | AgentRunState::Executing
        | AgentRunState::Evaluating
        | AgentRunState::Completed
        | AgentRunState::Cancelled => return None,
    }
    if let Some(call) = evidence.pending_approval.as_ref() {
        return Some(ProjectChildPauseReason::AwaitingApproval {
            tool: call.name.clone(),
            input_digest: project_child_input_digest(&call.arguments),
        });
    }
    if evidence.pending_input.is_some() || evidence.pending_question.is_some() {
        return Some(ProjectChildPauseReason::AwaitingInput {
            question_id: evidence
                .pending_question
                .map(|id| id.to_string())
                .unwrap_or_else(|| "unrecorded".to_owned()),
        });
    }
    // The budget is read from the limits the run was created with, so a stop
    // that reached one is told apart from any other failure.
    if let Some(limits) = evidence.limits.as_ref()
        && loom_core::LimitStatus::new(limits.clone(), evidence.usage.clone()).is_exceeded()
    {
        return Some(budget_exhausted(&evidence.usage));
    }
    // A limit stop also records a failure that names the budget, which covers a
    // run whose limits are no longer recorded.
    if evidence
        .failure
        .as_deref()
        .is_some_and(|failure| failure.contains(loom_model::AGENT_RUN_LIMIT_FAILURE_REASON))
    {
        return Some(budget_exhausted(&evidence.usage));
    }
    if let Some(call) = evidence.last_failed_call.as_ref() {
        return Some(ProjectChildPauseReason::StepFailed {
            tool: call.name.clone(),
            error_kind: evidence
                .failure
                .as_deref()
                .map(project_child_failure_kind)
                .unwrap_or_else(|| "unrecorded".to_owned()),
        });
    }
    if evidence.run_state == AgentRunState::Failed
        && let Some(failure) = evidence.failure.as_deref()
    {
        return Some(ProjectChildPauseReason::ProviderError {
            kind: project_child_failure_kind(failure),
            attempts: evidence.attempts,
            last_status: Some(
                failure
                    .chars()
                    .take(MAX_PAUSE_REASON_DETAIL_CHARS)
                    .collect(),
            ),
        });
    }
    Some(ProjectChildPauseReason::Unknown)
}

impl InProcessConnection {
    /// The reason one delegated child is parked, read from the state Loom
    /// already records.
    ///
    /// A live run answers from its own state; a run that is not loaded answers
    /// from its durable snapshot, execution state, interaction log, attempt
    /// history, and pinned limits. `None` means the child is not parked: its run
    /// is progressing, or it finished or was cancelled.
    pub(crate) fn project_child_pause_reason(
        &self,
        task: &loom_core::DelegatedTaskRecord,
    ) -> Result<Option<ProjectChildPauseReason>> {
        let Some(persistence) = self.backend.persistence.as_ref() else {
            return Ok(None);
        };
        let Some(summary) =
            persistence.load_latest_run_summary_for_session(task.target_session_id)?
        else {
            // A blocked task with no run has nothing a manager can act on, but
            // it is still a wait the listing must explain.
            return Ok((task.status == loom_core::DelegatedTaskStatus::Blocked)
                .then_some(ProjectChildPauseReason::Unknown));
        };
        let run_id = summary.snapshot.id;
        // The live state answers for a run this process still owns, but only
        // when it describes the same run state the durable summary does, so a
        // run that moved on between the two reads is not reported stale.
        let live = self
            .backend
            .runs()
            .ok()
            .and_then(|runs| runs.get(&run_id).cloned())
            .map(|handle| handle.state())
            .filter(|state| state.run.id == run_id && state.run.state == summary.snapshot.state);
        let evidence = match live.as_ref() {
            Some(state) => ProjectChildParkEvidence::from_runtime_state(state),
            None => {
                let execution = persistence.load_run_execution_state(run_id)?;
                let interactions = persistence.load_run_interactions(run_id)?;
                let attempts = persistence.load_run_attempts(run_id)?;
                let limits = persistence
                    .load_run_runtime_config(run_id)?
                    .map(|config| config.limits);
                ProjectChildParkEvidence {
                    run_state: summary.snapshot.state,
                    failure: summary
                        .snapshot
                        .summary
                        .as_deref()
                        .and_then(run_failure_reason)
                        .map(str::to_owned),
                    usage: summary.usage.clone(),
                    limits,
                    pending_approval: execution
                        .as_ref()
                        .and_then(|execution| execution.pending_approval.clone()),
                    pending_input: execution
                        .as_ref()
                        .and_then(|execution| execution.pending_input.clone()),
                    pending_question: pending_question_id(&interactions),
                    last_failed_call: execution
                        .as_ref()
                        .and_then(|execution| execution.last_failed_call.clone()),
                    attempts: u32::try_from(attempts.len()).unwrap_or(u32::MAX),
                }
            }
        };
        Ok(project_child_pause_reason(&evidence))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PAUSE_REASON_DETAIL_CHARS, ProjectChildParkEvidence, pending_question_id,
        project_child_failure_kind, project_child_input_digest, project_child_pause_reason,
        run_failure_reason,
    };
    use crate::ProjectChildPauseReason;
    use loom_agent::AgentRunState;
    use loom_core::{AgentSessionId, InteractionId, RunAttemptId, RunId, SessionLimits};
    use loom_core::{Timestamp, UsageSnapshot};
    use loom_model::{
        AgentInteractionKind, AgentInteractionRecord, AgentInteractionStatus, ToolCall,
    };

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: loom_core::ToolCallId::new(),
            name: name.to_owned(),
            arguments,
        }
    }

    fn evidence(
        run_state: AgentRunState,
        usage: &UsageSnapshot,
        limits: &Option<SessionLimits>,
    ) -> ProjectChildParkEvidence {
        ProjectChildParkEvidence {
            run_state,
            failure: None,
            usage: usage.clone(),
            limits: limits.clone(),
            pending_approval: None,
            pending_input: None,
            pending_question: None,
            last_failed_call: None,
            attempts: 1,
        }
    }

    #[test]
    fn a_pending_approval_is_reported_with_its_tool_and_input_digest() {
        let usage = UsageSnapshot::default();
        let limits = None;
        let waiting = call("apply_patch", serde_json::json!({"path": "README.md"}));
        let mut parked = evidence(AgentRunState::AwaitingApproval, &usage, &limits);
        parked.pending_approval = Some(waiting.clone());
        assert_eq!(
            project_child_pause_reason(&parked),
            Some(ProjectChildPauseReason::AwaitingApproval {
                tool: "apply_patch".to_owned(),
                input_digest: project_child_input_digest(&waiting.arguments),
            })
        );

        // The same input digests the same way, and different input does not
        // collide with it.
        assert_eq!(
            project_child_input_digest(&serde_json::json!({"path": "README.md"})),
            project_child_input_digest(&serde_json::json!({"path": "README.md"}))
        );
        assert_ne!(
            project_child_input_digest(&serde_json::json!({"path": "README.md"})),
            project_child_input_digest(&serde_json::json!({"path": "other.md"}))
        );

        // A paused run that still holds the approval reports the wait, so a
        // manager resuming it is told what it still waits for.
        parked.run_state = AgentRunState::Paused;
        assert!(matches!(
            project_child_pause_reason(&parked),
            Some(ProjectChildPauseReason::AwaitingApproval { .. })
        ));
    }

    #[test]
    fn a_pending_question_names_the_interaction_a_person_has_to_answer() {
        let usage = UsageSnapshot::default();
        let limits = None;
        let question = InteractionId::new();
        let prompt = "Which option should I use?";
        let mut parked = evidence(AgentRunState::NeedsInput, &usage, &limits);
        parked.pending_input = Some(prompt.to_owned());
        parked.pending_question = Some(question);
        assert_eq!(
            project_child_pause_reason(&parked),
            Some(ProjectChildPauseReason::AwaitingInput {
                question_id: question.to_string(),
            })
        );

        // A wait recorded without its interaction row still reports the wait.
        parked.pending_question = None;
        assert_eq!(
            project_child_pause_reason(&parked),
            Some(ProjectChildPauseReason::AwaitingInput {
                question_id: "unrecorded".to_owned(),
            })
        );
        assert_eq!(
            pending_question_id(&[AgentInteractionRecord {
                id: question,
                run_id: RunId::new(),
                session_id: AgentSessionId::new(),
                attempt_id: RunAttemptId::new(),
                control_revision: 1,
                kind: AgentInteractionKind::UserInput,
                status: AgentInteractionStatus::Pending,
                tool_call_id: None,
                prompt: prompt.to_owned(),
                decision: None,
                created_at: Timestamp::from_unix_millis(1),
                resolved_at: None,
            }]),
            Some(question)
        );
    }

    #[test]
    fn an_exhausted_budget_is_read_from_usage_against_the_recorded_limits() {
        let usage = UsageSnapshot {
            input_tokens: 8,
            output_tokens: 4,
            tool_calls: 3,
            ..Default::default()
        };
        let limits = Some(SessionLimits {
            max_tool_calls: Some(3),
            ..Default::default()
        });
        let mut parked = evidence(AgentRunState::Failed, &usage, &limits);
        parked.failure = Some(loom_model::AGENT_RUN_LIMIT_FAILURE_REASON.to_owned());
        assert_eq!(
            project_child_pause_reason(&parked),
            Some(ProjectChildPauseReason::BudgetExhausted {
                steps: 3,
                tokens: 12,
            })
        );

        // Without the recorded limits, the failure the limit stop wrote still
        // names the budget.
        let unrecorded_limits = None;
        let mut parked = evidence(AgentRunState::Failed, &usage, &unrecorded_limits);
        parked.failure = Some(loom_model::AGENT_RUN_LIMIT_FAILURE_REASON.to_owned());
        assert_eq!(
            project_child_pause_reason(&parked),
            Some(ProjectChildPauseReason::BudgetExhausted {
                steps: 3,
                tokens: 12,
            })
        );
    }

    #[test]
    fn a_failed_tool_step_names_the_tool_and_the_recorded_failure() {
        let usage = UsageSnapshot::default();
        let limits = None;
        let failed = call("run_command", serde_json::json!({"command": "false"}));
        let mut parked = evidence(AgentRunState::Failed, &usage, &limits);
        parked.last_failed_call = Some(failed.clone());
        parked.failure = Some("tool execution was interrupted with an unknown outcome".to_owned());
        assert_eq!(
            project_child_pause_reason(&parked),
            Some(ProjectChildPauseReason::StepFailed {
                tool: "run_command".to_owned(),
                error_kind: "unclassified".to_owned(),
            })
        );

        // Undecodable model arguments keep the call they failed on.
        parked.failure =
            Some("the model sent undecodable arguments for tool 'run_command' 3 times".to_owned());
        assert_eq!(
            project_child_pause_reason(&parked),
            Some(ProjectChildPauseReason::StepFailed {
                tool: "run_command".to_owned(),
                error_kind: "undecodable_tool_arguments".to_owned(),
            })
        );
    }

    #[test]
    fn a_failure_without_a_failed_step_reports_the_provider_error() {
        let usage = UsageSnapshot::default();
        let limits = None;
        let mut parked = evidence(AgentRunState::Failed, &usage, &limits);
        parked.attempts = 3;
        parked.failure = Some("scripted-project is unavailable (HTTP 503)".to_owned());
        assert_eq!(
            project_child_pause_reason(&parked),
            Some(ProjectChildPauseReason::ProviderError {
                kind: "provider_unavailable".to_owned(),
                attempts: 3,
                last_status: Some("scripted-project is unavailable (HTTP 503)".to_owned()),
            })
        );
        assert_eq!(
            project_child_failure_kind("openai is rate limited or timed out (HTTP 429)"),
            "provider_rate_limited"
        );
        assert_eq!(
            project_child_failure_kind("openai rejected the configured credential (HTTP 401)"),
            "provider_authentication"
        );
        assert_eq!(
            project_child_failure_kind("deepseek transport failed: timed out"),
            "provider_transport"
        );
        assert_eq!(
            project_child_failure_kind("the model kept reaching the output token limit"),
            "output_truncated"
        );
        assert_eq!(
            project_child_failure_kind("model returned an empty stream"),
            "empty_model_stream"
        );
        assert_eq!(project_child_failure_kind("something else"), "unclassified");

        // One long message is bounded instead of growing the tool output.
        let long = "x".repeat(4 * MAX_PAUSE_REASON_DETAIL_CHARS);
        parked.failure = Some(long);
        let Some(ProjectChildPauseReason::ProviderError { last_status, .. }) =
            project_child_pause_reason(&parked)
        else {
            panic!("a provider failure should be reported");
        };
        assert_eq!(
            last_status.map(|status| status.chars().count()),
            Some(MAX_PAUSE_REASON_DETAIL_CHARS)
        );
    }

    #[test]
    fn a_stopped_run_with_no_recorded_cause_reports_unknown() {
        let usage = UsageSnapshot::default();
        let limits = None;
        assert_eq!(
            project_child_pause_reason(&evidence(AgentRunState::Paused, &usage, &limits)),
            Some(ProjectChildPauseReason::Unknown)
        );
        // A failed run that recorded no reason is unknown too, rather than
        // claiming a provider error.
        assert_eq!(
            project_child_pause_reason(&evidence(AgentRunState::Failed, &usage, &limits)),
            Some(ProjectChildPauseReason::Unknown)
        );
    }

    #[test]
    fn a_run_that_is_not_parked_reports_no_reason() {
        let usage = UsageSnapshot::default();
        let limits = None;
        for run_state in [
            AgentRunState::Planning,
            AgentRunState::Executing,
            AgentRunState::Evaluating,
            AgentRunState::Completed,
            AgentRunState::Cancelled,
        ] {
            assert_eq!(
                project_child_pause_reason(&evidence(run_state, &usage, &limits)),
                None,
                "{run_state:?} is not a parked state"
            );
        }
    }

    #[test]
    fn a_failure_summary_reads_as_its_reason() {
        assert_eq!(
            run_failure_reason("Agent failed: model returned an empty stream"),
            Some("model returned an empty stream")
        );
        assert_eq!(
            run_failure_reason("a summary without the prefix"),
            Some("a summary without the prefix")
        );
        assert_eq!(run_failure_reason(""), None);
    }
}
