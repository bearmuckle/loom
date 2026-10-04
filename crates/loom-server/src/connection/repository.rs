use super::*;

impl InProcessConnection {
    pub(crate) fn search_github_repositories(&self, query: &str) -> Result<ServerResponse> {
        let token = self.backend.providers.github_account_token()?;
        let repositories = search_github_repositories(&token, query)?;
        Ok(ServerResponse::Repository(
            RepositoryResponse::GitHubRepositories { repositories },
        ))
    }

    pub(crate) fn list_cloned_repositories(&self) -> Result<ServerResponse> {
        let repositories = self.backend.cached_repositories()?;
        Ok(ServerResponse::Repository(
            RepositoryResponse::ClonedRepositories { repositories },
        ))
    }

    pub(crate) fn attach_session_repository(
        &self,
        session_id: AgentSessionId,
        source: String,
        relative_path: String,
        revision: Option<String>,
        reuse_local: bool,
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
        let cached_mirror = if reuse_local {
            self.backend.cached_repository_mirror(&source)?
        } else {
            None
        };
        if let Some(mirror) = &cached_mirror {
            log::info!(
                "[loom-server] reusing cached clone of {source_name} for session {session_id} from {}",
                mirror.display()
            );
        } else {
            log::info!(
                "[loom-server] cloning repository {source_name} for session {session_id} into {}",
                destination.display()
            );
        }
        let cloned = if let Some(mirror) = &cached_mirror {
            GitService::clone_from_local(mirror, &temporary, revision.as_deref(), &source)
        } else {
            GitService::clone_from_authenticated(
                &source,
                &temporary,
                revision.as_deref(),
                github_token.as_deref(),
            )
        };
        let cloned = match cloned {
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
        if let Err(error) = self.attribute_attached_checkout(&service, &source) {
            log::warn!(
                "[loom-server] could not configure the commit identity for {source_name}: {}",
                error.message
            );
        }
        let status = service.status()?;
        self.cache_clone_for_reuse(&source, &destination, status.branch.as_deref());
        let repository = SessionRepository {
            id: repository_id,
            source: source_name.clone(),
            path: relative_path,
            revision: status.head,
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

    /// Attributes commits created in a newly attached checkout to the user.
    ///
    /// A cloned repository carries no local `user.name`/`user.email`, so Git
    /// falls back to whatever the worker host provides. That is how an agent
    /// commit ends up authored by a generic account (observed as `Agent`) that
    /// deployment providers such as Vercel cannot match to a GitHub user.
    ///
    /// Local imports already know the identity the contributor uses, so copy
    /// it; a hosted clone instead uses the authenticated GitHub account's
    /// verified noreply address.
    fn attribute_attached_checkout(&self, service: &GitService, source: &str) -> Result<()> {
        let source_identity = match GitService::open(source) {
            Ok(repository) => repository.configured_identity()?,
            Err(_) => None,
        };
        let source_is_github = github_repository_full_name(source).is_some();
        let account_identity = if source_is_github {
            match self.backend.providers.github_account_token() {
                Ok(token) => match github_account_identity(&token) {
                    Ok(identity) => Some(identity),
                    Err(error) => {
                        log::warn!(
                            "[loom-server] could not resolve the GitHub account identity: {}",
                            error.message
                        );
                        None
                    }
                },
                Err(_) => None,
            }
        } else {
            None
        };
        if let Some((name, email)) =
            resolve_attached_identity(source_identity, source_is_github, account_identity)
        {
            service.set_identity(&name, &email)?;
        }
        Ok(())
    }

    /// Caches a freshly cloned GitHub checkout as a node mirror so later
    /// sessions can start from an existing clone instead of the network.
    fn cache_clone_for_reuse(&self, source: &str, checkout: &Path, branch: Option<&str>) {
        let Some(full_name) = github_repository_full_name(source) else {
            return;
        };
        let Ok(mirror_path) = self.backend.repository_mirror_path(source) else {
            return;
        };
        if let Err(error) = GitService::create_mirror(checkout, &mirror_path, source) {
            log::warn!(
                "[loom-server] could not cache a clone mirror for {full_name}: {}",
                error.message
            );
            return;
        }
        let repository = ClonedRepository {
            full_name,
            clone_url: source.to_owned(),
            branch: branch.map(str::to_owned),
            last_used_at: Timestamp::now(),
        };
        if let Err(error) = self.backend.register_cloned_repository(&repository) {
            log::warn!(
                "[loom-server] could not record the cached clone for {source}: {}",
                error.message
            );
        }
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
