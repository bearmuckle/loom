//! In-process tests: project misc.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn session_scoped_tokens_are_limited_to_their_sessions_and_source_roots() {
    let backend = InProcessBackend::new();
    let unrestricted = backend.connect();
    negotiate(&unrestricted);
    let workspace = match unrestricted
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Scoped access".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session_id = match unrestricted
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Authorized session".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };

    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaces
            )))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::Workspaces { .. }
        ))
    ));
    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::GetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfig(_)
        ))
    ));
    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: WorkspaceConfig::default(),
                }
            ),))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceConfigUpdated
        ))
    ));
    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::RenameWorkspace {
                    workspace_id: workspace.id,
                    name: "Renamed workspace".to_owned(),
                }
            )))
            .result,
        Ok(ServerResponse::Workspace(
            WorkspaceResponse::WorkspaceRenamed(_)
        ))
    ));
    assert!(matches!(
        unrestricted
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::ListWorkspaceSessions {
                    workspace_id: workspace.id,
                    include_archived: false,
                }
            )))
            .result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessions { .. }
        ))
    ));

    let tokens = AuthTokenStore::new();
    let issued = tokens
        .issue(AuthorizationScope::for_sessions(
            [session_id],
            backend.supported_capabilities.clone(),
        ))
        .unwrap();
    let scoped = backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap());
    negotiate(&scoped);
    assert!(matches!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::GetAgentSession { session_id }
            )))
            .result,
        Ok(ServerResponse::Session(SessionResponse::AgentSession(_)))
    ));
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::GetAgentSession {
                    session_id: AgentSessionId::new(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateWorkspace {
                    name: "Denied workspace".to_owned(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Denied session".to_owned(),
                }
            ),))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::AttachSessionDirectory {
                    session_id,
                    source: std::env::temp_dir().display().to_string(),
                    path: "sources/local".to_owned(),
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::WorkspaceAccessDenied
    );
    assert_eq!(
        scoped
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: None,
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );

    fs::remove_dir_all(&backend.session_root_base).unwrap();
}
