use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use super::*;

struct GitHubCopilotLoginRecord {
    status: GitHubCopilotLoginStatus,
    expires_at: Instant,
}

/// Owns the in-flight GitHub Copilot device-login records for this worker.
#[derive(Default)]
pub(crate) struct CredentialService {
    logins: Mutex<BTreeMap<String, GitHubCopilotLoginRecord>>,
}

impl CredentialService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn logins(&self) -> MutexGuard<'_, BTreeMap<String, GitHubCopilotLoginRecord>> {
        self.logins.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drops completed logins older than `retention`, then records a new pending
    /// login unless too many are already pending.
    pub(crate) fn begin_pending(
        &self,
        login_id: String,
        now: Instant,
        retention: Duration,
        max_pending: usize,
        expires_at: Instant,
    ) -> Result<()> {
        let mut logins = self.logins();
        logins.retain(|_, login| login.expires_at + retention > now);
        let pending = logins
            .values()
            .filter(|login| matches!(login.status, GitHubCopilotLoginStatus::Pending))
            .count();
        if pending >= max_pending {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                "too many GitHub Copilot sign-ins are already pending on this worker",
                true,
            ));
        }
        logins.insert(
            login_id,
            GitHubCopilotLoginRecord {
                status: GitHubCopilotLoginStatus::Pending,
                expires_at,
            },
        );
        Ok(())
    }

    pub(crate) fn remove(&self, login_id: &str) {
        self.logins().remove(login_id);
    }

    pub(crate) fn set_expires_at(&self, login_id: &str, expires_at: Instant) {
        if let Some(login) = self.logins().get_mut(login_id) {
            login.expires_at = expires_at;
        }
    }

    /// Records a terminal login state, leaving an already-finalized record as is.
    pub(crate) fn finish(&self, login_id: &str, status: GitHubCopilotLoginStatus) {
        if let Some(login) = self.logins().get_mut(login_id)
            && matches!(login.status, GitHubCopilotLoginStatus::Pending)
        {
            login.status = status;
        }
    }

    /// Returns the login status, materializing an expired pending login as a
    /// failure.
    pub(crate) fn status(&self, login_id: &str, now: Instant) -> Result<GitHubCopilotLoginStatus> {
        let mut logins = self.logins();
        let login = logins
            .get_mut(login_id)
            .ok_or_else(|| LoomError::not_found("GitHub Copilot sign-in", login_id))?;
        if matches!(login.status, GitHubCopilotLoginStatus::Pending) && now >= login.expires_at {
            login.status = GitHubCopilotLoginStatus::Failed {
                message: "GitHub device authorization expired".to_owned(),
            };
        }
        Ok(login.status.clone())
    }
}
