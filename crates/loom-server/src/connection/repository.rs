use super::*;

impl InProcessConnection {
    pub(crate) fn list_github_repositories(&self) -> Result<ServerResponse> {
        let token = self.backend.providers.github_account_token()?;
        let repositories = fetch_github_repositories(&token, "https://api.github.com/user/repos")?;
        Ok(ServerResponse::Repository(
            RepositoryResponse::GitHubRepositories { repositories },
        ))
    }

    pub(crate) fn attach_session_repository(
        &self,
        session_id: AgentSessionId,
        source: String,
        relative_path: String,
        revision: Option<String>,
    ) -> Result<SessionRepository> {
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
                "repositories cannot be attached while a session is active",
            ));
        }
        let source_name = repository_display_name(&source)?;
        let relative = checked_session_relative_path(&relative_path)?;
        let filesystem = self.session_filesystem(session_id)?;
        let root = filesystem.root();
        let destination = root.join(&relative);
        if destination.exists() {
            return Err(LoomError::conflict(format!(
                "session path '{}' already exists",
                relative.display()
            )));
        }
        let parent = destination.parent().ok_or_else(|| {
            LoomError::invalid_request("repository checkout path must have a parent")
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create repository checkout parent: {error}"),
                false,
            )
        })?;
        let canonical_root = fs::canonicalize(root).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve session filesystem root: {error}"),
                false,
            )
        })?;
        let canonical_parent = fs::canonicalize(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve repository checkout parent: {error}"),
                false,
            )
        })?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "repository checkout path escapes its session filesystem root",
                false,
            ));
        }
        let repository_id = RepositoryId::new();
        let temporary = parent.join(format!(".loom-clone-{repository_id}"));
        if temporary.exists() {
            return Err(LoomError::conflict(
                "temporary repository checkout path already exists",
            ));
        }
        let github_token = url::Url::parse(&source)
            .ok()
            .filter(|url| url.scheme() == "https" && url.host_str() == Some("github.com"))
            .and_then(|_| self.backend.providers.github_account_token().ok());
        log::info!(
            "[loom-server] cloning repository {source_name} for session {session_id} into {}",
            destination.display()
        );
        let cloned = match GitService::clone_from_authenticated(
            &source,
            &temporary,
            revision.as_deref(),
            github_token.as_deref(),
        ) {
            Ok(cloned) => cloned,
            Err(error) => {
                log::warn!(
                    "[loom-server] clone failed for repository {source_name} in session {session_id}: {}",
                    error.message
                );
                if temporary.exists() {
                    fs::remove_dir_all(&temporary).map_err(|cleanup_error| {
                        LoomError::new(
                            ErrorCode::ToolExecution,
                            format!(
                                "repository clone failed and temporary checkout cleanup failed: {cleanup_error}"
                            ),
                            false,
                        )
                    })?;
                }
                return Err(error);
            }
        };
        drop(cloned);
        log::info!(
            "[loom-server] clone completed for repository {source_name}; installing checkout"
        );
        if let Err(error) = fs::rename(&temporary, &destination) {
            let cleanup = fs::remove_dir_all(&temporary);
            if let Err(cleanup_error) = cleanup {
                return Err(LoomError::new(
                    ErrorCode::ToolExecution,
                    format!(
                        "could not install cloned repository ({error}) or clean its temporary checkout ({cleanup_error})"
                    ),
                    false,
                ));
            }
            return Err(LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not install cloned repository: {error}"),
                false,
            ));
        }
        let service = GitService::open(&destination)?;
        let repository = SessionRepository {
            id: repository_id,
            source: source_name.clone(),
            path: relative_path,
            revision: service.status()?.head,
            attached_at: Timestamp::now(),
        };
        self.backend
            .session_vcs()?
            .insert((session_id, repository_id), service);
        {
            self.backend
                .session_repositories()?
                .entry(session_id)
                .or_default()
                .insert(repository_id, repository.clone());
        }
        filesystem.mark_state_dirty()?;
        log::info!("repository {source_name} attached to session {session_id}");
        Ok(repository)
    }

    pub(crate) fn detach_session_repository(
        &self,
        session_id: AgentSessionId,
        repository_id: RepositoryId,
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
                "repositories cannot be detached while a session is active",
            ));
        }
        let filesystem = self.session_filesystem(session_id)?;
        let repository = self
            .backend
            .session_repositories()?
            .get(&session_id)
            .and_then(|repositories| repositories.get(&repository_id))
            .cloned()
            .ok_or_else(|| LoomError::not_found("session repository", repository_id))?;
        if filesystem.mounted_source_for(&repository.path)?.is_none() {
            let path = filesystem.directory_path(&repository.path)?;
            fs::remove_dir_all(&path).map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not remove detached repository checkout: {error}"),
                    false,
                )
            })?;
        }
        {
            self.backend
                .session_repositories()?
                .entry(session_id)
                .or_default()
                .remove(&repository_id);
        }
        self.backend
            .session_vcs()?
            .remove(&(session_id, repository_id));
        filesystem.mark_state_dirty()?;
        Ok(())
    }

    pub(crate) fn session_git(
        &self,
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    ) -> Result<GitService> {
        if let Some(service) = self
            .backend
            .session_vcs()?
            .get(&(session_id, repository_id))
            .cloned()
        {
            return Ok(service);
        }
        let _filesystem = self.session_filesystem(session_id)?;
        let repository = self
            .backend
            .session_repositories()?
            .get(&session_id)
            .and_then(|repositories| repositories.get(&repository_id))
            .cloned()
            .ok_or_else(|| LoomError::not_found("session repository", repository_id))?;
        let filesystem = self.session_filesystem(session_id)?;
        let path = filesystem.directory_path(&repository.path)?;
        let service = GitService::open(path)?;
        self.backend
            .session_vcs()?
            .insert((session_id, repository_id), service.clone());
        Ok(service)
    }
}
