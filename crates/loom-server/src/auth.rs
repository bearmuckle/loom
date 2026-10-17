use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use loom_core::{
    AgentSessionId, Capability, CapabilitySet, ErrorCode, LoomError,
    MAX_PARTICIPANT_DISPLAY_NAME_BYTES, ParticipantId, ParticipantIdentity, Result, WorkspaceId,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthorizationScope {
    /// `None` means every capability supported by the backend.
    pub capabilities: Option<CapabilitySet>,
    /// `None` means every session in an allowed workspace.
    pub sessions: Option<BTreeSet<AgentSessionId>>,
    /// `None` means every workspace; `Some(empty)` grants no workspace access.
    pub workspaces: Option<BTreeSet<WorkspaceId>>,
    /// When present, local repository sources must be below one of these roots.
    pub repository_source_roots: Option<Vec<PathBuf>>,
}

impl AuthorizationScope {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn for_sessions(
        sessions: impl IntoIterator<Item = AgentSessionId>,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            capabilities: Some(capabilities),
            sessions: Some(sessions.into_iter().collect()),
            workspaces: None,
            repository_source_roots: Some(Vec::new()),
        }
    }

    pub fn for_workspaces(
        workspaces: impl IntoIterator<Item = WorkspaceId>,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            capabilities: Some(capabilities),
            sessions: None,
            workspaces: Some(workspaces.into_iter().collect()),
            repository_source_roots: Some(Vec::new()),
        }
    }

    pub fn allows_capability(&self, capability: Capability) -> bool {
        self.capabilities
            .as_ref()
            .is_none_or(|capabilities| capabilities.contains(capability))
    }

    pub fn allows_session(&self, session_id: AgentSessionId) -> bool {
        self.sessions
            .as_ref()
            .is_none_or(|sessions| sessions.contains(&session_id))
    }

    pub fn allows_workspace(&self, workspace_id: WorkspaceId) -> bool {
        self.workspaces
            .as_ref()
            .is_none_or(|workspaces| workspaces.contains(&workspace_id))
    }

    pub fn with_repository_source_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.repository_source_roots
            .get_or_insert_with(Vec::new)
            .push(root.into());
        self
    }

    pub fn allows_repository_source(&self, source: &Path) -> bool {
        let Some(roots) = &self.repository_source_roots else {
            return true;
        };
        let Ok(source) = std::fs::canonicalize(source) else {
            return false;
        };
        roots
            .iter()
            .any(|root| std::fs::canonicalize(root).is_ok_and(|root| source.starts_with(root)))
    }
}

#[derive(Clone)]
pub struct AuthSession {
    store: Arc<AuthTokenStore>,
    token_id: String,
    digest: [u8; 32],
    scope: AuthorizationScope,
    participant: Option<ParticipantIdentity>,
}

impl fmt::Debug for AuthSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthSession")
            .field("token_id", &self.token_id)
            .field("scope", &self.scope)
            .field("participant", &self.participant)
            .finish_non_exhaustive()
    }
}

impl AuthSession {
    pub fn token_id(&self) -> &str {
        &self.token_id
    }

    pub fn scope(&self) -> &AuthorizationScope {
        &self.scope
    }

    /// The person this credential belongs to.
    ///
    /// The identity is bound to the token by the server operator and is never
    /// taken from a client. `None` marks an operator token: it is not bound to
    /// a person and is not subject to session roles, only to its scope.
    pub fn participant(&self) -> Option<&ParticipantIdentity> {
        self.participant.as_ref()
    }

    pub fn verify(&self) -> Result<()> {
        let active = self
            .store
            .tokens
            .lock()
            .map_err(|_| auth_internal_error())?
            .get(&self.token_id)
            .is_some_and(|record| {
                !record.revoked
                    && bool::from(record.digest.ct_eq(&self.digest))
                    && record.scope == self.scope
                    && record.participant == self.participant
            });
        if active {
            Ok(())
        } else {
            Err(LoomError::new(
                ErrorCode::AuthenticationFailed,
                "authentication token has been revoked or is no longer valid",
                false,
            ))
        }
    }
}

#[derive(Clone)]
pub struct IssuedToken {
    pub token_id: String,
    pub token: String,
    pub scope: AuthorizationScope,
    pub participant: Option<ParticipantIdentity>,
}

impl fmt::Debug for IssuedToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedToken")
            .field("token_id", &self.token_id)
            .field("token", &"<redacted>")
            .field("scope", &self.scope)
            .field("participant", &self.participant)
            .finish()
    }
}

impl fmt::Display for IssuedToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.token)
    }
}

impl IssuedToken {
    pub fn redacted(&self) -> String {
        format!("{}...", &self.token_id[..self.token_id.len().min(8)])
    }
}

#[derive(Clone, Debug)]
struct TokenRecord {
    digest: [u8; 32],
    scope: AuthorizationScope,
    participant: Option<ParticipantIdentity>,
    revoked: bool,
}

/// One entry of a participant token file.
///
/// The file is a JSON array of these objects. `participant_id` is optional:
/// without it the id is derived from the token, so it stays stable for as long
/// as the token does. Session grants are keyed by this id, so changing it (or
/// the token, when it is derived) gives the person a new identity.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantTokenEntry {
    pub token: String,
    pub display_name: String,
    #[serde(default)]
    pub participant_id: Option<ParticipantId>,
}

impl fmt::Debug for ParticipantTokenEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParticipantTokenEntry")
            .field("token", &"<redacted>")
            .field("display_name", &self.display_name)
            .field("participant_id", &self.participant_id)
            .finish()
    }
}

/// The stable participant id derived from a token when none is configured.
pub fn participant_id_for_token(token: &str) -> ParticipantId {
    let digest = Sha256::digest(format!("loom-participant:{token}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ParticipantId::from_uuid(uuid::Builder::from_random_bytes(bytes).into_uuid())
}

/// Validates and trims a participant display name.
pub fn validate_display_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > MAX_PARTICIPANT_DISPLAY_NAME_BYTES
        || name.chars().any(char::is_control)
    {
        return Err(LoomError::invalid_request(format!(
            "participant display names must be 1 to {MAX_PARTICIPANT_DISPLAY_NAME_BYTES} bytes \
             without control characters"
        )));
    }
    Ok(name.to_owned())
}

/// Parses a participant token file; see [`ParticipantTokenEntry`].
pub fn parse_participant_tokens(json: &str) -> Result<Vec<ParticipantTokenEntry>> {
    let entries: Vec<ParticipantTokenEntry> = serde_json::from_str(json).map_err(|error| {
        LoomError::invalid_request(format!("participant token file is not valid: {error}"))
    })?;
    let mut tokens = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for entry in &entries {
        if entry.token.trim().is_empty() || !tokens.insert(entry.token.clone()) {
            return Err(LoomError::invalid_request(
                "participant tokens must be non-empty and unique",
            ));
        }
        validate_display_name(&entry.display_name)?;
        let id = entry
            .participant_id
            .unwrap_or_else(|| participant_id_for_token(&entry.token));
        if !ids.insert(id) {
            return Err(LoomError::invalid_request(
                "participant ids in the token file must be unique",
            ));
        }
    }
    Ok(entries)
}

#[derive(Clone, Debug, Default)]
pub struct AuthTokenStore {
    tokens: Arc<Mutex<BTreeMap<String, TokenRecord>>>,
}

impl AuthTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a new `loom-<uuid>` token value without registering it, so a
    /// standalone server can write the same shape it would issue itself.
    pub fn generate_token() -> String {
        format!("loom-{}", Uuid::new_v4().simple())
    }

    pub fn issue(&self, scope: AuthorizationScope) -> Result<IssuedToken> {
        self.insert(Self::generate_token(), scope)
    }

    pub fn insert(
        &self,
        token: impl Into<String>,
        scope: AuthorizationScope,
    ) -> Result<IssuedToken> {
        self.insert_record(token.into(), scope, None)
    }

    /// Registers a token that authenticates as `participant`.
    ///
    /// Connections using it are bound to that identity and are subject to
    /// session roles in addition to `scope`.
    pub fn insert_for_participant(
        &self,
        token: impl Into<String>,
        scope: AuthorizationScope,
        participant: ParticipantIdentity,
    ) -> Result<IssuedToken> {
        let participant = ParticipantIdentity {
            id: participant.id,
            display_name: validate_display_name(&participant.display_name)?,
        };
        self.insert_record(token.into(), scope, Some(participant))
    }

    /// Mints a new token for `participant`; see [`Self::insert_for_participant`].
    pub fn issue_for_participant(
        &self,
        scope: AuthorizationScope,
        participant: ParticipantIdentity,
    ) -> Result<IssuedToken> {
        self.insert_for_participant(Self::generate_token(), scope, participant)
    }

    /// Registers every entry of a participant token file with full scope and
    /// returns the identities it bound.
    pub fn insert_participant_tokens(
        &self,
        entries: &[ParticipantTokenEntry],
    ) -> Result<Vec<ParticipantIdentity>> {
        entries
            .iter()
            .map(|entry| {
                let identity = ParticipantIdentity {
                    id: entry
                        .participant_id
                        .unwrap_or_else(|| participant_id_for_token(&entry.token)),
                    display_name: entry.display_name.clone(),
                };
                self.insert_for_participant(
                    entry.token.clone(),
                    AuthorizationScope::all(),
                    identity.clone(),
                )
                .map(|_| ParticipantIdentity {
                    id: identity.id,
                    display_name: identity.display_name.trim().to_owned(),
                })
            })
            .collect()
    }

    fn insert_record(
        &self,
        token: String,
        scope: AuthorizationScope,
        participant: Option<ParticipantIdentity>,
    ) -> Result<IssuedToken> {
        let token_id = Uuid::new_v4().to_string();
        let digest = digest(&token);
        self.tokens
            .lock()
            .map_err(|_| auth_internal_error())?
            .insert(
                token_id.clone(),
                TokenRecord {
                    digest,
                    scope: scope.clone(),
                    participant: participant.clone(),
                    revoked: false,
                },
            );
        Ok(IssuedToken {
            token_id,
            token,
            scope,
            participant,
        })
    }

    pub fn revoke(&self, token_id: &str) -> Result<bool> {
        let mut tokens = self.tokens.lock().map_err(|_| auth_internal_error())?;
        let Some(record) = tokens.get_mut(token_id) else {
            return Ok(false);
        };
        record.revoked = true;
        Ok(true)
    }

    pub fn revoke_all(&self) -> Result<()> {
        let mut tokens = self.tokens.lock().map_err(|_| auth_internal_error())?;
        for record in tokens.values_mut() {
            record.revoked = true;
        }
        Ok(())
    }

    pub fn authenticate(&self, token: &str) -> Result<AuthSession> {
        if token.trim().is_empty() {
            return Err(LoomError::new(
                ErrorCode::AuthenticationRequired,
                "a bearer token is required",
                false,
            ));
        }
        let digest = digest(token);
        let tokens = self.tokens.lock().map_err(|_| auth_internal_error())?;
        let Some((token_id, record)) = tokens
            .iter()
            .find(|(_, record)| !record.revoked && bool::from(record.digest.ct_eq(&digest)))
        else {
            return Err(LoomError::new(
                ErrorCode::AuthenticationFailed,
                "bearer token was not recognized",
                false,
            ));
        };
        Ok(AuthSession {
            store: Arc::new(self.clone()),
            token_id: token_id.clone(),
            digest,
            scope: record.scope.clone(),
            participant: record.participant.clone(),
        })
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn auth_internal_error() -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        "authentication store lock was poisoned",
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;

    fn temporary_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("loom-auth-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn authorization_scope_applies_capability_workspace_session_and_source_limits() {
        let session = AgentSessionId::new();
        let workspace = WorkspaceId::new();
        let limited = AuthorizationScope::for_sessions(
            [session],
            CapabilitySet::new([Capability::ReadAgentSession]),
        );
        assert!(limited.allows_session(session));
        assert!(!limited.allows_session(AgentSessionId::new()));
        assert!(limited.allows_capability(Capability::ReadAgentSession));
        assert!(!limited.allows_capability(Capability::CreateAgentSession));
        assert!(limited.allows_workspace(workspace));
        assert!(!limited.allows_repository_source(Path::new("/")));

        let workspace_limited =
            AuthorizationScope::for_workspaces([workspace], CapabilitySet::default());
        assert!(workspace_limited.allows_workspace(workspace));
        assert!(!workspace_limited.allows_workspace(WorkspaceId::new()));
        assert!(!workspace_limited.allows_capability(Capability::ReadAgentSession));
        assert!(workspace_limited.allows_session(session));

        let parent = temporary_directory("scope");
        let allowed = parent.join("allowed");
        let outside = temporary_directory("outside");
        fs::create_dir_all(&allowed).unwrap();
        fs::write(allowed.join("repo.marker"), "repo").unwrap();
        let scoped = AuthorizationScope::all().with_repository_source_root(&parent);
        assert!(scoped.allows_repository_source(&allowed));
        assert!(!scoped.allows_repository_source(&outside));
        assert!(!scoped.allows_repository_source(parent.join("missing").as_path()));
        assert!(AuthorizationScope::all().allows_repository_source(Path::new("/missing")));
        fs::remove_dir_all(parent).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn issued_tokens_authenticate_verify_revoke_and_redact_secrets() {
        let store = AuthTokenStore::new();
        let issued = store.issue(AuthorizationScope::all()).unwrap();
        assert_eq!(issued.to_string(), issued.token);
        assert!(issued.redacted().starts_with(&issued.token_id[..8]));
        assert!(!format!("{issued:?}").contains(&issued.token));

        let session = store.authenticate(&issued.token).unwrap();
        assert_eq!(session.token_id(), issued.token_id);
        session.verify().unwrap();
        assert!(!format!("{session:?}").contains(&issued.token));
        assert_eq!(
            store.authenticate("  ").unwrap_err().code,
            ErrorCode::AuthenticationRequired
        );
        assert_eq!(
            store.authenticate("unknown").unwrap_err().code,
            ErrorCode::AuthenticationFailed
        );

        assert!(store.revoke(&issued.token_id).unwrap());
        assert!(store.revoke(&issued.token_id).unwrap());
        assert!(!store.revoke("missing-token-id").unwrap());
        assert!(session.verify().is_err());

        let first = store.insert("first", AuthorizationScope::all()).unwrap();
        let second = store.insert("second", AuthorizationScope::all()).unwrap();
        store.revoke_all().unwrap();
        assert!(store.authenticate(&first.token).is_err());
        assert!(store.authenticate(&second.token).is_err());
    }

    #[test]
    fn participant_tokens_bind_a_server_derived_identity() {
        let entries = parse_participant_tokens(
            r#"[
                {"token": "sam-token", "display_name": "  Sam  "},
                {"token": "nadia-token", "display_name": "Nadia",
                 "participant_id": "00000000-0000-4000-8000-000000000001"}
            ]"#,
        )
        .unwrap();
        assert!(!format!("{entries:?}").contains("sam-token"));
        let store = AuthTokenStore::new();
        let identities = store.insert_participant_tokens(&entries).unwrap();
        assert_eq!(identities[0].display_name, "Sam");
        assert_eq!(identities[0].id, participant_id_for_token("sam-token"));
        assert_eq!(
            identities[1].id.to_string(),
            "00000000-0000-4000-8000-000000000001"
        );
        let session = store.authenticate("sam-token").unwrap();
        assert_eq!(session.participant(), Some(&identities[0]));
        session.verify().unwrap();
        // Derived ids are stable for a token and distinct across tokens.
        assert_eq!(
            participant_id_for_token("sam-token"),
            participant_id_for_token("sam-token")
        );
        assert_ne!(
            participant_id_for_token("sam-token"),
            participant_id_for_token("nadia-token")
        );
        let operator = store.issue(AuthorizationScope::all()).unwrap();
        assert!(
            store
                .authenticate(&operator.token)
                .unwrap()
                .participant()
                .is_none()
        );

        for invalid in [
            "not json",
            r#"[{"token": "a", "display_name": " "}]"#,
            r#"[{"token": "a", "display_name": "A"}, {"token": "a", "display_name": "B"}]"#,
            r#"[{"token": "", "display_name": "A"}]"#,
            r#"[{"token": "a", "display_name": "A", "role": "owner"}]"#,
            r#"[{"token": "a", "display_name": "A",
                 "participant_id": "00000000-0000-4000-8000-000000000001"},
                {"token": "b", "display_name": "B",
                 "participant_id": "00000000-0000-4000-8000-000000000001"}]"#,
        ] {
            assert_eq!(
                parse_participant_tokens(invalid).unwrap_err().code,
                ErrorCode::InvalidRequest,
                "{invalid}"
            );
        }
        assert!(validate_display_name("bad\nname").is_err());
        assert!(validate_display_name(&"x".repeat(129)).is_err());
    }

    #[test]
    fn poisoned_authentication_store_returns_a_retryable_internal_error() {
        let store = AuthTokenStore::new();
        let tokens = Arc::clone(&store.tokens);
        let _ = std::thread::spawn(move || {
            let _guard = tokens.lock().unwrap();
            panic!("poison lock for the test");
        })
        .join();

        let error = store.issue(AuthorizationScope::all()).unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.retryable);
    }
}
