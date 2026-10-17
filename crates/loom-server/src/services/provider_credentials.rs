//! Participant-owned provider credentials, their versions, session
//! selections, and run pins.
//!
//! - A sign-in by a person creates a new *version* of that person's credential
//!   for the provider. Older versions stay resolvable, so a run that pinned
//!   one keeps working until the credential is revoked.
//! - A session owner selects which credential later runs of the session use.
//! - A run pins one credential id and version when it starts and keeps it for
//!   its lifetime.
//!
//! Secrets are stored only in this worker's private credential store under
//! opaque per-version references. Metadata (provider, owner, label, version)
//! is the only thing that leaves the worker. Nothing here logs a secret.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use loom_core::{
    AgentSessionId, CredentialId, ErrorCode, LoomError, ParticipantId, ParticipantIdentity,
    ProviderCredentialMetadata, Result, RunCredentialPin, RunId, SessionCredentialSelection,
    Timestamp,
};
use loom_model::{
    CancellationToken, ModelCapabilities, ModelDescriptor, ModelProvider, ModelRequest,
    ModelStreamSink,
};
use loom_providers::{CredentialRef, CredentialStore};
use serde::{Deserialize, Serialize};

/// Non-secret entry holding the credential metadata, selections, and pins.
const REGISTRY_REF: &str = "provider-credential-registry";
/// Prefix of the per-version secret references.
const SECRET_REF_PREFIX: &str = "provider-credential:";

#[derive(Default, Deserialize, Serialize)]
struct State {
    #[serde(default)]
    credentials: BTreeMap<CredentialId, ProviderCredentialMetadata>,
    #[serde(default)]
    selections: BTreeMap<AgentSessionId, SessionCredentialSelection>,
    #[serde(default)]
    pins: BTreeMap<RunId, RunCredentialPin>,
}

/// An authentication failure that names the recovery path. It never suggests
/// or performs a fallback to another account.
pub(crate) fn credential_error(message: impl std::fmt::Display) -> LoomError {
    LoomError::new(
        ErrorCode::ProviderAuthentication,
        format!(
            "{message}; the session owner must sign in again or select another credential \
             before a run can use it"
        ),
        false,
    )
}

fn secret_ref(id: CredentialId, version: u32) -> CredentialRef {
    CredentialRef::new(format!("{SECRET_REF_PREFIX}{id}:v{version}"))
}

#[derive(Default)]
pub(crate) struct ProviderCredentialService {
    state: Mutex<State>,
}

impl ProviderCredentialService {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn save(state: &State, store: &dyn CredentialStore) -> Result<()> {
        let encoded = serde_json::to_string(state).map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not encode provider credential metadata: {error}"),
                false,
            )
        })?;
        store.store(&CredentialRef::new(REGISTRY_REF), encoded)
    }

    /// Loads metadata saved by an earlier process. A store without an entry
    /// leaves the service empty.
    pub(crate) fn restore(&self, store: &dyn CredentialStore) -> Result<()> {
        let encoded = match store.resolve(&CredentialRef::new(REGISTRY_REF)) {
            Ok(encoded) => encoded,
            Err(error) if error.code == ErrorCode::ProviderAuthentication => return Ok(()),
            Err(error) => return Err(error),
        };
        let state = serde_json::from_str::<State>(&encoded).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("stored provider credential metadata is invalid: {error}"),
                false,
            )
        })?;
        *self.state() = state;
        Ok(())
    }

    /// Records a new sign-in of `owner` with `provider`.
    ///
    /// The person's existing credential for the provider gets a new version;
    /// the secret of every older version stays in place for runs that pinned
    /// it. A person without one, or whose last one was revoked, gets a new
    /// credential.
    pub(crate) fn sign_in(
        &self,
        store: &dyn CredentialStore,
        provider: &str,
        owner: &ParticipantIdentity,
        secret: String,
    ) -> Result<ProviderCredentialMetadata> {
        if secret.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "provider credential secret must not be empty",
            ));
        }
        let mut state = self.state();
        let now = Timestamp::now();
        let existing = state
            .credentials
            .values()
            .find(|credential| {
                credential.owner == Some(owner.id)
                    && credential.provider == provider
                    && !credential.revoked
            })
            .cloned();
        let mut credential = existing.unwrap_or_else(|| ProviderCredentialMetadata {
            id: CredentialId::new(),
            provider: provider.to_owned(),
            owner: Some(owner.id),
            owner_label: owner.display_name.clone(),
            version: 0,
            created_at: now,
            updated_at: now,
            revoked: false,
        });
        credential.version = credential.version.saturating_add(1);
        credential.owner_label = owner.display_name.clone();
        credential.updated_at = now;
        // The secret is written before the metadata names its version, so a
        // crash between the two never leaves a version without a secret.
        store.store(&secret_ref(credential.id, credential.version), secret)?;
        let previous = state.credentials.insert(credential.id, credential.clone());
        if let Err(error) = Self::save(&state, store) {
            match previous {
                Some(previous) => state.credentials.insert(credential.id, previous),
                None => state.credentials.remove(&credential.id),
            };
            return Err(error);
        }
        Ok(credential)
    }

    /// Credentials of `owner`, or every credential when `owner` is `None`.
    pub(crate) fn list(&self, owner: Option<ParticipantId>) -> Vec<ProviderCredentialMetadata> {
        self.state()
            .credentials
            .values()
            .filter(|credential| owner.is_none() || credential.owner == owner)
            .cloned()
            .collect()
    }

    pub(crate) fn get(&self, id: CredentialId) -> Option<ProviderCredentialMetadata> {
        self.state().credentials.get(&id).cloned()
    }

    /// Selects `credential_id` for later runs of `session_id`. A person may
    /// select only a credential they own; an operator (`by == None`) may
    /// select any credential.
    pub(crate) fn select(
        &self,
        store: &dyn CredentialStore,
        session_id: AgentSessionId,
        credential_id: CredentialId,
        by: Option<ParticipantId>,
    ) -> Result<SessionCredentialSelection> {
        let mut state = self.state();
        let credential = state
            .credentials
            .get(&credential_id)
            .ok_or_else(|| LoomError::not_found("provider credential", credential_id))?;
        if by.is_some() && credential.owner != by {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "only the person who signed in with a credential may select it",
                false,
            ));
        }
        if credential.revoked {
            return Err(credential_error(format!(
                "credential {credential_id} was revoked"
            )));
        }
        let selection = SessionCredentialSelection {
            credential_id,
            selected_by: by,
            selected_at: Timestamp::now(),
        };
        let previous = state.selections.insert(session_id, selection.clone());
        if let Err(error) = Self::save(&state, store) {
            match previous {
                Some(previous) => state.selections.insert(session_id, previous),
                None => state.selections.remove(&session_id),
            };
            return Err(error);
        }
        Ok(selection)
    }

    /// The session's selection and the selected credential's metadata. The
    /// metadata is `None` when the selected credential no longer exists.
    pub(crate) fn selection(
        &self,
        session_id: AgentSessionId,
    ) -> Option<(
        SessionCredentialSelection,
        Option<ProviderCredentialMetadata>,
    )> {
        let state = self.state();
        let selection = state.selections.get(&session_id)?.clone();
        let credential = state.credentials.get(&selection.credential_id).cloned();
        Some((selection, credential))
    }

    /// Revokes a credential in every version and drops its secrets. A person
    /// may revoke only their own credential; an operator may revoke any.
    pub(crate) fn revoke(
        &self,
        store: &dyn CredentialStore,
        credential_id: CredentialId,
        by: Option<ParticipantId>,
    ) -> Result<()> {
        let mut state = self.state();
        let credential = state
            .credentials
            .get_mut(&credential_id)
            .ok_or_else(|| LoomError::not_found("provider credential", credential_id))?;
        if by.is_some() && credential.owner != by {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "only the person who signed in with a credential may revoke it",
                false,
            ));
        }
        credential.revoked = true;
        credential.updated_at = Timestamp::now();
        let versions = credential.version;
        Self::save(&state, store)?;
        for version in 1..=versions {
            // Overwrite rather than keep the secret; resolution already
            // refuses a revoked credential.
            store.store(&secret_ref(credential_id, version), String::new())?;
        }
        Ok(())
    }

    pub(crate) fn is_revoked(&self, credential_id: CredentialId) -> bool {
        self.state()
            .credentials
            .get(&credential_id)
            .is_none_or(|credential| credential.revoked)
    }

    /// Resolves the secret of one pinned version on this worker.
    pub(crate) fn resolve_secret(
        &self,
        store: &dyn CredentialStore,
        credential_id: CredentialId,
        version: u32,
    ) -> Result<String> {
        match self.get(credential_id) {
            None => {
                return Err(credential_error(format!(
                    "credential {credential_id} no longer exists"
                )));
            }
            Some(credential) if credential.revoked => {
                return Err(credential_error(format!(
                    "credential {credential_id} of {} was revoked",
                    credential.owner_label
                )));
            }
            Some(credential) if version == 0 || version > credential.version => {
                return Err(credential_error(format!(
                    "credential {credential_id} has no version {version}"
                )));
            }
            Some(_) => {}
        }
        match store.resolve(&secret_ref(credential_id, version)) {
            Ok(secret) if !secret.is_empty() => Ok(secret),
            _ => Err(credential_error(format!(
                "the secret of credential {credential_id} version {version} is missing on this \
                 worker"
            ))),
        }
    }

    pub(crate) fn pin(
        &self,
        store: &dyn CredentialStore,
        run_id: RunId,
        pin: RunCredentialPin,
    ) -> Result<()> {
        let mut state = self.state();
        state.pins.insert(run_id, pin);
        Self::save(&state, store)
    }

    pub(crate) fn run_pin(&self, run_id: RunId) -> Option<RunCredentialPin> {
        self.state().pins.get(&run_id).cloned()
    }
}

/// A provider bound to a pinned credential. It checks the credential before
/// every model call, so a revoked credential stops the run with an
/// authentication error instead of continuing or switching accounts.
pub(crate) struct PinnedCredentialProvider {
    pub(crate) inner: Box<dyn ModelProvider>,
    pub(crate) credentials: Arc<ProviderCredentialService>,
    pub(crate) credential_id: CredentialId,
}

impl PinnedCredentialProvider {
    fn check(&self) -> Result<()> {
        if self.credentials.is_revoked(self.credential_id) {
            Err(credential_error(format!(
                "the run's pinned credential {} was revoked",
                self.credential_id
            )))
        } else {
            Ok(())
        }
    }
}

impl ModelProvider for PinnedCredentialProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        self.inner.descriptor()
    }

    fn stream(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        self.check()?;
        self.inner.stream(request, cancel, sink)
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        self.inner.list_models()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    fn count_tokens(&self, request: &ModelRequest) -> u64 {
        self.inner.count_tokens(request)
    }

    fn health_check(&mut self) -> Result<()> {
        self.check()?;
        self.inner.health_check()
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_providers::InMemoryCredentialStore;

    fn person(name: &str) -> ParticipantIdentity {
        ParticipantIdentity {
            id: ParticipantId::new(),
            display_name: name.to_owned(),
        }
    }

    #[test]
    fn sign_ins_add_versions_and_revocation_blocks_every_version() {
        let store = InMemoryCredentialStore::default();
        let service = ProviderCredentialService::default();
        let owner = person("Ada");
        let first = service.sign_in(&store, "github-copilot", &owner, "one".to_owned());
        let first = first.unwrap();
        let second = service
            .sign_in(&store, "github-copilot", &owner, "two".to_owned())
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!((first.version, second.version), (1, 2));
        assert_eq!(service.resolve_secret(&store, first.id, 1).unwrap(), "one");
        assert_eq!(service.resolve_secret(&store, first.id, 2).unwrap(), "two");
        assert!(service.resolve_secret(&store, first.id, 3).is_err());

        let restored = ProviderCredentialService::default();
        restored.restore(&store).unwrap();
        assert_eq!(restored.get(first.id).unwrap().version, 2);

        service.revoke(&store, first.id, Some(owner.id)).unwrap();
        let error = service.resolve_secret(&store, first.id, 1).unwrap_err();
        assert_eq!(error.code, ErrorCode::ProviderAuthentication);
        let other = person("Bob");
        assert!(
            service
                .select(&store, AgentSessionId::new(), first.id, Some(other.id))
                .is_err()
        );
        assert!(
            service
                .sign_in(&store, "x", &owner, " ".to_owned())
                .is_err()
        );
    }
}
