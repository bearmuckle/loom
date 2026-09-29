use super::*;

impl InProcessConnection {
    pub(crate) fn create_workspace(&self, name: String) -> Result<WorkspaceRecord> {
        self.backend.workspace_records()?.create(name)
    }

    pub(crate) fn register_workspace(&self, workspace: WorkspaceRecord) -> Result<WorkspaceRecord> {
        self.backend.workspace_records()?.register(workspace)
    }

    pub(crate) fn rename_workspace(
        &self,
        workspace_id: WorkspaceId,
        name: String,
    ) -> Result<WorkspaceRecord> {
        let mut records = self.backend.workspace_records()?;
        let previous = records.export_state();
        let renamed = records.rename(workspace_id, name)?;
        drop(records);
        let sequence = self.backend.journal()?.append_workspace(
            workspace_id,
            WorkspaceEvent::Renamed {
                name: renamed.name.clone(),
            },
        );
        if let Err(error) = self.backend.persist_state() {
            *self.backend.workspace_records()? = WorkspaceManager::from_state(previous)?;
            self.backend.journal()?.discard_pending_workspace(sequence);
            return Err(error);
        }
        Ok(renamed)
    }

    pub(crate) fn create_session_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        name: String,
    ) -> Result<AgentSessionSnapshot> {
        self.backend.workspace_records()?.get(workspace_id)?;
        if name.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent session name must not be empty",
            ));
        }
        let session_id = AgentSessionId::new();
        let filesystem = self
            .backend
            .create_session_filesystem(workspace_id, session_id)?;
        let (snapshot, record) =
            self.backend
                .sessions()?
                .create_in_workspace_with_id(workspace_id, session_id, name)?;
        self.backend
            .session_filesystems()?
            .insert(session_id, filesystem);
        self.backend
            .session_repositories()?
            .insert(session_id, BTreeMap::new());
        self.backend.journal()?.append_session(record);
        Ok(snapshot)
    }
}
