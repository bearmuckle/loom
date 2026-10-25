mod capability;
mod collaboration;
mod command_env;
mod context;
mod credential;
mod duration;
mod error;
mod filesystem;
mod id;
mod limits;
mod participant;
#[cfg(not(target_family = "wasm"))]
mod paths;
mod policy;
mod process;
mod project;
mod session;
mod time;
mod title;
mod vcs;
mod version;
mod workspace;

pub use capability::{Capability, CapabilitySet};
pub use collaboration::{
    Author, CollaborationEntry, CollaborationEntryKind, MAX_COLLABORATION_TEXT_BYTES, Recipient,
    RunControlAction, RunRevision,
};
#[cfg(unix)]
pub use command_env::BASELINE_PATH_DIRECTORIES;
pub use command_env::{baseline_path, child_path};
pub use context::{
    ContextAssemblyOptions, ContextBudget, ContextInspection, ContextItem, ContextItemKind,
    ContextSummary,
};
pub use credential::{ProviderCredentialMetadata, RunCredentialPin, SessionCredentialSelection};
pub use duration::{parse_force_discard, parse_retention_window};
pub use error::{ErrorCode, LoomError, Result};
pub use filesystem::{
    Checkpoint, CheckpointFile, ClonedRepository, ContextFileKind, ContextFileReference,
    DEFAULT_RUN_ADMISSION_TIMEOUT_SECS, GITHUB_REPOSITORY_QUERY_MIN_CHARS, GitHubRepository,
    MAX_PROJECT_AGENT_CONCURRENCY, MAX_RUN_ADMISSION_TIMEOUT_SECS, MIN_PROJECT_AGENT_CONCURRENCY,
    MIN_RUN_ADMISSION_TIMEOUT_SECS, RevertResult, SessionDirectory, SessionFilesystemChange,
    SessionFilesystemFile, SessionFilesystemSnapshot, SessionRepository, UndoResult,
    WorkerNodeConfig, WorkspaceChangeKind, WorkspaceConfig, WorkspaceControl, WorkspaceEdit,
    WorkspaceEditResult, WorkspaceEntry, WorkspaceEntryKind,
};
pub use id::{
    AccessRequestId, ActivityId, AgentMessageId, AgentSessionId, CheckpointId, CredentialId,
    EventSequence, InteractionId, ParticipantId, ProjectId, ProjectManagerWaitId, RepositoryId,
    RequestId, RunAttemptId, RunId, StepId, TaskId, TerminalId, ToolCallId, WorkspaceId,
};
pub use limits::{LimitKind, LimitStatus, SessionLimits, UsageSnapshot};
pub use participant::{
    AccessRequestDecision, GitHubSignInStatus, MAX_ACCESS_REQUEST_NOTE_BYTES,
    MAX_PARTICIPANT_DISPLAY_NAME_BYTES, ParticipantIdentity, PresenceStatus, RosterEntry,
    SessionAccessRequest, SessionGrant, SessionRole, github_participant_id,
};
#[cfg(not(target_family = "wasm"))]
pub use paths::{
    bind_key, config_dir, create_private_dir, instance_dir, state_dir, state_root, token_path,
};
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
    ProjectWorktreeBaseRefresh, ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord,
    ProjectWorktreeStatus, TaskContextReference,
};
pub use session::{
    AgentSessionSnapshot, AgentSessionState, MAX_AGENT_SESSION_GOAL_BYTES, SessionEvent,
    SessionEventRecord, SessionManagerState, normalize_session_goal,
};
pub use time::Timestamp;
pub use title::{
    MAX_SESSION_TITLE_CHARS, MAX_SESSION_TITLE_WORDS, SessionTitleSource, is_placeholder_title,
    sanitize_title, title_from_goal, titles_diverge,
};
pub use vcs::{
    GitBranch, GitDiff, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitFileStatus,
    GitFileStatusKind, GitRepositoryStatus,
};
pub use version::{CURRENT_PROTOCOL_VERSION, ProtocolVersion};
pub use workspace::{WorkspaceManagerState, WorkspaceRecord};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceLink {
    pub label: String,
    pub uri: String,
}
