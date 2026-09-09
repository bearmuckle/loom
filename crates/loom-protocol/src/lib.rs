use loom_agent::{AgentEvent, AgentRunSnapshot};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, Capability, CapabilitySet, EventSequence, LoomError,
    ProjectId, ProtocolVersion, RequestId, RunId, SessionEvent, SessionEventRecord, ToolCallId,
};
use loom_model::{ModelDescriptor, ModelId};
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientRequest {
    Negotiate {
        client_version: ProtocolVersion,
        capabilities: CapabilitySet,
    },
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
    GetAgentRun {
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
    ListModels,
}

impl ClientRequest {
    pub const fn required_capability(&self) -> Option<Capability> {
        match self {
            Self::Negotiate { .. } => None,
            Self::CreateAgentSession { .. } => Some(Capability::CreateAgentSession),
            Self::GetAgentSession { .. } => Some(Capability::ReadAgentSession),
            Self::GetSessionEvents { .. } => Some(Capability::SubscribeSessionEvents),
            Self::StartAgentRun { .. } => Some(Capability::StartAgentRun),
            Self::GetAgentRun { .. } => Some(Capability::ReadAgentRun),
            Self::ApproveAgentAction { .. } | Self::RejectAgentAction { .. } => {
                Some(Capability::ApproveAgentAction)
            }
            Self::InterruptAgentRun { .. } | Self::RetryAgentStep { .. } => {
                Some(Capability::ControlAgentRun)
            }
            Self::ListModels => None,
        }
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
    AgentSessionCreated(AgentSessionSnapshot),
    AgentSession(AgentSessionSnapshot),
    AgentRunStarted(AgentRunSnapshot),
    AgentRun(AgentRunSnapshot),
    SessionEvents { events: Vec<ServerEventEnvelope> },
    Models { models: Vec<ModelDescriptor> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NegotiationResult {
    pub protocol_version: ProtocolVersion,
    pub capabilities: CapabilitySet,
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
    Agent {
        event: AgentEvent,
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

pub fn unsupported_version_error(requested: ProtocolVersion) -> LoomError {
    LoomError::unsupported_protocol(format!(
        "protocol version {}.{} is not compatible with {}.{}",
        requested.major,
        requested.minor,
        CURRENT_PROTOCOL_VERSION.major,
        CURRENT_PROTOCOL_VERSION.minor
    ))
}
