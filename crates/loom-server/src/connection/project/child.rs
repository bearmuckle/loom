use super::*;

mod control;
mod create;
mod recovery;
mod result;
mod schedule;
mod worktree;

/// Deadline for the remote refresh that runs before a checkout is created or
/// measured. The refresh is bounded because a stalled remote must not hold the
/// project scheduling locks indefinitely (#274); a remote that does not answer
/// within this window is reported as an unavailable base instead of blocking
/// creation.
pub(crate) const CHILD_WORKTREE_REFRESH_DEADLINE: Duration = Duration::from_secs(10);

/// A recorded base refresh is reused while it is younger than this, so one
/// child creation fetches once instead of once per call site that creates the
/// checkout.
pub(crate) const CHILD_WORKTREE_REFRESH_REUSE: Duration = Duration::from_secs(60);

/// Reports whether a recorded base refresh is recent enough to be reused
/// instead of fetching again.
pub(crate) fn base_refresh_is_recent(
    refresh: Option<&ProjectWorktreeBaseRefresh>,
    now: Timestamp,
) -> bool {
    let Some(refresh) = refresh else {
        return false;
    };
    let age = now
        .as_unix_millis()
        .saturating_sub(refresh.reported_at().as_unix_millis());
    age <= CHILD_WORKTREE_REFRESH_REUSE.as_millis() as u64
}

impl InProcessConnection {
    /// Fetches `origin` with a bounded deadline and records how far a
    /// checkout's `base_revision` is from the default-branch tip that fetch
    /// resolved.
    ///
    /// A failed or expired refresh becomes an `Unavailable` record and a log
    /// warning rather than an error: an unreachable remote must not block the
    /// caller, and the cached `origin/*` refs are deliberately not used as a
    /// stand-in for the fetch that failed.
    pub(crate) fn refresh_checkout_base_revision(
        &self,
        git: &GitService,
        base_revision: &str,
    ) -> ProjectWorktreeBaseRefresh {
        let token = self.origin_fetch_token(git);
        match git.refresh_origin_default_branch(
            Some(base_revision),
            token.as_deref(),
            CHILD_WORKTREE_REFRESH_DEADLINE,
        ) {
            Ok(report) => ProjectWorktreeBaseRefresh::Fetched {
                remote: report.remote,
                base_revision: base_revision.to_owned(),
                default_branch: report.default_branch,
                default_branch_revision: report.default_branch_revision,
                ahead: u32::try_from(report.ahead).unwrap_or(u32::MAX),
                behind: u32::try_from(report.behind).unwrap_or(u32::MAX),
                fetched_at: report.fetched_at,
            },
            Err(error) => {
                log::warn!(
                    "could not refresh 'origin' for the checkout at {base_revision}: {}",
                    error.message
                );
                ProjectWorktreeBaseRefresh::Unavailable {
                    remote: "origin".to_owned(),
                    base_revision: base_revision.to_owned(),
                    attempted_at: Timestamp::now(),
                    warning: error.message,
                }
            }
        }
    }

    /// Access token used to refresh a checkout's `origin`: the authenticated
    /// GitHub account token for GitHub HTTPS remotes, matching how clones are
    /// authenticated, and nothing for remotes that need no credentials.
    fn origin_fetch_token(&self, parent_git: &GitService) -> Option<String> {
        let origin = parent_git.remote_url("origin").ok().flatten()?;
        let url = url::Url::parse(&origin).ok()?;
        if url.scheme() != "https" || url.host_str() != Some("github.com") {
            return None;
        }
        self.backend.providers.github_account_token().ok()
    }
}
