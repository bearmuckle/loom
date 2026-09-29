use super::*;

impl InProcessConnection {
    pub(super) fn workspace_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::CreateWorkspace { name } => Ok(ServerResponse::WorkspaceCreated(
                self.create_workspace(name)?,
            )),
            ClientRequest::RegisterWorkspace { workspace } => Ok(ServerResponse::WorkspaceCreated(
                self.register_workspace(workspace)?,
            )),
            ClientRequest::ListWorkspaces => {
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
                Ok(ServerResponse::Workspaces { workspaces })
            }
            ClientRequest::RenameWorkspace { workspace_id, name } => Ok(
                ServerResponse::WorkspaceRenamed(self.rename_workspace(workspace_id, name)?),
            ),
            ClientRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived,
            } => Ok(ServerResponse::AgentSessions {
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
            }),
            ClientRequest::CreateAgentSessionInWorkspace { workspace_id, name } => {
                Ok(ServerResponse::AgentSessionCreated(
                    self.create_session_in_workspace(workspace_id, name)?,
                ))
            }
            ClientRequest::GetWorkspaceConfigForWorkspace { workspace_id } => {
                Ok(ServerResponse::WorkspaceConfig(
                    self.backend
                        .workspace_configs()?
                        .get(&workspace_id)
                        .cloned()
                        .unwrap_or_default(),
                ))
            }
            ClientRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config,
            } => {
                self.backend.set_workspace_config(workspace_id, config)?;
                Ok(ServerResponse::WorkspaceConfigUpdated)
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
