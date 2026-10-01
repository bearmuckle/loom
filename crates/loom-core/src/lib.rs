mod capability;
mod context;
mod error;
mod filesystem;
mod id;
mod limits;
mod policy;
mod process;
mod project;
mod session;
mod time;
mod version;
mod workspace;

pub use capability::{Capability, CapabilitySet};
pub use context::{
    ContextAssemblyOptions, ContextBudget, ContextInspection, ContextItem, ContextItemKind,
    ContextSummary,
};
pub use error::{ErrorCode, LoomError, Result};
pub use filesystem::{
    Checkpoint, CheckpointFile, ContextFileKind, ContextFileReference, GitHubRepository,
    MAX_PROJECT_AGENT_CONCURRENCY, MIN_PROJECT_AGENT_CONCURRENCY, RevertResult, SessionDirectory,
    SessionFilesystemChange, SessionFilesystemFile, SessionFilesystemSnapshot, SessionRepository,
    UndoResult, WorkerNodeConfig, WorkspaceChangeKind, WorkspaceConfig, WorkspaceControl,
    WorkspaceEdit, WorkspaceEditResult, WorkspaceEntry, WorkspaceEntryKind,
};
pub use id::{
    ActivityId, AgentMessageId, AgentSessionId, CheckpointId, EventSequence, InteractionId,
    ProjectId, ProjectManagerWaitId, RepositoryId, RequestId, RunAttemptId, RunId, StepId, TaskId,
    TerminalId, ToolCallId, WorkspaceId,
};
pub use limits::{LimitKind, LimitStatus, SessionLimits, UsageSnapshot};
pub use policy::{ActionKind, ApprovalPolicy, PolicyDecision, PolicyEvaluation};
pub use process::{
    TaskArtifact, TaskEvent, TaskEventRecord, TaskEvidenceLink, TaskKind, TaskSnapshot, TaskSpec,
    TaskStatus, TerminalEvent, TerminalEventRecord, TerminalSnapshot, TerminalStatus,
    TerminalStream,
};
pub use project::{
    AgentMessageDraft, AgentMessageKind, AgentMessageRecord, DelegatedTaskRecord,
    DelegatedTaskSpec, DelegatedTaskStatus, MAX_PROJECT_AGENT_DEPTH, ProjectAgentPermissions,
    ProjectAgentRecord, ProjectManagerWaitRecord, ProjectManagerWaitStatus, ProjectSnapshot,
    ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord, ProjectWorktreeStatus,
    TaskContextReference,
};
pub use session::{
    AgentSessionSnapshot, AgentSessionState, SessionEvent, SessionEventRecord, SessionManagerState,
};
pub use time::Timestamp;
pub use version::{CURRENT_PROTOCOL_VERSION, ProtocolVersion};
pub use workspace::{WorkspaceManagerState, WorkspaceRecord};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceLink {
    pub label: String,
    pub uri: String,
}
