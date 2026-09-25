use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use loom_core::{
    AgentSessionId, Capability, CapabilitySet, ErrorCode, LoomError, ProjectId, Result, WorkspaceId,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthorizationScope {
    /// `None` means every capability supported by the backend.
    pub capabilities: Option<CapabilitySet>,
    /// `None` means every project.
    pub projects: Option<BTreeSet<ProjectId>>,
    /// `None` means every session in an allowed project.
    pub sessions: Option<BTreeSet<AgentSessionId>>,
    /// `None` means every workspace; `Some(empty)` grants no workspace access.
    pub workspaces: Option<BTreeSet<WorkspaceId>>,
    /// When present, a project may only be opened at its configured root.
    pub workspace_roots: Option<BTreeMap<ProjectId, PathBuf>>,
    /// When present, local repository sources must be below one of these roots.
    pub repository_source_roots: Option<Vec<PathBuf>>,
}

impl AuthorizationScope {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn for_projects(
        projects: impl IntoIterator<Item = ProjectId>,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            capabilities: Some(capabilities),
            projects: Some(projects.into_iter().collect()),
            sessions: None,
            workspaces: None,
            workspace_roots: None,
            repository_source_roots: Some(Vec::new()),
        }
    }

    pub fn for_sessions(
        sessions: impl IntoIterator<Item = AgentSessionId>,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            capabilities: Some(capabilities),
            projects: None,
            sessions: Some(sessions.into_iter().collect()),
            workspaces: None,
            workspace_roots: None,
            repository_source_roots: Some(Vec::new()),
        }
    }

    pub fn for_workspaces(
        workspaces: impl IntoIterator<Item = WorkspaceId>,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            capabilities: Some(capabilities),
            projects: None,
            sessions: None,
            workspaces: Some(workspaces.into_iter().collect()),
            workspace_roots: None,
            repository_source_roots: Some(Vec::new()),
        }
    }

    pub fn allows_capability(&self, capability: Capability) -> bool {
        self.capabilities
            .as_ref()
            .is_none_or(|capabilities| capabilities.contains(capability))
    }

    pub fn allows_project(&self, project_id: ProjectId) -> bool {
        self.projects
            .as_ref()
            .is_none_or(|projects| projects.contains(&project_id))
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

    pub fn allows_project_and_session(
        &self,
        project_id: ProjectId,
        session_id: AgentSessionId,
    ) -> bool {
        self.allows_project(project_id) && self.allows_session(session_id)
    }

    pub fn with_workspace_root(mut self, project_id: ProjectId, root: impl Into<PathBuf>) -> Self {
        self.workspace_roots
            .get_or_insert_with(BTreeMap::new)
            .insert(project_id, root.into());
        self
    }

    pub fn allows_workspace_root(&self, project_id: ProjectId, root: &Path) -> bool {
        let Some(roots) = &self.workspace_roots else {
            return true;
        };
        let Some(expected) = roots.get(&project_id) else {
            return false;
        };
        match (std::fs::canonicalize(expected), std::fs::canonicalize(root)) {
            (Ok(expected), Ok(root)) => expected == root,
            _ => false,
        }
    }
}

#[derive(Clone)]
pub struct AuthSession {
    store: Arc<AuthTokenStore>,
    token_id: String,
    digest: [u8; 32],
    scope: AuthorizationScope,
}

impl fmt::Debug for AuthSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthSession")
            .field("token_id", &self.token_id)
            .field("scope", &self.scope)
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
}

impl fmt::Debug for IssuedToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedToken")
            .field("token_id", &self.token_id)
            .field("token", &"<redacted>")
            .field("scope", &self.scope)
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
    revoked: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AuthTokenStore {
    tokens: Arc<Mutex<BTreeMap<String, TokenRecord>>>,
}

impl AuthTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(&self, scope: AuthorizationScope) -> Result<IssuedToken> {
        let token = format!("loom-{}", Uuid::new_v4().simple());
        self.insert(token, scope)
    }

    pub fn insert(
        &self,
        token: impl Into<String>,
        scope: AuthorizationScope,
    ) -> Result<IssuedToken> {
        let token = token.into();
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
                    revoked: false,
                },
            );
        Ok(IssuedToken {
            token_id,
            token,
            scope,
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
