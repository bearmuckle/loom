use serde::{Deserialize, Serialize};

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
