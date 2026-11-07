use super::*;

impl InProcessConnection {
    pub(crate) fn session_filesystem(&self, session_id: AgentSessionId) -> Result<Workspace> {
        self.backend.restore_session_filesystem(session_id)
    }

    /// Lists a session's mounted directories without restoring its workspace.
    ///
    /// Restoring a workspace captures a baseline snapshot that recursively walks
    /// every mounted tree. Mounts can point at arbitrarily large directories
    /// (for example a user's Downloads folder), so a cheap metadata read must not
    /// trigger that scan. The in-memory mounts are authoritative once a
    /// workspace is live; otherwise the persisted directory rows are used. A
    /// session that works in its project's shared workspace reports the
    /// project root's directories.
    pub(crate) fn session_mounted_directories(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Vec<(String, std::path::PathBuf)>> {
        if let Some(filesystem) = self.backend.session_filesystems()?.get(&session_id) {
            return filesystem.mounted_directories();
        }
        let directories = self.persisted_session_directories(session_id)?;
        if !directories.is_empty() {
            return Ok(directories);
        }
        match self.backend.shared_workspace_owner(session_id)? {
            Some(owner) => self.persisted_session_directories(owner),
            None => Ok(directories),
        }
    }

    /// Directories recorded for one session, ignoring the project it belongs to.
    fn persisted_session_directories(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Vec<(String, std::path::PathBuf)>> {
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("filesystem for session {session_id} is unavailable"),
                true,
            )
        })?;
        let record = persistence
            .load_filesystem_record(session_id)?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("filesystem for session {session_id} is unavailable"),
                    true,
                )
            })?;
        Ok(record
            .directories
            .into_iter()
            .map(|directory| (directory.path, std::path::PathBuf::from(directory.source)))
            .collect())
    }

    pub(crate) fn import_session_directory(
        &self,
        session_id: AgentSessionId,
        source: String,
        relative_path: String,
    ) -> Result<ServerResponse> {
        let source_path = Path::new(&source);
        if !source_path.is_absolute() {
            return Err(LoomError::invalid_request(
                "local import source must be an absolute path",
            ));
        }
        if GitService::open(source_path).is_ok() {
            let repository = self.attach_session_repository(
                session_id,
                source,
                relative_path.clone(),
                None,
                false,
            )?;
            return Ok(ServerResponse::Filesystem(
                FilesystemResponse::SessionDirectoryImported {
                    path: relative_path,
                    repository: Some(repository),
                },
            ));
        }
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            return Err(LoomError::invalid_state(
                "directories cannot be imported while a session is active",
            ));
        }
        let relative = checked_session_relative_path(&relative_path)?;
        let filesystem = self.session_filesystem(session_id)?;
        let destination = filesystem.root().join(&relative);
        let parent = destination
            .parent()
            .ok_or_else(|| LoomError::invalid_request("session import path must have a parent"))?;
        fs::create_dir_all(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create session import parent: {error}"),
                false,
            )
        })?;
        let root = fs::canonicalize(filesystem.root()).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve session filesystem root: {error}"),
                false,
            )
        })?;
        let parent = fs::canonicalize(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve session import parent: {error}"),
                false,
            )
        })?;
        if !parent.starts_with(&root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "session import path escapes its filesystem root",
                false,
            ));
        }
        let destination = parent.join(
            destination
                .file_name()
                .ok_or_else(|| LoomError::invalid_request("invalid session import path"))?,
        );
        copy_directory_contents(source_path, &destination)?;
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionDirectoryImported {
                path: relative_path,
                repository: None,
            },
        ))
    }

    pub(crate) fn attach_session_directory(
        &self,
        session_id: AgentSessionId,
        source: String,
        relative_path: String,
    ) -> Result<ServerResponse> {
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            return Err(LoomError::invalid_state(
                "directories cannot be attached while a session is active",
            ));
        }
        checked_session_relative_path(&relative_path)?;
        self.ensure_session_owns_its_workspace(session_id)?;
        let filesystem = self.session_filesystem(session_id)?;
        let source_path = Path::new(&source);
        if !source_path.is_absolute() {
            return Err(LoomError::invalid_request(
                "local directory source must be an absolute path",
            ));
        }
        let source_path = Workspace::canonical_root(source_path)?;
        let mut discovered = Vec::new();
        if source_path.join(".git").exists() {
            discovered.push((relative_path.clone(), source_path.clone()));
        }
        for entry in fs::read_dir(&source_path).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect local directory: {error}"),
                false,
            )
        })? {
            let entry = entry.map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not inspect local directory entry: {error}"),
                    false,
                )
            })?;
            if entry.file_type().is_ok_and(|kind| kind.is_dir())
                && entry.path().join(".git").exists()
            {
                discovered.push((
                    format!("{}/{}", relative_path, entry.file_name().to_string_lossy()),
                    entry.path(),
                ));
            }
        }
        let mut services = Vec::new();
        for (path, source_path) in discovered {
            let service = GitService::open(&source_path)?;
            services.push((path, service));
        }
        let source_path = filesystem.mount_directory(&relative_path, &source_path)?;
        self.share_directory_with_project_members(session_id, &relative_path, Some(&source_path))?;
        let mut repositories = Vec::new();
        for (path, service) in services {
            let repository = SessionRepository {
                id: RepositoryId::new(),
                source: service.root().display().to_string(),
                path,
                revision: service.status()?.head,
                attached_at: Timestamp::now(),
            };
            self.backend
                .session_vcs()?
                .insert((session_id, repository.id), service);
            self.backend
                .session_repositories()?
                .entry(session_id)
                .or_default()
                .insert(repository.id, repository.clone());
            repositories.push(repository);
        }
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionDirectoryAttached {
                directory: SessionDirectory {
                    source: source_path.display().to_string(),
                    path: relative_path,
                },
                repositories,
            },
        ))
    }

    pub(crate) fn detach_session_directory(
        &self,
        session_id: AgentSessionId,
        path: String,
    ) -> Result<()> {
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            return Err(LoomError::invalid_state(
                "directories cannot be detached while a session is active",
            ));
        }
        self.ensure_session_owns_its_workspace(session_id)?;
        let filesystem = self.session_filesystem(session_id)?;
        filesystem.unmount_directory(&path)?;
        self.share_directory_with_project_members(session_id, &path, None)?;
        let removed = self
            .backend
            .session_repositories()?
            .entry(session_id)
            .or_default()
            .iter()
            .filter(|(_, repository)| {
                repository.path == path || repository.path.starts_with(&format!("{path}/"))
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for repository_id in removed {
            self.backend
                .session_repositories()?
                .entry(session_id)
                .or_default()
                .remove(&repository_id);
            self.backend
                .session_vcs()?
                .remove(&(session_id, repository_id));
        }
        Ok(())
    }

    /// Refuses a directory attachment on a session that works in its project's
    /// shared workspace.
    ///
    /// A shared session has no filesystem of its own to mount into: the
    /// directory would become part of the project root's workspace and be
    /// registered for one member only. The project root owns those attachments.
    fn ensure_session_owns_its_workspace(&self, session_id: AgentSessionId) -> Result<()> {
        match self.backend.shared_workspace_owner(session_id)? {
            Some(owner) => Err(LoomError::invalid_request(format!(
                "session {session_id} works in the shared workspace of its project root {owner}; attach or detach directories on that session"
            ))),
            None => Ok(()),
        }
    }

    /// Applies a directory attachment or detachment to the live sessions that
    /// share the changed workspace.
    ///
    /// The project root owns the mount, so its members follow it immediately
    /// instead of keeping a stale view until they are opened again. Each
    /// follower persists its own directory rows on the next state save.
    fn share_directory_with_project_members(
        &self,
        owner_session_id: AgentSessionId,
        relative: &str,
        source: Option<&Path>,
    ) -> Result<()> {
        let Some(owner_root) = self
            .backend
            .session_filesystems()?
            .get(&owner_session_id)
            .map(|filesystem| filesystem.root().to_path_buf())
        else {
            return Ok(());
        };
        let sharers = self
            .backend
            .session_filesystems()?
            .iter()
            .filter(|(session_id, filesystem)| {
                **session_id != owner_session_id && filesystem.root() == owner_root.as_path()
            })
            .map(|(session_id, filesystem)| (*session_id, filesystem.clone()))
            .collect::<Vec<_>>();
        for (session_id, filesystem) in sharers {
            let followed = match source {
                Some(source) => filesystem.mount_directory(relative, source).map(|_| ()),
                None => match filesystem.forget_mount(relative) {
                    Err(error) if error.code == ErrorCode::NotFound => Ok(()),
                    other => other,
                },
            };
            match followed {
                Ok(()) => log::debug!(
                    "[loom-server] session {session_id} followed directory '{relative}' of its shared workspace"
                ),
                // A follower that cannot follow is still correct on its next
                // open, which reads the project root's directories again.
                Err(error) => log::warn!(
                    "[loom-server] session {session_id} could not follow directory '{relative}' of its shared workspace: {}",
                    error.message
                ),
            }
        }
        Ok(())
    }
}
