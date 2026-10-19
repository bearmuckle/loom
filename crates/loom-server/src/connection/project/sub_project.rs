use super::*;

/// Longest sub project name a client may request, in bytes.
const MAX_SUB_PROJECT_NAME_BYTES: usize = 256;

impl InProcessConnection {
    /// Creates a project branched out of `parent_session_id`.
    ///
    /// The new project is a project of its own: it owns a root session, the
    /// agents it delegates to, and its checkouts. The only durable trace of the
    /// branch is the link back to the project it came from, which lets a client
    /// group it under that project. The session row, its project root row, and
    /// the link are written in one transaction, so a refused request leaves no
    /// half-created project behind.
    ///
    /// Sources and the model are not copied here: a client inherits them by
    /// attaching the parent's sources to the new session, which keeps one
    /// attach path for projects that were branched out and projects that were
    /// created on their own.
    pub(crate) fn create_sub_project(
        &self,
        parent_session_id: AgentSessionId,
        name: String,
    ) -> Result<ServerResponse> {
        let name = name.trim().to_owned();
        if name.is_empty() || name.len() > MAX_SUB_PROJECT_NAME_BYTES {
            return Err(LoomError::invalid_request(format!(
                "sub project name must contain 1 to {MAX_SUB_PROJECT_NAME_BYTES} bytes"
            )));
        }
        let Some(persistence) = &self.backend.persistence else {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                "sub projects require durable storage",
                false,
            ));
        };
        let parent = self.backend.sessions()?.get(parent_session_id)?;
        // A sub project branches out of a project, so its parent is a project
        // root. A delegated agent is managed by the project that created it and
        // cannot own one of its own.
        let parent_project = self.load_project_snapshot_for_session(parent_session_id)?;
        if parent_project.root_session_id != parent_session_id {
            return Err(LoomError::invalid_request(
                "a sub project branches out of a project root, not of one of its agents",
            ));
        }
        let parent_project_id = parent_project.project_id;
        let session_id = AgentSessionId::new();
        let filesystem = self
            .backend
            .create_session_filesystem(parent.workspace_id, session_id)?;
        let (snapshot, record) = self.backend.sessions()?.create_in_workspace_with_id(
            parent.workspace_id,
            session_id,
            name,
        )?;
        let next_sequence = self.backend.sessions()?.next_sequence().next();
        // The journal event is appended only after the transaction commits, so
        // a refused request never publishes a session that does not exist.
        if let Err(error) =
            persistence.create_sub_project(&snapshot, parent_project_id, next_sequence)
        {
            let _ = self.backend.sessions()?.remove(session_id);
            let _ = fs::remove_dir_all(filesystem.root());
            return Err(error);
        }
        self.backend.journal()?.append_session(record);
        self.backend
            .session_filesystems()?
            .insert(session_id, filesystem);
        self.backend
            .session_repositories()?
            .insert(session_id, BTreeMap::new());
        self.grant_creator_ownership(session_id);
        log::info!("[loom-server] created sub project {session_id} of project {parent_project_id}");
        Ok(ServerResponse::Project(
            ProjectResponse::SubProjectCreated {
                project_id: ProjectId::from_uuid(*snapshot.id.as_uuid()),
                session: Box::new(snapshot),
            },
        ))
    }
}
