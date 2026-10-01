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
            ClientRequest::Repository(RepositoryRequest::SearchGitHubRepositories { query }) => {
                self.search_github_repositories(&query)
            }
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
                let repositories = match repositories {
                    Some(repositories) => repositories,
                    None => self
                        .backend
                        .persisted_filesystem_record(session_id)?
                        .map(|persisted| persisted.repositories.into_values().collect())
                        .unwrap_or_default(),
                };
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
                let mut diff = self
                    .session_git(session_id, repository_id)?
                    .diff(path.as_deref(), staged)?;
                let mut remaining = MAX_REVIEW_DIFF_BYTES;
                let mut truncated = false;
                for hunk in &mut diff.hunks {
                    if truncated {
                        hunk.lines.clear();
                        continue;
                    }
                    let keep = hunk
                        .lines
                        .iter()
                        .take_while(|line| {
                            let size = line.content.len() + 32;
                            if size > remaining {
                                truncated = true;
                                false
                            } else {
                                remaining -= size;
                                true
                            }
                        })
                        .count();
                    if keep < hunk.lines.len() {
                        truncated = true;
                        hunk.lines.truncate(keep);
                    }
                }
                diff.hunks.retain(|hunk| !hunk.lines.is_empty());
                diff.truncated = truncated;
                diff.patch = bounded_review_text(&diff.patch, MAX_REVIEW_DIFF_BYTES);
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
