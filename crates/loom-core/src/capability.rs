use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    CreateAgentSession,
    ReadAgentSession,
    ControlAgentSession,
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
    CreateProjectChild,
    SendProjectAgentMessage,
    ReadProjectAgentMessages,
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
}
