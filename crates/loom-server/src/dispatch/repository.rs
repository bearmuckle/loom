use super::*;

impl InProcessConnection {
    pub(super) fn repository_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::AttachSessionRepository {
                session_id,
                source,
                path,
                revision,
            } => Ok(ServerResponse::SessionRepositoryAttached(
                self.attach_session_repository(session_id, source, path, revision)?,
            )),
            ClientRequest::ListGitHubRepositories => self.list_github_repositories(),
            ClientRequest::ListSessionRepositories { session_id } => {
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
                Ok(ServerResponse::SessionRepositories { repositories })
            }
            ClientRequest::DetachSessionRepository {
                session_id,
                repository_id,
            } => {
                self.detach_session_repository(session_id, repository_id)?;
                Ok(ServerResponse::SessionRepositoryDetached)
            }
            ClientRequest::GetSessionVcsStatus {
                session_id,
                repository_id,
            } => Ok(ServerResponse::VcsStatus(
                self.session_git(session_id, repository_id)?.status()?,
            )),
            ClientRequest::GetSessionVcsDiff {
                session_id,
                repository_id,
                path,
                staged,
            } => {
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
                Ok(ServerResponse::VcsDiff(diff))
            }
            ClientRequest::GetSessionVcsBranches {
                session_id,
                repository_id,
            } => Ok(ServerResponse::VcsBranches {
                branches: self.session_git(session_id, repository_id)?.branches()?,
            }),
            ClientRequest::GetSessionVcsConflicts {
                session_id,
                repository_id,
            } => Ok(ServerResponse::VcsConflicts {
                paths: self.session_git(session_id, repository_id)?.conflicts()?,
            }),
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
