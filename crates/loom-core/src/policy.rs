use serde::{Deserialize, Serialize};

use crate::RunId;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Read,
    Write,
    Command,
    Network,
    Destructive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    RequireApproval,
    Deny,
}

/// Per-kind approval decisions for tool actions.
///
/// `destructive` classifies audited, hard-to-reverse actions; it is not a
/// refusal. The shipped presets allow destructive actions, and a user who wants
/// a gate sets that dimension to [`PolicyDecision::RequireApproval`], which
/// prompts per instance instead of deciding for the user. The agent still
/// enforces [`PolicyDecision::Deny`], which a deployment can choose for a hard
/// refusal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalPolicy {
    pub read: PolicyDecision,
    pub write: PolicyDecision,
    pub command: PolicyDecision,
    pub network: PolicyDecision,
    pub destructive: PolicyDecision,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            read: PolicyDecision::Allow,
            write: PolicyDecision::RequireApproval,
            command: PolicyDecision::RequireApproval,
            network: PolicyDecision::RequireApproval,
            destructive: PolicyDecision::Allow,
        }
    }
}

impl ApprovalPolicy {
    /// Allows every action kind without prompting.
    ///
    /// The destructive classification describes an audited, hard-to-reverse
    /// action rather than a refusal, so it is allowed here like any other kind.
    /// Selecting this policy is the caller's explicit decision to let such
    /// actions run unapproved; a user who wants a per-instance prompt sets the
    /// destructive dimension to [`PolicyDecision::RequireApproval`] instead.
    pub const fn auto_approve() -> Self {
        Self {
            read: PolicyDecision::Allow,
            write: PolicyDecision::Allow,
            command: PolicyDecision::Allow,
            network: PolicyDecision::Allow,
            destructive: PolicyDecision::Allow,
        }
    }

    pub const fn decision(&self, action: ActionKind) -> PolicyDecision {
        match action {
            ActionKind::Read => self.read,
            ActionKind::Write => self.write,
            ActionKind::Command => self.command,
            ActionKind::Network => self.network,
            ActionKind::Destructive => self.destructive,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PolicyEvaluation {
    pub action: ActionKind,
    pub decision: PolicyDecision,
    pub reason: String,
}

impl PolicyEvaluation {
    pub fn evaluate(policy: &ApprovalPolicy, action: ActionKind, detail: &str) -> Self {
        let decision = policy.decision(action);
        let reason = match decision {
            PolicyDecision::Allow => format!("{detail} is allowed by the workspace policy"),
            PolicyDecision::RequireApproval => {
                format!("{detail} requires explicit user approval")
            }
            PolicyDecision::Deny => format!("{detail} is denied by the workspace policy"),
        };
        Self {
            action,
            decision,
            reason,
        }
    }

    /// States the remedy in a denial whose policy was pinned to a run.
    ///
    /// A run resolves its approval policy once, when it is created, and
    /// evaluates every step against that copy, so a later session policy change
    /// only reaches the next run. A caller that reads only "denied by the
    /// workspace policy", which is already true for a policy that changed, has
    /// no way to learn that or what would change the decision, so the denial
    /// names the run and the remedy. Only [`PolicyDecision::Deny`] gains the
    /// note: an allow, or a prompt the user can still answer, has no remedy to
    /// state.
    pub fn pinned_to_run(mut self, run_id: RunId) -> Self {
        if self.decision == PolicyDecision::Deny {
            self.reason.push_str(&format!(
                ". Run {run_id} fixed its approval policy when the run started, so this decision holds \
                 for the rest of the run; start a new run, or change the session approval policy to \
                 apply it from the next run, to allow this action"
            ));
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_allows_reads_and_destructive_actions() {
        let policy = ApprovalPolicy::default();
        assert_eq!(policy.decision(ActionKind::Read), PolicyDecision::Allow);
        assert_eq!(
            policy.decision(ActionKind::Write),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            policy.decision(ActionKind::Command),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            policy.decision(ActionKind::Network),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            policy.decision(ActionKind::Destructive),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn evaluations_explain_the_policy_decision() {
        let policy = ApprovalPolicy::default();
        let evaluation = PolicyEvaluation::evaluate(&policy, ActionKind::Command, "cargo test");
        assert_eq!(evaluation.decision, PolicyDecision::RequireApproval);
        assert!(evaluation.reason.contains("approval"));
    }

    #[test]
    fn auto_approve_allows_every_action_kind() {
        let policy = ApprovalPolicy::auto_approve();
        assert_eq!(policy.decision(ActionKind::Read), PolicyDecision::Allow);
        assert_eq!(policy.decision(ActionKind::Write), PolicyDecision::Allow);
        assert_eq!(policy.decision(ActionKind::Command), PolicyDecision::Allow);
        assert_eq!(policy.decision(ActionKind::Network), PolicyDecision::Allow);
        assert_eq!(
            policy.decision(ActionKind::Destructive),
            PolicyDecision::Allow
        );
    }

    /// A deployment that wants destructive actions refused outright can still
    /// say so; the agent keeps enforcing that decision.
    #[test]
    fn a_policy_that_denies_destructive_actions_evaluates_to_deny() {
        let policy = ApprovalPolicy {
            destructive: PolicyDecision::Deny,
            ..Default::default()
        };
        let evaluation =
            PolicyEvaluation::evaluate(&policy, ActionKind::Destructive, "github_push_branch");
        assert_eq!(evaluation.decision, PolicyDecision::Deny);
        assert!(evaluation.reason.contains("denied"));
    }

    /// A denial states the remedy, not only the decision: the run fixed its own
    /// policy and only a new run (or a session policy change taken up by the
    /// next run) changes it.
    #[test]
    fn a_run_pinned_denial_states_the_pin_and_the_remedy() {
        let policy = ApprovalPolicy {
            destructive: PolicyDecision::Deny,
            ..Default::default()
        };
        let run_id = RunId::new();
        let evaluation =
            PolicyEvaluation::evaluate(&policy, ActionKind::Destructive, "github_push_branch")
                .pinned_to_run(run_id);

        assert_eq!(evaluation.decision, PolicyDecision::Deny);
        assert!(evaluation.reason.contains("denied by the workspace policy"));
        assert!(evaluation.reason.contains(&run_id.to_string()));
        assert!(
            evaluation
                .reason
                .contains("fixed its approval policy when the run started")
        );
        assert!(evaluation.reason.contains("start a new run"));
        assert!(evaluation.reason.contains("next run"));
    }

    /// An allow or an approval prompt must not gain run-pinning text: neither
    /// decision is pinned, because the user can still answer the prompt and an
    /// allow needs no remedy.
    #[test]
    fn only_a_denial_gains_the_run_pinning_note() {
        let policy = ApprovalPolicy::default();
        let run_id = RunId::new();
        let reason_for = |action: ActionKind| {
            PolicyEvaluation::evaluate(&policy, action, "a tool call")
                .pinned_to_run(run_id)
                .reason
        };

        assert_eq!(
            reason_for(ActionKind::Read),
            "a tool call is allowed by the workspace policy"
        );
        assert_eq!(
            reason_for(ActionKind::Command),
            "a tool call requires explicit user approval"
        );
    }

    /// A user who wants a gate on the destructive dimension asks for a prompt
    /// rather than a refusal.
    #[test]
    fn a_policy_can_require_approval_for_destructive_actions() {
        let policy = ApprovalPolicy {
            destructive: PolicyDecision::RequireApproval,
            ..Default::default()
        };
        let evaluation =
            PolicyEvaluation::evaluate(&policy, ActionKind::Destructive, "github_push_branch");
        assert_eq!(evaluation.decision, PolicyDecision::RequireApproval);
        assert!(evaluation.reason.contains("approval"));
    }
}
