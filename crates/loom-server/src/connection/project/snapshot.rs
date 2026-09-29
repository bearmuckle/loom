use super::*;

impl InProcessConnection {
    pub(crate) fn send_project_agent_message(
        &self,
        _request_id: RequestId,
        _draft: loom_core::AgentMessageDraft,
    ) -> Result<ServerResponse> {
        Err(LoomError::new(
            ErrorCode::AuthorizationDenied,
            "agent messages may only be sent by a server-bound agent run",
            false,
        ))
    }

    pub(crate) fn list_project_agent_messages(
        &self,
        project_id: ProjectId,
        session_id: AgentSessionId,
        after_sequence: u64,
        limit: u32,
    ) -> Result<ServerResponse> {
        if !(1..=512).contains(&limit) {
            return Err(LoomError::invalid_request(
                "agent message page size must be between 1 and 512",
            ));
        }
        let project = self.load_project_snapshot(project_id)?;
        if !project
            .agents
            .iter()
            .any(|agent| agent.session_id == session_id)
        {
            return Err(LoomError::invalid_request(
                "session is not a project member",
            ));
        }
        if let Some(auth) = &self.auth
            && !auth.scope().allows_session(session_id)
        {
            return Err(unauthorized_session(session_id));
        }
        if let Some(auth) = &self.auth
            && !auth
                .scope()
                .allows_workspace(self.backend.sessions()?.get(session_id)?.workspace_id)
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "token is not authorized for the project member's workspace",
                false,
            ));
        }
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "agent messaging requires durable storage",
                false,
            )
        })?;
        let messages = persistence.list_agent_messages(
            project_id,
            session_id,
            after_sequence,
            limit as usize,
        )?;
        let next_after_project_sequence = (messages.len() == limit as usize)
            .then(|| messages.last().map(|message| message.project_sequence))
            .flatten();
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectAgentMessages {
                messages,
                next_after_project_sequence,
            },
        ))
    }

    pub(crate) fn load_project_snapshot(&self, project_id: ProjectId) -> Result<ProjectSnapshot> {
        if let Some(snapshot) = self
            .backend
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_project_snapshot(project_id))
            .transpose()?
            .flatten()
        {
            return Ok(snapshot);
        }

        // The root session ID is also the project ID. This fallback keeps
        // ephemeral backends and newly-created standalone sessions addressable
        // before any hierarchy rows exist in durable storage.
        let root_session_id = AgentSessionId::from_uuid(*project_id.as_uuid());
        let root = self
            .backend
            .sessions()?
            .get(root_session_id)
            .map_err(|_| LoomError::not_found("project", project_id))?;
        Ok(ProjectSnapshot {
            project_id,
            root_session_id,
            agents: vec![ProjectAgentRecord {
                session_id: root.id,
                project_id,
                parent_session_id: None,
                depth: 1,
                state: root.state,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: root.updated_at,
            }],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        })
    }

    pub(crate) fn load_project_snapshot_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<ProjectSnapshot> {
        if let Some(snapshot) = self
            .backend
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_project_snapshot_for_session(session_id))
            .transpose()?
            .flatten()
        {
            return Ok(snapshot);
        }

        let root = self
            .backend
            .sessions()?
            .get(session_id)
            .map_err(|_| LoomError::not_found("project agent", session_id))?;
        let project_id = ProjectId::from_uuid(*session_id.as_uuid());
        Ok(ProjectSnapshot {
            project_id,
            root_session_id: root.id,
            agents: vec![ProjectAgentRecord {
                session_id: root.id,
                project_id,
                parent_session_id: None,
                depth: 1,
                state: root.state,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: root.updated_at,
            }],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        })
    }

    pub(crate) fn authorize_project_snapshot(
        &self,
        auth: &AuthSession,
        snapshot: &ProjectSnapshot,
    ) -> Result<()> {
        // A project projection includes every descendant, so each member must
        // independently fit the token's session and workspace scope.
        let mut session_ids = snapshot
            .agents
            .iter()
            .map(|agent| agent.session_id)
            .collect::<BTreeSet<_>>();
        session_ids.insert(snapshot.root_session_id);
        for session_id in session_ids {
            if !auth.scope().allows_session(session_id) {
                return Err(unauthorized_session(session_id));
            }
            let session = self.backend.sessions()?.get(session_id)?;
            if !auth.scope().allows_workspace(session.workspace_id) {
                return Err(LoomError::new(
                    ErrorCode::AuthorizationDenied,
                    "token is not authorized for a project member's workspace",
                    false,
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn project_delegation_enabled_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<bool> {
        self.project_agent_permission_enabled_for_session(
            session_id,
            Capability::CreateProjectChild,
            ProjectAgentPermission::Delegation,
        )
    }

    pub(crate) fn project_capability_enabled_for_session(
        &self,
        session_id: AgentSessionId,
        capability: Capability,
    ) -> Result<bool> {
        if !self.backend.supported_capabilities.contains(capability)
            || !self.authorized_capabilities().contains(capability)
        {
            return Ok(false);
        }
        let Some(persistence) = &self.backend.persistence else {
            return Ok(false);
        };
        let Some(project) = persistence.load_project_snapshot_for_session(session_id)? else {
            return Ok(false);
        };
        if !project
            .agents
            .iter()
            .any(|agent| agent.session_id == session_id)
        {
            return Ok(false);
        }
        if let Some(auth) = &self.auth {
            let session = self.backend.sessions()?.get(session_id)?;
            if !auth.scope().allows_session(session_id)
                || !auth.scope().allows_workspace(session.workspace_id)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn project_agent_permission_enabled_for_session(
        &self,
        session_id: AgentSessionId,
        capability: Capability,
        permission: ProjectAgentPermission,
    ) -> Result<bool> {
        if !self.project_capability_enabled_for_session(session_id, capability)? {
            return Ok(false);
        }
        let Some(persistence) = &self.backend.persistence else {
            return Ok(false);
        };
        let Some(project) = persistence.load_project_snapshot_for_session(session_id)? else {
            return Ok(false);
        };
        let Some(agent) = project
            .agents
            .iter()
            .find(|agent| agent.session_id == session_id)
        else {
            return Ok(false);
        };
        if matches!(permission, ProjectAgentPermission::Delegation)
            && agent.depth >= MAX_PROJECT_AGENT_DEPTH
        {
            return Ok(false);
        }
        if matches!(permission, ProjectAgentPermission::Delegation)
            && agent.depth > 1
            && (!self
                .backend
                .supported_capabilities
                .contains(Capability::CreateNestedProjectChild)
                || !self
                    .authorized_capabilities()
                    .contains(Capability::CreateNestedProjectChild))
        {
            return Ok(false);
        }
        if project.root_session_id == session_id {
            return Ok(true);
        }
        Ok(persistence
            .load_delegated_task_for_target(session_id)?
            .is_some_and(|task| permission.is_granted(task.permissions)))
    }
}
