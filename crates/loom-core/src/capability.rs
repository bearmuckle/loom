use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    CreateAgentSession,
    ReadAgentSession,
    ControlAgentSession,
    SubscribeSessionEvents,
    StartAgentRun,
    ReadAgentRun,
    ControlAgentRun,
    PauseAgentRun,
    ResumeAgentRun,
    ForkAgentSession,
    RetryFromCheckpoint,
    ApproveAgentAction,
    ListProviders,
    ReadProviderHealth,
    ReadUsage,
    InspectContext,
    OpenWorkspace,
    ReadWorkspace,
    WriteWorkspace,
    SubscribeWorkspaceEvents,
    OpenTerminal,
    ControlTerminal,
    ReadTask,
    StartTask,
    ControlTask,
    ConfigureApprovalPolicy,
    ManageCheckpoints,
    TakeoverWorkspace,
    ReadWorkspaceInstructions,
    ReadVcsStatus,
    ReadVcsDiff,
    ReadTaskEvidence,
    JsonProtocol,
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
