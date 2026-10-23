//! GitHub sign-in: binding a person to a connection after it was accepted.
//!
//! A connection accepted without a credential is pre-login: it may negotiate,
//! discover capabilities, read worker status and the current participant, and
//! sign in or out. Every other request is refused until a person signs in, and
//! that gate lives here beside the binding it waits for.

use loom_core::{
    ErrorCode, GitHubSignInStatus, LoomError, ParticipantIdentity, Result, SessionRole,
};
use loom_protocol::{AccessRequest, AccessResponse, ClientRequest, ControlRequest};

use super::*;
use crate::services::identity::MAX_SIGN_IN_LIFETIME;

impl InProcessConnection {
    /// Requests a connection may make before it is bound to a person.
    fn is_available_before_sign_in(request: &ClientRequest) -> bool {
        matches!(
            request,
            ClientRequest::Control(
                ControlRequest::Negotiate { .. }
                    | ControlRequest::DiscoverCapabilities
                    | ControlRequest::GetWorkerNodeStatus
            ) | ClientRequest::Access(
                AccessRequest::GetCurrentParticipant
                    | AccessRequest::StartGitHubSignIn
                    | AccessRequest::GetGitHubSignInStatus { .. }
                    | AccessRequest::SignOutGitHub
            )
        )
    }

    /// Refuses every request a pre-login connection may not make yet.
    pub(crate) fn require_sign_in_for(&self, request: &ClientRequest) -> Result<()> {
        if !self.awaits_sign_in() || Self::is_available_before_sign_in(request) {
            return Ok(());
        }
        Err(LoomError::new(
            ErrorCode::AuthenticationRequired,
            "this connection is not signed in to GitHub; negotiate, discover capabilities, read \
             worker status or the current participant, or sign in first",
            false,
        ))
    }

    /// Binds this connection to `identity`.
    ///
    /// The person is added to the participant directory, a presence is
    /// registered for them (which pushes the new roster to every session they
    /// hold access to), and the directory is saved so an owner can grant them
    /// access after a restart. An operator token on the connection is kept:
    /// the connection is then both an operator client and a person.
    pub(crate) fn bind_participant(
        &self,
        identity: ParticipantIdentity,
    ) -> Result<ParticipantIdentity> {
        let identity = ParticipantIdentity {
            id: identity.id,
            display_name: validate_display_name(&identity.display_name)?,
        };
        // Registered before the lock is taken: `connect` pushes the roster and
        // must never run under the connection's identity lock.
        let presence = Arc::new(self.backend.access.connect(&identity));
        {
            let mut state = self.identity_state()?;
            state.participant = Some(identity.clone());
            state.presence = Some(presence);
        }
        if let Err(error) = self.backend.persist_state() {
            self.unbind_participant();
            return Err(error);
        }
        Ok(identity)
    }

    /// Releases the person this connection is bound to, if any.
    ///
    /// The presence handle is dropped outside the identity lock, which removes
    /// the person's connection from the roster. An operator token the
    /// connection authenticated with is left in place, so the connection
    /// returns to an unbound operator (or, when it was accepted without a
    /// credential, to the pre-login state).
    pub(crate) fn unbind_participant(&self) -> Option<ParticipantIdentity> {
        let (participant, presence) = {
            let Ok(mut state) = self.identity_state() else {
                return None;
            };
            (state.participant.take(), state.presence.take())
        };
        drop(presence);
        participant
    }

    /// Whether a GitHub sign-in on this connection also adopts every session
    /// that has no owner.
    ///
    /// Only an operator token whose scope reaches every session and workspace
    /// qualifies: `None` in both fields means every one of them, and
    /// [`AuthorizationScope::all`] is that default. Such a token already reaches
    /// every session, so ownership follows continuity rather than adding
    /// authority. A pre-login connection holds no credential, and a token
    /// narrowed by [`AuthorizationScope::for_sessions`] or
    /// [`AuthorizationScope::for_workspaces`] names what it may reach, so
    /// neither adopts. A token bound to a person cannot sign in at all, because
    /// the connection is already bound to that person.
    pub(crate) fn adopts_ownerless_sessions(&self) -> bool {
        self.auth_session().is_some_and(|auth| {
            let scope = auth.scope();
            auth.participant().is_none() && scope.sessions.is_none() && scope.workspaces.is_none()
        })
    }

    /// Whether this connection still holds operator authority after a sign-in.
    ///
    /// An unrestricted operator token reaches everything already, so binding a
    /// GitHub identity on top of it cannot narrow what the token may do. The
    /// person is then an operator with an attributed identity: session roles are
    /// not consulted, and worker-wide GitHub browsing stays available. The same
    /// applies to an in-process connection, which is an operator without any
    /// credential. A pre-login connection that signed in, a credential-bound
    /// person, and a token narrowed by scope are governed by their roles instead.
    pub(crate) fn retains_operator_authority(&self) -> bool {
        if self.pre_login {
            return false;
        }
        match self.auth_session() {
            Some(_) => self.adopts_ownerless_sessions(),
            None => true,
        }
    }

    /// Makes a person who just signed in the owner of every session that has no
    /// owner, when this connection authenticated with an unrestricted operator
    /// token (see [`Self::adopts_ownerless_sessions`]).
    ///
    /// The sessions are listed here because the access service holds no session
    /// manager. Archived sessions are included: they still exist and may be
    /// unarchived, so a person who owns the others is not locked out of them. A
    /// session that already has a grant is left untouched, which covers every
    /// session that already has an owner.
    ///
    /// Each adopted session is announced like a manual grant of the same role:
    /// [`Self::record_access_change`] pushes the new roster to the session's
    /// live readers and records the change as an attributed history entry,
    /// which also saves the state. A session whose record fails is logged by
    /// id and does not stop the others or fail the sign-in: the identity
    /// binding was saved already, so the person stays signed in and only that
    /// session's entry and save are missing.
    pub(crate) fn adopt_ownerless_sessions_for(&self, identity: &ParticipantIdentity) {
        if !self.adopts_ownerless_sessions() {
            return;
        }
        let sessions = match self.backend.sessions() {
            Ok(sessions) => sessions
                .list_in_workspace(None, true)
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            Err(error) => {
                log::error!(
                    "[loom-server] {} signed in, but the sessions to adopt could not be listed: {}",
                    identity.display_name,
                    error.message
                );
                return;
            }
        };
        let adopted = self
            .backend
            .access
            .adopt_ownerless_sessions(identity.id, sessions);
        if adopted.is_empty() {
            return;
        }
        log::info!(
            "[loom-server] {} signed in on an unrestricted operator token and now owns every \
             session that had no owner: {} session(s)",
            identity.display_name,
            adopted.len()
        );
        for session_id in adopted {
            if let Err(error) =
                self.record_access_change(session_id, identity.id, Some(SessionRole::Owner))
            {
                log::error!(
                    "[loom-server] {} signed in and now owns session {session_id}, but the change \
                     was not recorded or saved: {}",
                    identity.display_name,
                    error.message
                );
            }
        }
    }

    /// Starts a GitHub device sign-in for this connection.
    ///
    /// The sign-in requests only the `read:user` scope, reads the account once
    /// the person authorizes the device code, checks the login against the
    /// worker's allowlist, and binds this connection to the participant derived
    /// from the account's numeric id. The GitHub token is dropped afterwards and
    /// is never stored as a provider credential.
    pub(crate) fn start_github_sign_in(&self) -> Result<ServerResponse> {
        let config = self.backend.identity.config();
        if !config.is_enabled() {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                "GitHub sign-in is not enabled on this worker; start it with \
                 --allow-github-login <login> to accept sign-ins",
                false,
            ));
        }
        if let Some(participant) = self.participant() {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                format!(
                    "this connection is already signed in as {}; sign out first to sign in as \
                     someone else",
                    participant.display_name
                ),
                false,
            ));
        }

        let now = Instant::now();
        let login_id = uuid::Uuid::new_v4().to_string();
        self.backend.identity.begin_pending(
            login_id.clone(),
            now,
            now + MAX_SIGN_IN_LIFETIME,
            None,
        )?;
        let device = match config.authenticator().begin() {
            Ok(device) => device,
            Err(error) => {
                self.backend.identity.remove(&login_id);
                return Err(error);
            }
        };
        self.backend.identity.set_expires_at(
            &login_id,
            now + Duration::from_secs(device.expires_in).min(MAX_SIGN_IN_LIFETIME),
        );

        let connection = self.clone();
        let worker_login_id = login_id.clone();
        let account_endpoint = config.user_api_url().to_owned();
        let device_for_poll = device.clone();
        let spawn_result = thread::Builder::new()
            .name("github-sign-in".to_owned())
            .spawn(move || {
                let (status, resume_token) = match config.authenticator().poll(&device_for_poll) {
                    Ok(token) => match github_account_ref_at(&token, &account_endpoint) {
                        Ok(account) if !config.allows_login(&account.login) => (
                            GitHubSignInStatus::Failed {
                                message: format!(
                                    "GitHub account '{}' is not allowed to sign in to this worker",
                                    account.login
                                ),
                            },
                            None,
                        ),
                        Ok(account) => {
                            let identity = config.identity_for(&account);
                            match connection.bind_participant(identity) {
                                Ok(identity) => {
                                    log::info!(
                                        "[loom-server] {} signed in with GitHub; participant {}",
                                        identity.display_name,
                                        identity.id
                                    );
                                    // Before the status is reported, so the
                                    // client's post-sign-in refresh already
                                    // sees the grants.
                                    connection.adopt_ownerless_sessions_for(&identity);
                                    // Also before the status is reported, so
                                    // the first poll that reports `signed_in`
                                    // carries the credential a later connection
                                    // authenticates with. A failure costs only
                                    // that: the person is signed in for this
                                    // connection either way.
                                    let resume_token = match connection
                                        .backend
                                        .issue_person_token(&identity)
                                    {
                                        Ok(Some(issued)) => {
                                            // The connection keeps only the id, so
                                            // signing out here revokes the credential
                                            // it hands to the client.
                                            connection.remember_person_token(issued.token_id);
                                            Some(issued.token)
                                        }
                                        Ok(None) => None,
                                        Err(error) => {
                                            log::warn!(
                                                "[loom-server] {} signed in, but a credential for \
                                                 a later connection could not be issued: {}",
                                                identity.display_name,
                                                error.message
                                            );
                                            None
                                        }
                                    };
                                    (
                                        GitHubSignInStatus::SignedIn {
                                            participant: identity,
                                        },
                                        resume_token,
                                    )
                                }
                                Err(error) => (
                                    GitHubSignInStatus::Failed {
                                        message: error.message,
                                    },
                                    None,
                                ),
                            }
                        }
                        Err(error) => (
                            GitHubSignInStatus::Failed {
                                message: error.message,
                            },
                            None,
                        ),
                    },
                    Err(error) => (
                        GitHubSignInStatus::Failed {
                            message: error.message,
                        },
                        None,
                    ),
                };
                connection
                    .backend
                    .identity
                    .finish(&worker_login_id, status, resume_token);
            });
        if let Err(error) = spawn_result {
            self.backend.identity.remove(&login_id);
            return Err(LoomError::new(
                ErrorCode::Internal,
                format!("could not start GitHub sign-in: {error}"),
                false,
            ));
        }

        Ok(ServerResponse::Access(
            AccessResponse::GitHubSignInStarted {
                login_id,
                user_code: device.user_code,
                verification_uri: device.verification_uri,
                expires_in: device.expires_in,
                interval: device.interval,
            },
        ))
    }

    /// The state of a sign-in this connection started.
    pub(crate) fn github_sign_in_status(&self, login_id: &str) -> Result<ServerResponse> {
        // Which sign-in a connection may read is keyed by the person its
        // credential names, not by the person it is bound to: a sign-in binds
        // a person to the connection just before the record is finished, so
        // the binding can change between two polls of the same sign-in.
        let requester = self
            .auth_session()
            .and_then(|auth| auth.participant().cloned())
            .map(|participant| participant.id);
        let sign_in = self
            .backend
            .identity
            .status_for(login_id, Instant::now(), requester)?;
        Ok(ServerResponse::Access(AccessResponse::GitHubSignInStatus {
            status: sign_in.status,
            // Taken out of the sign-in record, so the credential travels with
            // the first poll that reports `signed_in` and no later one.
            resume_token: sign_in.resume_token,
        }))
    }

    /// Binds this connection back to a person it signed in as before.
    ///
    /// The client remembers the participant id for a worker, so a reload can
    /// show the person online without another device flow. Only an
    /// unrestricted operator token may resume an identity: it already reaches
    /// every session, and the worker only accepts an id that is in its
    /// participant directory. A credential-bound connection and a pre-login
    /// connection may not resume.
    pub(crate) fn resume_github_identity(
        &self,
        participant_id: loom_core::ParticipantId,
    ) -> Result<ServerResponse> {
        if self.participant().is_some() {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                "this connection is already bound to a person",
                false,
            ));
        }
        if !self.retains_operator_authority() {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "resuming a GitHub identity needs an unrestricted operator token",
                false,
            ));
        }
        let identity = self
            .backend
            .access
            .known_identity(participant_id)
            .ok_or_else(|| LoomError::not_found("participant", participant_id.to_string()))?;
        let identity = self.bind_participant(identity)?;
        log::info!(
            "[loom-server] {} resumed their GitHub identity; participant {}",
            identity.display_name,
            identity.id
        );
        Ok(ServerResponse::Access(
            AccessResponse::GitHubIdentityResumed {
                participant: identity,
            },
        ))
    }

    /// Signs this connection out of GitHub, dropping its presence from every
    /// session roster a person can see.
    ///
    /// A connection that signed in with a person's credential stops that
    /// credential, so signing out on one device ends it; an operator token the
    /// connection authenticated with is kept.
    pub(crate) fn sign_out_github(&self) -> Result<ServerResponse> {
        let Some(participant) = self.unbind_participant() else {
            return Err(LoomError::invalid_request(
                "this connection is not signed in to GitHub",
            ));
        };
        if let Some(auth) = self.auth_session()
            && let Err(error) = self.backend.revoke_person_token(&auth)
        {
            log::warn!(
                "[loom-server] {} signed out, but the credential they signed in with could not \
                 be revoked: {}",
                participant.display_name,
                error.message
            );
        }
        // A connection accepted without a credential holds no credential of its
        // own, so the one its sign-in handed to the client is the only handle on
        // that sign-in. Revoking it here is what makes signing out end it
        // everywhere.
        if let Some(token_id) = self.take_person_token()
            && let Err(error) = self.backend.revoke_person_token_id(&token_id)
        {
            log::warn!(
                "[loom-server] {} signed out, but the credential their sign-in issued could not be \
                 revoked: {}",
                participant.display_name,
                error.message
            );
        }
        log::info!(
            "[loom-server] {} signed out of GitHub; participant {}",
            participant.display_name,
            participant.id
        );
        Ok(ServerResponse::Access(AccessResponse::SignedOutGitHub))
    }
}
