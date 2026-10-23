//! GitHub identity sign-in for people.
//!
//! The service owns two things:
//!
//! - the sign-in policy: which GitHub logins may sign in to this worker, and
//!   which OAuth app and endpoints the device flow uses;
//! - the registry of pending sign-ins, bounded like the provider credential
//!   logins in [`crate::services::credential`]: at most
//!   [`MAX_PENDING_SIGN_INS`] are pending at once, an abandoned record is
//!   dropped after [`SIGN_IN_RETENTION`], and a status read is scoped to the
//!   connection that started it.
//!
//! A sign-in binds the connection to the participant derived from the GitHub
//! account's immutable numeric id (see [`loom_core::github_participant_id`]);
//! the GitHub token is kept only long enough to read the account and is never
//! stored as a provider credential.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use loom_core::{
    ErrorCode, GitHubSignInStatus, LoomError, ParticipantId, ParticipantIdentity, Result,
    github_participant_id,
};

/// Most GitHub sign-ins one worker keeps pending at once.
pub(crate) const MAX_PENDING_SIGN_INS: usize = 8;
/// How long a finished or abandoned sign-in record is kept for a late poll.
pub(crate) const SIGN_IN_RETENTION: Duration = Duration::from_secs(300);
/// Most a device authorization may stay usable, whatever GitHub reports.
pub(crate) const MAX_SIGN_IN_LIFETIME: Duration = Duration::from_secs(3600);
/// Where the account of a signed-in person is read from.
pub const GITHUB_USER_API_URL: &str = "https://api.github.com/user";

/// Who may sign in to this worker, and where the device flow runs.
///
/// Sign-in is enabled unless it is switched off: a WebSocket upgrade without a
/// credential is then accepted as a pre-login connection, and any GitHub
/// account may sign in. Such a person starts with no session grants, so they
/// see nothing until an owner grants access. An allowlist narrows who may sign
/// in at all, which is what a worker reachable from an untrusted network
/// wants. A login is matched case-insensitively because GitHub logins are.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubSignInConfig {
    /// Whether people may sign in with GitHub at all.
    pub enabled: bool,
    /// GitHub logins allowed to sign in, as typed on the worker's command
    /// line. Empty allows every GitHub account.
    pub allowlist: Vec<String>,
    /// OAuth app client id; `None` uses the embedded GitHub CLI public client.
    pub client_id: Option<String>,
    /// Device-authorization endpoint override, for tests against a fixture.
    pub device_code_url: Option<String>,
    /// Token-exchange endpoint override, for tests against a fixture.
    pub access_token_url: Option<String>,
    /// Account endpoint override; defaults to [`GITHUB_USER_API_URL`].
    pub user_api_url: Option<String>,
}

impl Default for GitHubSignInConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowlist: Vec::new(),
            client_id: None,
            device_code_url: None,
            access_token_url: None,
            user_api_url: None,
        }
    }
}

impl GitHubSignInConfig {
    /// A policy that allows exactly `allowlist` (case-insensitively) to sign in.
    pub fn new(allowlist: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowlist: allowlist.into_iter().collect(),
            ..Self::default()
        }
    }

    /// A policy that refuses sign-in entirely, for a worker whose clients all
    /// hold an operator token.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Whether GitHub sign-in is enabled at all.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// How many logins may sign in; zero means any GitHub account may.
    pub fn allowlist_size(&self) -> usize {
        self.allowlist.len()
    }

    /// Whether `login` may sign in to this worker.
    pub fn allows_login(&self, login: &str) -> bool {
        let login = login.trim();
        if !self.is_enabled() {
            return false;
        }
        self.allowlist.is_empty()
            || self
                .allowlist
                .iter()
                .any(|allowed| allowed.trim().eq_ignore_ascii_case(login))
    }

    /// The account endpoint the sign-in reads.
    pub fn user_api_url(&self) -> &str {
        self.user_api_url.as_deref().unwrap_or(GITHUB_USER_API_URL)
    }

    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    pub fn with_device_code_url(mut self, endpoint: impl Into<String>) -> Self {
        self.device_code_url = Some(endpoint.into());
        self
    }

    pub fn with_access_token_url(mut self, endpoint: impl Into<String>) -> Self {
        self.access_token_url = Some(endpoint.into());
        self
    }

    pub fn with_user_api_url(mut self, endpoint: impl Into<String>) -> Self {
        self.user_api_url = Some(endpoint.into());
        self
    }

    /// The device-flow authenticator this policy asks for: the identity login
    /// scoped to `read:user`, pointed at the configured endpoints.
    pub(crate) fn authenticator(&self) -> loom_providers::GitHubCopilotAuthenticator {
        let mut authenticator = loom_providers::GitHubCopilotAuthenticator::identity();
        if let Some(client_id) = &self.client_id {
            authenticator = authenticator.with_client_id(client_id.clone());
        }
        if let Some(endpoint) = &self.device_code_url {
            authenticator = authenticator.with_device_code_url(endpoint.clone());
        }
        if let Some(endpoint) = &self.access_token_url {
            authenticator = authenticator.with_access_token_url(endpoint.clone());
        }
        authenticator
    }

    /// The person a GitHub account signs in as: the id comes from the numeric
    /// account id, the display name from the login.
    pub(crate) fn identity_for(
        &self,
        account: &crate::util::GitHubAccountRef,
    ) -> ParticipantIdentity {
        ParticipantIdentity {
            id: github_participant_id(account.id),
            display_name: account.login.trim().to_owned(),
        }
    }
}

struct SignInRecord {
    status: GitHubSignInStatus,
    /// The credential a sign-in hands out to the client that started it; the
    /// first read of a `signed_in` status takes it, so no later read repeats
    /// it.
    resume_token: Option<String>,
    expires_at: Instant,
    /// The person who started the sign-in; `None` for a connection that has no
    /// identity yet (a pre-login connection or an operator token). Only the
    /// initiator reads the status, so another connection cannot observe a
    /// sign-in it did not start.
    initiator: Option<ParticipantId>,
}

/// A sign-in status read by the connection that started it, beside the
/// credential it hands out once.
pub(crate) struct SignInStatus {
    pub(crate) status: GitHubSignInStatus,
    /// The credential for a later connection, present only on the first read
    /// that reports [`GitHubSignInStatus::SignedIn`].
    pub(crate) resume_token: Option<String>,
}

impl fmt::Debug for SignInStatus {
    /// Reports whether a credential is present without formatting its value.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignInStatus")
            .field("status", &self.status)
            .field(
                "resume_token",
                &self.resume_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Owns the sign-in policy and the in-flight GitHub sign-ins of one worker.
#[derive(Default)]
pub(crate) struct IdentityService {
    config: Mutex<GitHubSignInConfig>,
    sign_ins: Mutex<BTreeMap<String, SignInRecord>>,
}

impl IdentityService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Replaces the sign-in policy, for example when a listener binds with the
    /// configuration it was started with.
    pub(crate) fn configure(&self, config: GitHubSignInConfig) {
        *self.config.lock().unwrap_or_else(PoisonError::into_inner) = config;
    }

    pub(crate) fn config(&self) -> GitHubSignInConfig {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether a connection may sign in at all.
    pub(crate) fn is_enabled(&self) -> bool {
        self.config().is_enabled()
    }

    fn sign_ins(&self) -> MutexGuard<'_, BTreeMap<String, SignInRecord>> {
        self.sign_ins.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drops finished sign-ins older than [`SIGN_IN_RETENTION`], then records a
    /// new pending sign-in unless too many are already pending.
    pub(crate) fn begin_pending(
        &self,
        login_id: String,
        now: Instant,
        expires_at: Instant,
        initiator: Option<ParticipantId>,
    ) -> Result<()> {
        let mut sign_ins = self.sign_ins();
        sign_ins.retain(|_, sign_in| sign_in.expires_at + SIGN_IN_RETENTION > now);
        let pending = sign_ins
            .values()
            .filter(|sign_in| matches!(sign_in.status, GitHubSignInStatus::Pending))
            .count();
        if pending >= MAX_PENDING_SIGN_INS {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                "too many GitHub sign-ins are already pending on this worker",
                true,
            ));
        }
        sign_ins.insert(
            login_id,
            SignInRecord {
                status: GitHubSignInStatus::Pending,
                resume_token: None,
                expires_at,
                initiator,
            },
        );
        Ok(())
    }

    pub(crate) fn remove(&self, login_id: &str) {
        self.sign_ins().remove(login_id);
    }

    pub(crate) fn set_expires_at(&self, login_id: &str, expires_at: Instant) {
        if let Some(sign_in) = self.sign_ins().get_mut(login_id) {
            sign_in.expires_at = expires_at;
        }
    }

    /// Records a terminal state, leaving an already-finalized sign-in as is.
    ///
    /// `resume_token` is the credential for a later connection; it is kept
    /// beside the status so the first read reports both and no later read
    /// repeats the value.
    pub(crate) fn finish(
        &self,
        login_id: &str,
        status: GitHubSignInStatus,
        resume_token: Option<String>,
    ) {
        if let Some(sign_in) = self.sign_ins().get_mut(login_id)
            && matches!(sign_in.status, GitHubSignInStatus::Pending)
        {
            sign_in.status = status;
            sign_in.resume_token = resume_token;
        }
    }

    /// The status as seen by `requester`, with the credential it hands out.
    ///
    /// The credential is taken out of the record, so it is delivered exactly
    /// once: a client that loses the response cannot obtain it by polling
    /// again and signs in once more instead.
    ///
    /// A person sees only a sign-in they started, and a connection without an
    /// identity sees only one started without one; anything else reads as not
    /// found, so the existence of a sign-in does not leak.
    pub(crate) fn status_for(
        &self,
        login_id: &str,
        now: Instant,
        requester: Option<ParticipantId>,
    ) -> Result<SignInStatus> {
        let mut sign_ins = self.sign_ins();
        let sign_in = sign_ins
            .get_mut(login_id)
            .filter(|sign_in| match &sign_in.status {
                // Once bound, the person may still poll the sign-in that bound
                // them; before that, only the connection that started it may.
                GitHubSignInStatus::SignedIn { participant } => {
                    sign_in.initiator == requester || requester == Some(participant.id)
                }
                _ => sign_in.initiator == requester,
            })
            .ok_or_else(|| LoomError::not_found("GitHub sign-in", login_id))?;
        if matches!(sign_in.status, GitHubSignInStatus::Pending) && now >= sign_in.expires_at {
            sign_in.status = GitHubSignInStatus::Failed {
                message: "GitHub device authorization expired".to_owned(),
            };
        }
        Ok(SignInStatus {
            status: sign_in.status.clone(),
            resume_token: sign_in.resume_token.take(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(login: &str) -> ParticipantIdentity {
        ParticipantIdentity {
            id: github_participant_id(7),
            display_name: login.to_owned(),
        }
    }

    #[test]
    fn the_allowlist_is_optional_case_insensitive_and_trimmed() {
        // Sign-in is on by default and then accepts any GitHub account: such a
        // person holds no session grants until an owner grants one.
        let open = GitHubSignInConfig::default();
        assert!(open.is_enabled());
        assert_eq!(open.allowlist_size(), 0);
        assert!(open.allows_login("octocat"));
        assert!(open.allows_login("Anyone"));

        let restricted =
            GitHubSignInConfig::new([" OctoCat ".to_owned(), "bjornharrtell".to_owned()]);
        assert!(restricted.is_enabled());
        assert_eq!(restricted.allowlist_size(), 2);
        assert!(restricted.allows_login("octocat"));
        assert!(restricted.allows_login("OCTOCAT"));
        assert!(restricted.allows_login("  bjornharrtell "));
        assert!(!restricted.allows_login("mallory"));
        assert!(!restricted.allows_login(""));

        // A worker whose clients all hold an operator token can switch it off.
        let disabled = GitHubSignInConfig::disabled();
        assert!(!disabled.is_enabled());
        assert!(!disabled.allows_login("octocat"));
    }

    #[test]
    fn the_device_flow_uses_the_configured_app_and_endpoints() {
        let defaults = GitHubSignInConfig::new(["octocat".to_owned()]).authenticator();
        assert_eq!(defaults.scope, loom_providers::GITHUB_IDENTITY_OAUTH_SCOPE);
        assert_eq!(
            defaults.client_id,
            loom_providers::GITHUB_REPOSITORY_OAUTH_CLIENT_ID
        );
        assert_eq!(
            defaults.device_code_url,
            "https://github.com/login/device/code"
        );
        assert_eq!(
            defaults.access_token_url,
            "https://github.com/login/oauth/access_token"
        );

        let own_app = GitHubSignInConfig::new(["octocat".to_owned()])
            .with_client_id("own-client-id")
            .with_device_code_url("http://127.0.0.1:1/device")
            .with_access_token_url("http://127.0.0.1:1/token")
            .with_user_api_url("http://127.0.0.1:1/user")
            .authenticator();
        assert_eq!(own_app.client_id, "own-client-id");
        assert_eq!(own_app.device_code_url, "http://127.0.0.1:1/device");
        assert_eq!(own_app.access_token_url, "http://127.0.0.1:1/token");
        assert_eq!(
            GitHubSignInConfig::new(["octocat".to_owned()])
                .with_user_api_url("http://127.0.0.1:1/user")
                .user_api_url(),
            "http://127.0.0.1:1/user"
        );
        assert_eq!(
            GitHubSignInConfig::default().user_api_url(),
            GITHUB_USER_API_URL
        );
    }

    #[test]
    fn pending_sign_ins_are_bounded_expire_and_stay_with_their_initiator() {
        let service = IdentityService::new();
        let now = Instant::now();
        let expires = now + Duration::from_secs(900);
        for index in 0..MAX_PENDING_SIGN_INS {
            service
                .begin_pending(format!("login-{index}"), now, expires, None)
                .unwrap();
        }
        assert_eq!(
            service
                .begin_pending("login-extra".to_owned(), now, expires, None)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );

        // A finished sign-in frees a slot.
        service.finish(
            "login-0",
            GitHubSignInStatus::SignedIn {
                participant: identity("octocat"),
            },
            Some("person-credential".to_owned()),
        );
        service
            .begin_pending("login-new".to_owned(), now, expires, None)
            .unwrap();

        // The credential a finished sign-in hands out is read exactly once.
        let finished = service.status_for("login-0", now, None).unwrap();
        assert!(matches!(
            finished.status,
            GitHubSignInStatus::SignedIn { .. }
        ));
        assert_eq!(finished.resume_token.as_deref(), Some("person-credential"));
        assert_eq!(
            service
                .status_for("login-0", now, None)
                .unwrap()
                .resume_token,
            None
        );

        // A pending sign-in materializes as a failure once it expires.
        let expired = service
            .status_for("login-1", now + Duration::from_secs(900), None)
            .unwrap();
        assert!(matches!(expired.status, GitHubSignInStatus::Failed { .. }));
        assert_eq!(expired.resume_token, None);

        // A person sees only a sign-in they started; the connection with no
        // identity sees only one started without one.
        let started_here = identity("octocat");
        service
            .begin_pending(
                "login-person".to_owned(),
                now,
                expires,
                Some(started_here.id),
            )
            .unwrap();
        assert_eq!(
            service
                .status_for("login-person", now, None)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            service
                .status_for("login-person", now, Some(ParticipantId::new()))
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            service
                .status_for("login-person", now, Some(started_here.id))
                .unwrap()
                .status,
            GitHubSignInStatus::Pending
        );
        assert_eq!(
            service.status_for("missing", now, None).unwrap_err().code,
            ErrorCode::NotFound
        );
        service.remove("login-person");
        assert!(
            service
                .status_for("login-person", now, Some(started_here.id))
                .is_err()
        );
    }

    #[test]
    fn a_github_account_derives_the_identity_from_its_numeric_id() {
        let config = GitHubSignInConfig::new(["octocat".to_owned()]);
        let first = config.identity_for(&crate::util::GitHubAccountRef {
            id: 141030,
            login: " octocat ".to_owned(),
        });
        assert_eq!(first.id, github_participant_id(141030));
        assert_eq!(first.display_name, "octocat");
        // A rename keeps the same person.
        let renamed = config.identity_for(&crate::util::GitHubAccountRef {
            id: 141030,
            login: "octocat-renamed".to_owned(),
        });
        assert_eq!(renamed.id, first.id);
        assert_ne!(renamed, first);
    }
}
