use super::*;

impl InProcessConnection {
    pub(super) fn repository_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
                session_id,
                source,
                path,
                revision,
                reuse_local,
            }) => Ok(ServerResponse::Repository(
                RepositoryResponse::SessionRepositoryAttached(self.attach_session_repository(
                    session_id,
                    source,
                    path,
                    revision,
                    reuse_local,
                )?),
            )),
            ClientRequest::Repository(RepositoryRequest::SearchGitHubRepositories {
                query,
                ..
            }) => self.search_github_repositories(&query),
            ClientRequest::Repository(RepositoryRequest::ListClonedRepositories) => {
                self.list_cloned_repositories()
            }
            ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                session_id,
            }) => {
                self.backend.sessions()?.get(session_id)?;
                let repositories = self
                    .backend
                    .session_repositories()?
                    .get(&session_id)
                    .map(|repositories| repositories.values().cloned().collect());
                let repositories: Vec<SessionRepository> = match repositories {
                    Some(repositories) => repositories,
                    None => self
                        .backend
                        .persisted_filesystem_record(session_id)?
                        .map(|persisted| persisted.repositories.into_values().collect())
                        .unwrap_or_default(),
                };
                // Every listed record carries a source that can be attached
                // again, which a record persisted with a display name does not.
                let repositories = repositories
                    .into_iter()
                    .map(|mut repository| {
                        repository.source =
                            self.reported_repository_source(session_id, &repository);
                        repository
                    })
                    .collect();
                Ok(ServerResponse::Repository(
                    RepositoryResponse::SessionRepositories { repositories },
                ))
            }
            ClientRequest::Repository(RepositoryRequest::DetachSessionRepository {
                session_id,
                repository_id,
            }) => {
                self.detach_session_repository(session_id, repository_id)?;
                Ok(ServerResponse::Repository(
                    RepositoryResponse::SessionRepositoryDetached,
                ))
            }
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsStatus {
                session_id,
                repository_id,
            }) => Ok(ServerResponse::Repository(RepositoryResponse::VcsStatus(
                self.session_git(session_id, repository_id)?.status()?,
            ))),
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsDiff {
                session_id,
                repository_id,
                path,
                staged,
            }) => {
                // The render bounds itself to whole hunks, so the payload is
                // the diff the client displays as sent: its patch parses, and
                // its hunks describe the lines that patch holds.
                let diff = self.session_git(session_id, repository_id)?.diff_limited(
                    path.as_deref(),
                    staged,
                    MAX_REVIEW_DIFF_BYTES,
                )?;
                Ok(ServerResponse::Repository(RepositoryResponse::VcsDiff(
                    diff,
                )))
            }
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsBranches {
                session_id,
                repository_id,
            }) => Ok(ServerResponse::Repository(
                RepositoryResponse::VcsBranches {
                    branches: self.session_git(session_id, repository_id)?.branches()?,
                },
            )),
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsConflicts {
                session_id,
                repository_id,
            }) => Ok(ServerResponse::Repository(
                RepositoryResponse::VcsConflicts {
                    paths: self.session_git(session_id, repository_id)?.conflicts()?,
                },
            )),
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
