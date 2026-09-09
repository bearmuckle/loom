use std::sync::{Arc, Mutex, MutexGuard};

use loom_core::{Capability, CapabilitySet, ErrorCode, LoomError, ProtocolVersion, Result};
use loom_model::ModelDescriptor;
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, NegotiationResult, RequestEnvelope, ResponseEnvelope,
    ServerEventEnvelope, ServerResponse, unsupported_version_error,
};
use loom_session::SessionManager;

pub struct InProcessBackend {
    sessions: Mutex<SessionManager>,
    supported_capabilities: CapabilitySet,
    models: Vec<ModelDescriptor>,
}

impl InProcessBackend {
    pub fn new() -> Arc<Self> {
        Self::with_models(Vec::new())
    }

    pub fn with_models(models: Vec<ModelDescriptor>) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(SessionManager::default()),
            supported_capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::SubscribeSessionEvents,
                Capability::JsonProtocol,
            ]),
            models,
        })
    }

    pub fn connect(self: &Arc<Self>) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
        }
    }

    fn sessions(&self) -> Result<MutexGuard<'_, SessionManager>> {
        self.sessions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session manager lock was poisoned",
                true,
            )
        })
    }
}

#[derive(Clone)]
pub struct InProcessConnection {
    backend: Arc<InProcessBackend>,
    negotiated_capabilities: Arc<Mutex<Option<CapabilitySet>>>,
}

impl InProcessConnection {
    fn new_state(backend: Arc<InProcessBackend>) -> Self {
        Self {
            backend,
            negotiated_capabilities: Arc::new(Mutex::new(None)),
        }
    }

    pub fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        let request_id = request.request_id;
        if !request
            .protocol_version
            .is_compatible_with(CURRENT_PROTOCOL_VERSION)
        {
            return ResponseEnvelope::failure(
                request_id,
                unsupported_version_error(request.protocol_version),
            );
        }

        let result = match request.request {
            ClientRequest::Negotiate {
                client_version,
                capabilities,
            } => self.negotiate(client_version, capabilities),
            request => self.handle_after_negotiation(request),
        };

        match result {
            Ok(response) => ResponseEnvelope::success(request_id, response),
            Err(error) => ResponseEnvelope::failure(request_id, error),
        }
    }

    fn negotiate(
        &self,
        client_version: ProtocolVersion,
        capabilities: CapabilitySet,
    ) -> Result<ServerResponse> {
        if !client_version.is_compatible_with(CURRENT_PROTOCOL_VERSION) {
            return Err(unsupported_version_error(client_version));
        }
        let negotiated = capabilities.intersection(&self.backend.supported_capabilities);
        *self.negotiated_capabilities()? = Some(negotiated.clone());

        Ok(ServerResponse::Negotiated(NegotiationResult {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            capabilities: negotiated,
        }))
    }

    fn handle_after_negotiation(&self, request: ClientRequest) -> Result<ServerResponse> {
        let capabilities = self
            .negotiated_capabilities()?
            .clone()
            .ok_or_else(|| LoomError::invalid_request("connection must negotiate first"))?;
        if let Some(required) = request.required_capability() {
            if !capabilities.contains(required) {
                return Err(LoomError::new(
                    ErrorCode::CapabilityDenied,
                    format!("connection did not negotiate capability {required:?}"),
                    false,
                ));
            }
        }

        match request {
            ClientRequest::Negotiate { .. } => unreachable!("negotiation is handled above"),
            ClientRequest::CreateAgentSession { project_id, name } => {
                let (snapshot, _) = self.backend.sessions()?.create(project_id, name)?;
                Ok(ServerResponse::AgentSessionCreated(snapshot))
            }
            ClientRequest::GetAgentSession { session_id } => {
                let snapshot = self.backend.sessions()?.get(session_id)?;
                Ok(ServerResponse::AgentSession(snapshot))
            }
            ClientRequest::GetSessionEvents {
                session_id,
                after_sequence,
            } => {
                let events = self
                    .backend
                    .sessions()?
                    .events_since(session_id, after_sequence)
                    .cloned()
                    .map(ServerEventEnvelope::from)
                    .collect();
                Ok(ServerResponse::SessionEvents { events })
            }
            ClientRequest::ListModels => Ok(ServerResponse::Models {
                models: self.backend.models.clone(),
            }),
        }
    }

    fn negotiated_capabilities(&self) -> Result<MutexGuard<'_, Option<CapabilitySet>>> {
        self.negotiated_capabilities.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "connection state lock was poisoned",
                true,
            )
        })
    }
}

impl InProcessConnection {
    pub fn disconnected(backend: Arc<InProcessBackend>) -> Self {
        Self::new_state(backend)
    }
}

#[cfg(test)]
mod tests {
    use loom_core::{CapabilitySet, ProjectId};
    use loom_protocol::{ClientRequest, RequestEnvelope, ServerResponse};

    use super::*;

    fn negotiate(connection: &InProcessConnection) {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::SubscribeSessionEvents,
            ]),
        }));
        assert!(matches!(response.result, Ok(ServerResponse::Negotiated(_))));
    }

    #[test]
    fn creates_session_and_reads_event_stream() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection);

        let create = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id: ProjectId::new(),
            name: "In-process demo".to_owned(),
        }));
        let session_id = match create.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };

        let events = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            after_sequence: None,
        }));
        let ServerResponse::SessionEvents { events } = events.result.unwrap() else {
            panic!("unexpected response");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, session_id);
    }

    #[test]
    fn requires_negotiation_before_session_requests() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        let response =
            connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
                project_id: ProjectId::new(),
                name: "Rejected".to_owned(),
            }));

        assert_eq!(response.result.unwrap_err().code, ErrorCode::InvalidRequest);
    }
}
