use super::*;

impl InProcessConnection {
    pub(super) fn workspace_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace { name }) => {
                Ok(ServerResponse::Workspace(
                    WorkspaceResponse::WorkspaceCreated(self.create_workspace(name)?),
                ))
            }
            ClientRequest::Workspace(WorkspaceRequest::RegisterWorkspace { workspace }) => {
                Ok(ServerResponse::Workspace(
                    WorkspaceResponse::WorkspaceCreated(self.register_workspace(workspace)?),
                ))
            }
            ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces) => {
                let workspaces = self
                    .backend
                    .workspace_records()?
                    .list()
                    .into_iter()
                    .filter(|workspace| {
                        self.auth
                            .as_ref()
                            .is_none_or(|auth| auth.scope().allows_workspace(workspace.id))
                    })
                    .collect();
                Ok(ServerResponse::Workspace(WorkspaceResponse::Workspaces {
                    workspaces,
                }))
            }
            ClientRequest::Workspace(WorkspaceRequest::RenameWorkspace { workspace_id, name }) => {
                Ok(ServerResponse::Workspace(
                    WorkspaceResponse::WorkspaceRenamed(self.rename_workspace(workspace_id, name)?),
                ))
            }
            ClientRequest::Workspace(WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived,
            }) => Ok(ServerResponse::Session(SessionResponse::AgentSessions {
                sessions: self
                    .backend
                    .sessions()?
                    .list_in_workspace(Some(workspace_id), include_archived)
                    .into_iter()
                    .filter(|session| {
                        self.auth
                            .as_ref()
                            .is_none_or(|auth| auth.scope().allows_session(session.id))
                    })
                    .collect(),
            })),
            ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name,
            }) => Ok(ServerResponse::Session(
                SessionResponse::AgentSessionCreated(
                    self.create_session_in_workspace(workspace_id, name)?,
                ),
            )),
            ClientRequest::Workspace(WorkspaceRequest::GetWorkspaceConfigForWorkspace {
                workspace_id,
            }) => Ok(ServerResponse::Workspace(
                WorkspaceResponse::WorkspaceConfig(
                    self.backend
                        .workspace_configs()?
                        .get(&workspace_id)
                        .cloned()
                        .unwrap_or_default(),
                ),
            )),
            ClientRequest::Workspace(WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config,
            }) => {
                self.backend.set_workspace_config(workspace_id, config)?;
                Ok(ServerResponse::Workspace(
                    WorkspaceResponse::WorkspaceConfigUpdated,
                ))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
