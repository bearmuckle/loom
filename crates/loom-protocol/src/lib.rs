use loom_agent::{AgentEvent, AgentRunSnapshot};
use loom_context::ContextAssemblyOptions;
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, Capability, CapabilitySet, EventSequence, LoomError,
    ProjectId, ProtocolVersion, RequestId, RunId, SessionEvent, SessionEventRecord, SessionLimits,
    ToolCallId, UsageSnapshot,
};
use loom_model::{ModelDescriptor, ModelId, ProviderId};
use loom_process::{
    TaskEventRecord, TaskSnapshot, TaskSpec, TerminalEventRecord, TerminalSnapshot,
};
use loom_providers::{ProviderHealth, ProviderSummary, ProviderUsageSummary};
use loom_workspace::{
    Checkpoint, RevertResult, UndoResult, WorkspaceChange, WorkspaceControl, WorkspaceEdit,
    WorkspaceEditResult, WorkspaceFile, WorkspaceSnapshot,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestEnvelope {
    pub protocol_version: ProtocolVersion,
    pub request_id: RequestId,
    pub request: ClientRequest,
}

impl RequestEnvelope {
    pub fn new(request: ClientRequest) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id: RequestId::new(),
            request,
        }
    }

    pub fn with_version(protocol_version: ProtocolVersion, request: ClientRequest) -> Self {
        Self {
            protocol_version,
            request_id: RequestId::new(),
            request,
        }
    }

    pub fn with_request_id(request_id: RequestId, request: ClientRequest) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id,
            request,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientRequest {
    Negotiate {
        client_version: ProtocolVersion,
        capabilities: CapabilitySet,
    },
    DiscoverCapabilities,
    CreateAgentSession {
        project_id: ProjectId,
        name: String,
    },
    GetAgentSession {
        session_id: AgentSessionId,
    },
    GetSessionEvents {
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    },
    StartAgentRun {
        session_id: AgentSessionId,
        task: String,
        model: ModelId,
        workspace_root: String,
        system_instructions: Option<String>,
        repository_instructions: Option<String>,
    },
    StartAgentRunWithOptions {
        session_id: AgentSessionId,
        task: String,
        model: ModelId,
        workspace_root: String,
        system_instructions: Option<String>,
        repository_instructions: Option<String>,
        limits: SessionLimits,
        context: ContextAssemblyOptions,
    },
    GetAgentRun {
        run_id: RunId,
    },
    GetRunCheckpoint {
        run_id: RunId,
    },
    ApproveAgentAction {
        run_id: RunId,
        tool_call_id: ToolCallId,
    },
    RejectAgentAction {
        run_id: RunId,
        tool_call_id: ToolCallId,
        reason: Option<String>,
    },
    InterruptAgentRun {
        run_id: RunId,
    },
    RetryAgentStep {
        run_id: RunId,
    },
    PauseAgentRun {
        run_id: RunId,
    },
    ResumeAgentRun {
        run_id: RunId,
    },
    RetryAgentFromCheckpoint {
        run_id: RunId,
        checkpoint_id: loom_core::CheckpointId,
    },
    ForkAgentSession {
        session_id: AgentSessionId,
        name: String,
    },
    ListModels,
    ListProviders,
    DiscoverProviderModels {
        provider_id: ProviderId,
    },
    GetProviderHealth {
        provider_id: ProviderId,
    },
    GetRunUsage {
        run_id: RunId,
    },
    GetSessionUsage {
        session_id: AgentSessionId,
    },
    InspectAgentContext {
        run_id: RunId,
    },
    OpenWorkspace {
        project_id: ProjectId,
        root: String,
    },
    GetWorkspaceSnapshot {
        project_id: ProjectId,
    },
    GetWorkspaceEvents {
        project_id: ProjectId,
        after_sequence: Option<EventSequence>,
    },
    ReadWorkspaceFile {
        project_id: ProjectId,
        path: String,
    },
    ApplyWorkspaceEdit {
        project_id: ProjectId,
        edit: WorkspaceEdit,
    },
    TakeWorkspaceControl {
        project_id: ProjectId,
        control: WorkspaceControl,
    },
    CreateCheckpoint {
        project_id: ProjectId,
        session_id: Option<AgentSessionId>,
        label: String,
    },
    RevertCheckpoint {
        project_id: ProjectId,
        checkpoint_id: loom_core::CheckpointId,
    },
    UndoWorkspaceEdit {
        project_id: ProjectId,
    },
    SetApprovalPolicy {
        project_id: ProjectId,
        policy: loom_core::ApprovalPolicy,
    },
    OpenTerminal {
        project_id: ProjectId,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
    },
    WriteTerminalInput {
        project_id: ProjectId,
        terminal_id: loom_core::TerminalId,
        input: String,
    },
    ResizeTerminal {
        project_id: ProjectId,
        terminal_id: loom_core::TerminalId,
        rows: u16,
        columns: u16,
    },
    GetTerminalEvents {
        project_id: ProjectId,
        terminal_id: loom_core::TerminalId,
        after_sequence: Option<EventSequence>,
    },
    CancelTerminal {
        project_id: ProjectId,
        terminal_id: loom_core::TerminalId,
    },
    StartTask {
        project_id: ProjectId,
        spec: TaskSpec,
    },
    GetTask {
        project_id: ProjectId,
        task_id: loom_core::TaskId,
    },
    GetTaskEvents {
        project_id: ProjectId,
        task_id: loom_core::TaskId,
        after_sequence: Option<EventSequence>,
    },
    CancelTask {
        project_id: ProjectId,
        task_id: loom_core::TaskId,
    },
}

impl ClientRequest {
    pub const fn required_capability(&self) -> Option<Capability> {
        match self {
            Self::Negotiate { .. } | Self::DiscoverCapabilities => None,
            Self::CreateAgentSession { .. } => Some(Capability::CreateAgentSession),
            Self::GetAgentSession { .. } => Some(Capability::ReadAgentSession),
            Self::GetSessionEvents { .. } => Some(Capability::SubscribeSessionEvents),
            Self::StartAgentRun { .. } => Some(Capability::StartAgentRun),
            Self::StartAgentRunWithOptions { .. } => Some(Capability::StartAgentRun),
            Self::GetAgentRun { .. } => Some(Capability::ReadAgentRun),
            Self::GetRunCheckpoint { .. } => Some(Capability::ReadAgentRun),
            Self::ApproveAgentAction { .. } | Self::RejectAgentAction { .. } => {
                Some(Capability::ApproveAgentAction)
            }
            Self::InterruptAgentRun { .. } | Self::RetryAgentStep { .. } => {
                Some(Capability::ControlAgentRun)
            }
            Self::PauseAgentRun { .. } => Some(Capability::PauseAgentRun),
            Self::ResumeAgentRun { .. } => Some(Capability::ResumeAgentRun),
            Self::RetryAgentFromCheckpoint { .. } => Some(Capability::RetryFromCheckpoint),
            Self::ForkAgentSession { .. } => Some(Capability::ForkAgentSession),
            Self::ListModels => None,
            Self::ListProviders => Some(Capability::ListProviders),
            Self::DiscoverProviderModels { .. } => Some(Capability::ListProviders),
            Self::GetProviderHealth { .. } => Some(Capability::ReadProviderHealth),
            Self::GetRunUsage { .. } => Some(Capability::ReadUsage),
            Self::GetSessionUsage { .. } => Some(Capability::ReadUsage),
            Self::InspectAgentContext { .. } => Some(Capability::InspectContext),
            Self::OpenWorkspace { .. } => Some(Capability::OpenWorkspace),
            Self::GetWorkspaceSnapshot { .. } | Self::ReadWorkspaceFile { .. } => {
                Some(Capability::ReadWorkspace)
            }
            Self::GetWorkspaceEvents { .. } => Some(Capability::SubscribeWorkspaceEvents),
            Self::ApplyWorkspaceEdit { .. } => Some(Capability::WriteWorkspace),
            Self::TakeWorkspaceControl { .. } => Some(Capability::TakeoverWorkspace),
            Self::CreateCheckpoint { .. }
            | Self::RevertCheckpoint { .. }
            | Self::UndoWorkspaceEdit { .. } => Some(Capability::ManageCheckpoints),
            Self::SetApprovalPolicy { .. } => Some(Capability::ConfigureApprovalPolicy),
            Self::OpenTerminal { .. } => Some(Capability::OpenTerminal),
            Self::WriteTerminalInput { .. }
            | Self::ResizeTerminal { .. }
            | Self::CancelTerminal { .. } => Some(Capability::ControlTerminal),
            Self::GetTerminalEvents { .. } => Some(Capability::ControlTerminal),
            Self::StartTask { .. } => Some(Capability::StartTask),
            Self::GetTask { .. } | Self::GetTaskEvents { .. } => Some(Capability::ReadTask),
            Self::CancelTask { .. } => Some(Capability::ControlTask),
        }
    }

    pub const fn is_retryable_mutation(&self) -> bool {
        matches!(
            self,
            Self::CreateAgentSession { .. }
                | Self::StartAgentRun { .. }
                | Self::StartAgentRunWithOptions { .. }
                | Self::ApproveAgentAction { .. }
                | Self::RejectAgentAction { .. }
                | Self::InterruptAgentRun { .. }
                | Self::RetryAgentStep { .. }
                | Self::PauseAgentRun { .. }
                | Self::ResumeAgentRun { .. }
                | Self::RetryAgentFromCheckpoint { .. }
                | Self::ForkAgentSession { .. }
                | Self::OpenWorkspace { .. }
                | Self::ApplyWorkspaceEdit { .. }
                | Self::TakeWorkspaceControl { .. }
                | Self::CreateCheckpoint { .. }
                | Self::RevertCheckpoint { .. }
                | Self::UndoWorkspaceEdit { .. }
                | Self::SetApprovalPolicy { .. }
                | Self::OpenTerminal { .. }
                | Self::WriteTerminalInput { .. }
                | Self::ResizeTerminal { .. }
                | Self::CancelTerminal { .. }
                | Self::StartTask { .. }
                | Self::CancelTask { .. }
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResponseEnvelope {
    pub protocol_version: ProtocolVersion,
    pub request_id: RequestId,
    pub result: std::result::Result<ServerResponse, LoomError>,
}

impl ResponseEnvelope {
    pub fn success(request_id: RequestId, response: ServerResponse) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id,
            result: Ok(response),
        }
    }

    pub fn failure(request_id: RequestId, error: LoomError) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id,
            result: Err(error),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerResponse {
    Negotiated(NegotiationResult),
    Capabilities(NegotiationResult),
    AgentSessionCreated(AgentSessionSnapshot),
    AgentSessionForked(AgentSessionSnapshot),
    AgentSession(AgentSessionSnapshot),
    AgentRunStarted(AgentRunSnapshot),
    AgentRun(AgentRunSnapshot),
    RunCheckpoint(loom_workspace::Checkpoint),
    SessionEvents {
        events: Vec<ServerEventEnvelope>,
    },
    SessionEventsSnapshot {
        session: AgentSessionSnapshot,
        events: Vec<ServerEventEnvelope>,
        oldest_sequence: EventSequence,
        latest_sequence: EventSequence,
    },
    Models {
        models: Vec<ModelDescriptor>,
    },
    Providers {
        providers: Vec<ProviderSummary>,
    },
    ProviderHealth(ProviderHealth),
    RunUsage {
        usage: UsageSnapshot,
        provider: ProviderUsageSummary,
    },
    SessionUsage {
        usage: UsageSnapshot,
        provider: ProviderUsageSummary,
    },
    ContextInspection(loom_context::ContextInspection),
    WorkspaceOpened(WorkspaceSnapshot),
    WorkspaceSnapshot(WorkspaceSnapshot),
    WorkspaceEvents {
        events: Vec<WorkspaceChange>,
    },
    WorkspaceFile(WorkspaceFile),
    WorkspaceEditApplied(WorkspaceEditResult),
    WorkspaceControl(WorkspaceControl),
    CheckpointCreated(Checkpoint),
    CheckpointReverted(RevertResult),
    WorkspaceUndo(UndoResult),
    ApprovalPolicy(loom_core::ApprovalPolicy),
    TerminalOpened(TerminalSnapshot),
    Terminal(TerminalSnapshot),
    TerminalEvents {
        events: Vec<TerminalEventRecord>,
    },
    TaskStarted(TaskSnapshot),
    Task(TaskSnapshot),
    TaskEvents {
        events: Vec<TaskEventRecord>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NegotiationResult {
    pub protocol_version: ProtocolVersion,
    pub capabilities: CapabilitySet,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientFrame {
    Request(Box<RequestEnvelope>),
    Cancel { request_id: RequestId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerFrame {
    Response(ResponseEnvelope),
    Event(ServerEventEnvelope),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerEventEnvelope {
    pub protocol_version: ProtocolVersion,
    pub sequence: EventSequence,
    pub session_id: AgentSessionId,
    pub event: ServerEvent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerEvent {
    AgentSessionCreated {
        snapshot: AgentSessionSnapshot,
    },
    AgentSessionStateChanged {
        previous: loom_core::AgentSessionState,
        current: loom_core::AgentSessionState,
    },
    AgentSessionForked {
        source_session_id: AgentSessionId,
        snapshot: AgentSessionSnapshot,
    },
    Agent {
        event: AgentEvent,
    },
    WorkspaceChanged {
        change: WorkspaceChange,
    },
    Terminal {
        event: TerminalEventRecord,
    },
    Task {
        event: TaskEventRecord,
    },
    ProviderHealthChanged {
        provider_id: ProviderId,
        health: ProviderHealth,
    },
}

impl From<SessionEventRecord> for ServerEventEnvelope {
    fn from(record: SessionEventRecord) -> Self {
        Self::from_session_event(record.sequence, record.session_id, record.event)
    }
}

impl ServerEventEnvelope {
    pub fn from_session_event(
        sequence: EventSequence,
        session_id: AgentSessionId,
        event: SessionEvent,
    ) -> Self {
        let event = match event {
            SessionEvent::AgentSessionCreated { snapshot } => {
                ServerEvent::AgentSessionCreated { snapshot }
            }
            SessionEvent::AgentSessionStateChanged {
                previous, current, ..
            } => ServerEvent::AgentSessionStateChanged { previous, current },
            SessionEvent::AgentSessionForked {
                source_session_id,
                snapshot,
            } => ServerEvent::AgentSessionForked {
                source_session_id,
                snapshot,
            },
        };
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id,
            event,
        }
    }

    pub fn from_agent_event(
        sequence: EventSequence,
        session_id: AgentSessionId,
        event: AgentEvent,
    ) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id,
            event: ServerEvent::Agent { event },
        }
    }
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("JSON codec error: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn encode_request(request: &RequestEnvelope) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(request)?)
}

pub fn decode_request(bytes: &[u8]) -> Result<RequestEnvelope, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_response(response: &ResponseEnvelope) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(response)?)
}

pub fn decode_response(bytes: &[u8]) -> Result<ResponseEnvelope, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_event(event: &ServerEventEnvelope) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(event)?)
}

pub fn decode_event(bytes: &[u8]) -> Result<ServerEventEnvelope, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_client_frame(frame: &ClientFrame) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(frame)?)
}

pub fn decode_client_frame(bytes: &[u8]) -> Result<ClientFrame, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_server_frame(frame: &ServerFrame) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(frame)?)
}

pub fn decode_server_frame(bytes: &[u8]) -> Result<ServerFrame, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn unsupported_version_error(requested: ProtocolVersion) -> LoomError {
    LoomError::unsupported_protocol(format!(
        "protocol version {}.{} is not compatible with {}.{}",
        requested.major,
        requested.minor,
        CURRENT_PROTOCOL_VERSION.major,
        CURRENT_PROTOCOL_VERSION.minor
    ))
}
