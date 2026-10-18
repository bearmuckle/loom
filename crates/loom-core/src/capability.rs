use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    CreateAgentSession,
    ReadAgentSession,
    ControlAgentSession,
    DeleteAgentSession,
    /// Allows a client to change the archive-retention policy this worker
    /// applies. Reading the policy stays on [`Capability::ReadWorkerNodeStatus`].
    ConfigureArchiveRetention,
    SubscribeSessionEvents,
    SubscribeWorkspaceEvents,
    StartAgentRun,
    ReadAgentRun,
    ReadAgentRunMessages,
    ControlAgentRun,
    PauseAgentRun,
    ResumeAgentRun,
    ForkAgentSession,
    RetryFromCheckpoint,
    ApproveAgentAction,
    ListProviders,
    ConfigureProviders,
    ReadProviderHealth,
    ReadUsage,
    InspectContext,
    ReadWorkspaceConfig,
    OpenSessionTerminal,
    ControlSessionTerminal,
    ReadSessionTask,
    StartSessionTask,
    ControlSessionTask,
    ConfigureApprovalPolicy,
    ManageCheckpoints,
    ReadVcsStatus,
    ReadVcsDiff,
    ReadSessionTaskEvidence,
    ReadWorkerNodeStatus,
    JsonProtocol,
    ManageWorkspaces,
    ManageSessionRepositories,
    BrowseGitHubRepositories,
    ReadSessionFilesystem,
    WriteSessionFilesystem,
    ReadProject,
    /// Allows a project run to receive the server-bound delegation tool when
    /// its persisted run grant also permits delegation.
    CreateProjectChild,
    /// Enables depth-two managers to create depth-three agents after rollout validation.
    CreateNestedProjectChild,
    CreateProjectWorktree,
    ControlProjectChild,
    SendProjectAgentMessage,
    SendProjectBranchMessage,
    ReadProjectAgentMessages,
    ReadProjectChildReview,
    IntegrateProjectChild,
    CleanupProjectChildWorktree,
    /// Allows reading a session roster: who has access, their role, and their
    /// aggregated presence.
    ReadSessionRoster,
    /// Allows granting, promoting, demoting, and revoking session access. The
    /// backend additionally requires the caller to own the session.
    ManageSessionAccess,
    /// Allows sending peer chat to the people in a session. The backend
    /// additionally requires the participant role.
    SendPeerMessage,
    /// Allows a person to request access to a session, or a higher role in
    /// it, and to withdraw that request. Needs only a bound identity.
    RequestSessionAccess,
    /// Allows a connection to start a GitHub sign-in and poll its status. A
    /// connection accepted without a credential may negotiate this capability
    /// before it is bound to a person; see the pre-login state described in
    /// `docs/protocol.md`.
    GitHubSignIn,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    pub fn contains(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    pub fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0).copied().collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.0.iter()
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self::new(iter)
    }
}

#[cfg(test)]
mod tests {
    use super::{Capability, CapabilitySet};

    #[test]
    fn capability_sets_deduplicate_and_intersect_in_order() {
        let first = CapabilitySet::new([
            Capability::ReadAgentSession,
            Capability::ReadVcsStatus,
            Capability::ReadAgentSession,
        ]);
        let second = [Capability::ReadVcsStatus, Capability::StartAgentRun]
            .into_iter()
            .collect::<CapabilitySet>();

        assert!(first.contains(Capability::ReadAgentSession));
        assert!(!first.contains(Capability::StartAgentRun));
        assert_eq!(
            first
                .intersection(&second)
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![Capability::ReadVcsStatus]
        );
        assert!(CapabilitySet::default().is_empty());
        assert!(!first.is_empty());
        assert_eq!(
            serde_json::from_str::<CapabilitySet>("[\"read_agent_session\"]")
                .unwrap()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![Capability::ReadAgentSession]
        );
    }

    #[test]
    fn github_sign_in_is_a_negotiable_capability() {
        let set = CapabilitySet::new([Capability::GitHubSignIn]);
        assert!(set.contains(Capability::GitHubSignIn));
        assert_eq!(
            serde_json::to_string(&Capability::GitHubSignIn).unwrap(),
            "\"git_hub_sign_in\""
        );
        assert_eq!(
            serde_json::from_str::<CapabilitySet>("[\"git_hub_sign_in\"]").unwrap(),
            set
        );
    }
}
