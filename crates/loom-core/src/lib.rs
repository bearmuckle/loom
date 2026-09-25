mod capability;
mod error;
mod id;
mod limits;
mod policy;
mod session;
mod time;
mod version;
mod workspace;

pub use capability::{Capability, CapabilitySet};
pub use error::{ErrorCode, LoomError, Result};
pub use id::{
    ActivityId, AgentSessionId, CheckpointId, EventSequence, ProjectId, RepositoryId, RequestId,
    RunId, StepId, TaskId, TerminalId, ToolCallId, WorkspaceId,
};
pub use limits::{LimitKind, LimitStatus, SessionLimits, UsageSnapshot};
pub use policy::{ActionKind, ApprovalPolicy, PolicyDecision, PolicyEvaluation};
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
