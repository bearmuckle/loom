use super::*;

use loom_core::ParticipantIdentity;
use uuid::Uuid;

/// A credential a sign-in issued: the value to hand to the client, and the id
/// that revokes it again.
///
/// The value is a secret and must travel only to the connection that signed in;
/// the id is not.
pub(crate) struct IssuedPersonToken {
    pub(crate) token: String,
    pub(crate) token_id: String,
}

impl InProcessBackend {
    /// The sign-in credentials this worker hands out and keeps durable.
    fn person_tokens(&self) -> MutexGuard<'_, PersonTokenState> {
        self.person_tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The token store of the listener serving this backend, when one attached
    /// it.
    fn person_token_store(&self) -> Option<Arc<AuthTokenStore>> {
        self.person_tokens().store.clone()
    }

    /// Attaches the store the listener authenticates connections with and
    /// restores the durable sign-in credentials into it.
    ///
    /// Every listener attaches its own store, including the ones tests build
    /// directly, so a credential a person keeps is recognized by the listener
    /// that serves them. A restore failure is logged and never fails the
    /// listener: a worker whose database is briefly unreadable still serves,
    /// and people can sign in again instead of being locked out.
    pub(crate) fn attach_person_token_store(&self, store: Arc<AuthTokenStore>) {
        if let Err(error) = self.restore_person_tokens(&store) {
            log::warn!(
                "[loom-server] stored GitHub sign-in credentials could not be read; people can \
                 sign in again, so no credential was restored: {}",
                error.message
            );
        }
        self.person_tokens().store = Some(store);
    }

    /// Registers every stored credential in `store`.
    fn restore_person_tokens(&self, store: &AuthTokenStore) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let stored = persistence.load_person_tokens()?;
        let mut restored = 0_usize;
        for token in stored {
            let token_id = Uuid::from_bytes(token.token_id).to_string();
            let Some(participant) = self.access.known_identity(token.participant_id) else {
                log::warn!(
                    "[loom-server] a stored GitHub sign-in credential was dropped: participant \
                     {} is not in the directory, so token {token_id} can no longer be used",
                    token.participant_id
                );
                continue;
            };
            if let Err(error) = store.insert_restored(
                token_id.clone(),
                token.digest,
                AuthorizationScope::all(),
                participant.clone(),
            ) {
                log::warn!(
                    "[loom-server] a stored GitHub sign-in credential of {} could not be \
                     restored: token {token_id}: {}",
                    participant.display_name,
                    error.message
                );
                continue;
            }
            restored += 1;
        }
        if restored > 0 {
            log::info!(
                "[loom-server] restored {restored} GitHub sign-in credential(s) for a later \
                 connection"
            );
        }
        Ok(())
    }

    /// Mints a credential that authenticates as `participant` on a later
    /// connection, or `None` when no listener attached a store.
    ///
    /// The value is returned exactly once, to the connection that reports the
    /// sign-in, and only its digest is kept. The person keeps the most recent
    /// [`MAX_PERSON_TOKENS`] credentials: older ones are revoked and deleted,
    /// so repeated sign-ins cannot grow the store without bound. A credential
    /// that cannot be saved is revoked again before this returns, because one
    /// the worker would not recognize after a restart is never handed out.
    pub(crate) fn issue_person_token(
        &self,
        participant: &ParticipantIdentity,
    ) -> Result<Option<IssuedPersonToken>> {
        let Some(store) = self.person_token_store() else {
            return Ok(None);
        };
        let issued = store.issue_for_participant(AuthorizationScope::all(), participant.clone())?;
        let stored = StoredPersonToken {
            token_id: stored_token_id(&issued.token_id)?,
            digest: AuthTokenStore::token_digest(&issued.token),
            participant_id: participant.id,
            created_at: Timestamp::now(),
        };
        if let Some(persistence) = &self.persistence {
            let saved = {
                let _gate = self
                    .state_persist_gate
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                persistence.save_person_token(&stored)
            };
            if let Err(error) = saved {
                // A credential the worker would not recognize after a restart
                // is never handed out.
                if let Err(revoke_error) = store.revoke(&issued.token_id) {
                    log::warn!(
                        "[loom-server] a GitHub sign-in credential of {} could not be saved or \
                         revoked, so it stays unusable until the process restarts: {}",
                        participant.display_name,
                        revoke_error.message
                    );
                }
                return Err(error);
            }
        }
        self.prune_person_tokens(&store, participant);
        Ok(Some(IssuedPersonToken {
            token: issued.token,
            token_id: issued.token_id,
        }))
    }

    /// Revokes and deletes the credentials of `participant` beyond the cap,
    /// oldest first.
    fn prune_person_tokens(&self, store: &AuthTokenStore, participant: &ParticipantIdentity) {
        let ids = match store.participant_token_ids(participant.id) {
            Ok(ids) => ids,
            Err(error) => {
                log::warn!(
                    "[loom-server] the GitHub sign-in credentials of {} could not be listed, so \
                     none were pruned: {}",
                    participant.display_name,
                    error.message
                );
                return;
            }
        };
        let excess = ids.len().saturating_sub(MAX_PERSON_TOKENS);
        if excess == 0 {
            return;
        }
        let mut pruned = 0_usize;
        for token_id in ids.into_iter().take(excess) {
            match store.revoke(&token_id) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    log::warn!(
                        "[loom-server] the oldest GitHub sign-in credential of {} could not be \
                         revoked: token {token_id}: {}",
                        participant.display_name,
                        error.message
                    );
                    continue;
                }
            }
            if let Err(error) = store.delete(&token_id) {
                log::warn!(
                    "[loom-server] a revoked GitHub sign-in credential of {} was not dropped: \
                     token {token_id}: {}",
                    participant.display_name,
                    error.message
                );
            }
            self.delete_person_token_row(&token_id);
            pruned += 1;
        }
        log::info!(
            "[loom-server] {} signed in on more than {MAX_PERSON_TOKENS} devices; {pruned} older \
             credential(s) were revoked",
            participant.display_name
        );
    }

    /// Revokes the credential `session` authenticated with, when that
    /// credential belongs to a person.
    ///
    /// An operator token is not bound to a person, so a sign-out never revokes
    /// one, and a credential this backend does not know is left alone. The
    /// revoked record is dropped as well, so nothing is left to revoke when the
    /// person signs out again.
    pub(crate) fn revoke_person_token(&self, session: &AuthSession) -> Result<()> {
        let Some(participant) = session.participant() else {
            return Ok(());
        };
        if !session.store().revoke(session.token_id())? {
            return Ok(());
        }
        if let Err(error) = session.store().delete(session.token_id()) {
            log::warn!(
                "[loom-server] the revoked GitHub sign-in credential {} of {} is still in the \
                 store: {}",
                session.token_id(),
                participant.display_name,
                error.message
            );
        }
        self.delete_person_token_row(session.token_id());
        log::info!(
            "[loom-server] {} signed out, so credential {} was revoked for every later connection",
            participant.display_name,
            session.token_id()
        );
        Ok(())
    }

    /// Revokes the credential a sign-in on this connection issued.
    ///
    /// A connection accepted without a credential signs in and stays that way:
    /// it carries no credential of its own, so the credential it hands to the
    /// client is the only handle on that sign-in. Signing out revokes it here,
    /// which is what makes "signing out ends it everywhere" true for the
    /// connection that signed in without one.
    pub(crate) fn revoke_person_token_id(&self, token_id: &str) -> Result<()> {
        let Some(store) = self.person_token_store() else {
            return Ok(());
        };
        if !store.revoke(token_id)? {
            return Ok(());
        }
        if let Err(error) = store.delete(token_id) {
            log::warn!(
                "[loom-server] the revoked GitHub sign-in credential {token_id} is still in the \
                 store: {}",
                error.message
            );
        }
        self.delete_person_token_row(token_id);
        log::info!(
            "[loom-server] a sign-out revoked the GitHub sign-in credential {token_id} for every \
             later connection"
        );
        Ok(())
    }

    /// Removes the durable row of a credential that no longer authenticates.
    ///
    /// A failure is logged instead of failing the caller: the credential is
    /// already revoked in memory, so the sign-out or the pruning has taken
    /// effect; only the stored row, which a restart would restore, is left.
    fn delete_person_token_row(&self, token_id: &str) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        let stored_id = match stored_token_id(token_id) {
            Ok(stored_id) => stored_id,
            Err(error) => {
                log::warn!(
                    "[loom-server] the stored row of a revoked GitHub sign-in credential could not \
                     be named: token {token_id}: {}",
                    error.message
                );
                return;
            }
        };
        let _gate = self
            .state_persist_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match persistence.delete_person_token(stored_id) {
            Ok(_) => {}
            Err(error) => log::warn!(
                "[loom-server] the stored row of the revoked GitHub sign-in credential {token_id} \
                 was not deleted, so a restart would restore it: {}",
                error.message
            ),
        }
    }
}

/// The durable form of a token id: the `Uuid` byte form the store's string id
/// came from, so one credential is named the same in memory and in the
/// database.
fn stored_token_id(token_id: &str) -> Result<[u8; 16]> {
    Uuid::parse_str(token_id)
        .map(|uuid| *uuid.as_bytes())
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("sign-in credential id is not a UUID: {error}"),
                false,
            )
        })
}
