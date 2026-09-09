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
            destructive: PolicyDecision::Deny,
        }
    }
}

impl ApprovalPolicy {
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
    fn default_policy_is_safe_but_keeps_reading_automatic() {
        let policy = ApprovalPolicy::default();
        assert_eq!(policy.decision(ActionKind::Read), PolicyDecision::Allow);
        assert_eq!(
            policy.decision(ActionKind::Write),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            policy.decision(ActionKind::Destructive),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluations_explain_the_policy_decision() {
        let policy = ApprovalPolicy::default();
        let evaluation = PolicyEvaluation::evaluate(&policy, ActionKind::Command, "cargo test");
        assert_eq!(evaluation.decision, PolicyDecision::RequireApproval);
        assert!(evaluation.reason.contains("approval"));
    }
}
