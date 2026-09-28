mod capability;
mod error;
mod id;
mod limits;
mod policy;
mod project;
mod session;
mod time;
mod version;
mod workspace;

pub use capability::{Capability, CapabilitySet};
pub use error::{ErrorCode, LoomError, Result};
pub use id::{
    ActivityId, AgentMessageId, AgentSessionId, CheckpointId, EventSequence, InteractionId,
    ProjectId, RepositoryId, RequestId, RunAttemptId, RunId, StepId, TaskId, TerminalId,
    ToolCallId, WorkspaceId,
};
pub use limits::{LimitKind, LimitStatus, SessionLimits, UsageSnapshot};
pub use policy::{ActionKind, ApprovalPolicy, PolicyDecision, PolicyEvaluation};
pub use project::{
    AgentMessageDraft, AgentMessageKind, AgentMessageRecord, DelegatedTaskRecord,
    DelegatedTaskSpec, DelegatedTaskStatus, MAX_PROJECT_AGENT_DEPTH, ProjectAgentPermissions,
    ProjectAgentRecord, ProjectSnapshot, ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord,
    ProjectWorktreeStatus, TaskContextReference,
};
pub use session::{AgentSessionSnapshot, AgentSessionState, SessionEvent, SessionEventRecord};
pub use time::Timestamp;
pub use version::ProtocolVersion;
pub use workspace::WorkspaceRecord;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceLink {
    pub label: String,
    pub uri: String,
}
