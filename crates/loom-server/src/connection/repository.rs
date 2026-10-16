use super::*;

/// Deadline for the bounded refresh that runs before a session clones from a
/// node mirror (#303). The refresh is bounded because a stalled remote must not
/// hold the attach; a remote that does not answer within this window degrades to
/// "reused, freshness unknown" instead of blocking or failing the attach.
const MIRROR_REFRESH_DEADLINE: Duration = Duration::from_secs(10);

/// A mirror whose last successful refresh is older than this is not reused at
/// all: the session clones from the network instead, so a node that has not seen
/// a repository in that long does not start a session from a nearly frozen
/// mirror. An unknown age is not stale, because the bounded refresh below
/// establishes it.
const MIRROR_REUSE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// A node mirror resolved for reuse, together with how current its contents were
/// when it was resolved.
#[derive(Debug)]
pub(crate) struct ReusableMirror {
    pub(crate) path: PathBuf,
    pub(crate) freshness: MirrorFreshness,
}

/// Whether a mirror with this refresh record may still be reused for a session.
fn mirror_may_be_reused(recorded: &CachedMirrorRefresh, now: Timestamp) -> bool {
    recorded.age_millis(now) <= MIRROR_REUSE_MAX_AGE.as_millis() as u64
}

/// Whether a mirror resolves `revision` from its own refs, which decides whether
/// a refresh that did not run may still satisfy the request.
fn mirror_contains_revision(mirror: &Path, revision: &str) -> bool {
    GitService::open(mirror).is_ok_and(|git| git.resolves_revision(revision))
}

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
            self.resolve_reusable_mirror(&source, &source_name, revision.as_deref())?
        } else {
            None
        };
        if let Some(mirror) = &cached_mirror {
            log::info!(
                "[loom-server] reusing cached clone of {source_name} for session {session_id} from {} ({})",
                mirror.path.display(),
                mirror.freshness.summary(Timestamp::now())
            );
        } else {
            log::info!(
                "[loom-server] cloning repository {source_name} for session {session_id} into {}",
                destination.display()
            );
        }
        let cloned = if let Some(mirror) = &cached_mirror {
            GitService::clone_from_local(&mirror.path, &temporary, revision.as_deref(), &source)
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
        self.cache_clone_for_reuse(
            &source,
            &destination,
            status.branch.as_deref(),
            status.head.as_deref(),
        );
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

    /// Resolves the node mirror to clone a session checkout from, refreshing it
    /// first, or reports `None` so the caller clones from the network.
    ///
    /// A checkout cloned from a mirror takes its `refs/remotes/origin/*` from the
    /// mirror's `refs/heads/*`, so a mirror that is not fetched hands every later
    /// session the tip it was created from (#303). The mirror is therefore
    /// fetched with a bounded deadline before it is used, and that refresh never
    /// fails the attach: a stalled or unreachable remote degrades to a mirror
    /// reported as `reused, freshness unknown` instead.
    ///
    /// A mirror whose recorded refresh is older than [`MIRROR_REUSE_MAX_AGE`] is
    /// not reused at all: the checkout comes from the network instead. That mirror
    /// is still refreshed, so a mirror that went unused past the limit becomes
    /// reusable again for the session after this one. A mirror whose bounded
    /// refresh failed is not reused for a request it cannot resolve.
    pub(crate) fn resolve_reusable_mirror(
        &self,
        source: &str,
        source_name: &str,
        revision: Option<&str>,
    ) -> Result<Option<ReusableMirror>> {
        let Some(mirror) = self.backend.cached_repository_mirror(source)? else {
            return Ok(None);
        };
        let now = Timestamp::now();
        let recorded = self.backend.mirror_refresh(source)?;
        if let Some(recorded) = recorded
            .as_ref()
            .filter(|recorded| !mirror_may_be_reused(recorded, now))
        {
            log::info!(
                "[loom-server] not reusing the cached clone of {source_name} at {}: last refreshed {} ms ago, past the {} ms reuse limit; cloning from the network instead",
                mirror.display(),
                recorded.age_millis(now),
                MIRROR_REUSE_MAX_AGE.as_millis()
            );
            self.refresh_cached_mirror(source, &mirror, Some(recorded.clone()));
            return Ok(None);
        }
        let freshness = self.refresh_cached_mirror(source, &mirror, recorded);
        let unsatisfied = revision.filter(|revision| {
            freshness.is_unknown() && !mirror_contains_revision(&mirror, revision)
        });
        if let Some(revision) = unsatisfied {
            log::warn!(
                "[loom-server] not reusing the cached clone of {source_name} at {}: {} and its cached tip does not contain the requested revision '{revision}'; cloning from the network instead",
                mirror.display(),
                freshness.summary(now)
            );
            return Ok(None);
        }
        Ok(Some(ReusableMirror {
            path: mirror,
            freshness,
        }))
    }

    /// Refreshes a node mirror with the deadline-bounded remote machinery
    /// ([`GitService::refresh_mirror_heads`]) and records when it finished.
    ///
    /// A failure is logged and reported as unknown freshness, never returned: the
    /// mirror is still a usable starting point, and an attach must not depend on
    /// the remote answering.
    fn refresh_cached_mirror(
        &self,
        source: &str,
        mirror: &Path,
        recorded: Option<CachedMirrorRefresh>,
    ) -> MirrorFreshness {
        let refreshed = GitService::open(mirror).and_then(|git| {
            let token = self.origin_fetch_token(&git);
            git.refresh_mirror_heads(token.as_deref(), MIRROR_REFRESH_DEADLINE)
        });
        let report = match refreshed {
            Ok(report) => report,
            Err(error) => {
                log::warn!(
                    "[loom-server] could not refresh the cached clone mirror {}: {}",
                    mirror.display(),
                    error.message
                );
                return MirrorFreshness::Unknown {
                    last_refresh: recorded,
                    warning: error.message,
                };
            }
        };
        let refresh = CachedMirrorRefresh {
            clone_url: source.to_owned(),
            refreshed_at: report.fetched_at,
            default_branch: Some(report.default_branch),
            revision: Some(report.default_branch_revision),
        };
        if let Err(error) = self.backend.record_mirror_refresh(&refresh) {
            log::warn!(
                "[loom-server] could not record the mirror refresh for {}: {}",
                mirror.display(),
                error.message
            );
        }
        MirrorFreshness::Refreshed(refresh)
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
    ///
    /// A mirror this call created is recorded as refreshed now, because it is a
    /// clone of the checkout that the remote was just cloned into. A mirror that
    /// already existed is left alone, record included: this call did not fetch it
    /// and cannot claim its heads are current (#303).
    fn cache_clone_for_reuse(
        &self,
        source: &str,
        checkout: &Path,
        branch: Option<&str>,
        revision: Option<&str>,
    ) {
        let Some(full_name) = github_repository_full_name(source) else {
            return;
        };
        let Ok(mirror_path) = self.backend.repository_mirror_path(source) else {
            return;
        };
        match GitService::create_mirror(checkout, &mirror_path, source) {
            Ok(MirrorCreation::Created) => {
                let refresh = CachedMirrorRefresh {
                    clone_url: source.to_owned(),
                    refreshed_at: Timestamp::now(),
                    default_branch: branch.map(str::to_owned),
                    revision: revision.map(str::to_owned),
                };
                if let Err(error) = self.backend.record_mirror_refresh(&refresh) {
                    log::warn!(
                        "[loom-server] could not record the mirror refresh for {full_name}: {}",
                        error.message
                    );
                }
            }
            Ok(MirrorCreation::Existing) => log::debug!(
                "[loom-server] the cached clone mirror for {full_name} already exists; its recorded freshness is unchanged"
            ),
            Err(error) => {
                log::warn!(
                    "[loom-server] could not cache a clone mirror for {full_name}: {}",
                    error.message
                );
                return;
            }
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

    /// Opens the Git service of one attached session repository.
    ///
    /// The backend owns the worktree and repository plumbing, because the
    /// retention sweep removes worktrees without a connection; this wrapper
    /// keeps the connection's call sites unchanged.
    pub(crate) fn session_git(
        &self,
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    ) -> Result<GitService> {
        self.backend.session_git(session_id, repository_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refresh(refreshed_at: Timestamp) -> CachedMirrorRefresh {
        CachedMirrorRefresh {
            clone_url: "https://github.com/owner/name.git".to_owned(),
            refreshed_at,
            default_branch: Some("main".to_owned()),
            revision: Some("a1b2c3".to_owned()),
        }
    }

    #[test]
    fn a_mirror_is_reused_until_its_recorded_refresh_is_a_day_old() {
        let now = Timestamp::from_unix_millis(100_000_000);
        assert!(mirror_may_be_reused(&refresh(now), now));
        assert!(mirror_may_be_reused(
            &refresh(Timestamp::from_unix_millis(100_000_000 - 1_000)),
            now
        ));
        // At the limit the mirror is still reused, past it the network wins.
        let limit = MIRROR_REUSE_MAX_AGE.as_millis() as u64;
        assert!(mirror_may_be_reused(
            &refresh(Timestamp::from_unix_millis(100_000_000 - limit)),
            now
        ));
        assert!(!mirror_may_be_reused(
            &refresh(Timestamp::from_unix_millis(100_000_000 - limit - 1)),
            now
        ));
        // A record written by a clock that ran ahead is not stale.
        assert!(mirror_may_be_reused(
            &refresh(Timestamp::from_unix_millis(100_000_001)),
            now
        ));
    }

    #[test]
    fn an_unreadable_mirror_contains_no_revision() {
        let missing = std::env::temp_dir().join(format!("loom-missing-{}", AgentSessionId::new()));
        assert!(!mirror_contains_revision(&missing, "main"));
    }
}
