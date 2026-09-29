use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use super::*;

/// Owns the per-entity admission locks that serialize conflicting work without
/// serializing unrelated mutations.
#[derive(Default)]
pub(crate) struct AdmissionService {
    sessions: Mutex<BTreeMap<AgentSessionId, Arc<Mutex<()>>>>,
    projects: Mutex<BTreeMap<ProjectId, Arc<Mutex<()>>>>,
    workspace_projects: Mutex<BTreeMap<WorkspaceId, Arc<Mutex<()>>>>,
}

impl AdmissionService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn session(&self, session_id: AgentSessionId) -> Result<Arc<Mutex<()>>> {
        let mut admissions = self.sessions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session admission lock was poisoned",
                true,
            )
        })?;
        Ok(Arc::clone(admissions.entry(session_id).or_default()))
    }

    pub(crate) fn project(&self, project_id: ProjectId) -> Result<Arc<Mutex<()>>> {
        let mut admissions = self.projects.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "project scheduling lock was poisoned",
                true,
            )
        })?;
        Ok(Arc::clone(admissions.entry(project_id).or_default()))
    }

    pub(crate) fn workspace_project(&self, workspace_id: WorkspaceId) -> Result<Arc<Mutex<()>>> {
        let mut admissions = self.workspace_projects.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace project scheduling lock was poisoned",
                true,
            )
        })?;
        Ok(Arc::clone(admissions.entry(workspace_id).or_default()))
    }
}
