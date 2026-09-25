use std::{cmp::Reverse, collections::BTreeMap};

use loom_core::{LoomError, Result, Timestamp, WorkspaceId, WorkspaceRecord};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceManagerState {
    pub workspaces: BTreeMap<WorkspaceId, WorkspaceRecord>,
}

#[derive(Debug, Default)]
pub struct WorkspaceManager {
    workspaces: BTreeMap<WorkspaceId, WorkspaceRecord>,
}

impl WorkspaceManager {
    pub fn from_state(state: WorkspaceManagerState) -> Result<Self> {
        if state.workspaces.iter().any(|(id, workspace)| {
            *id != workspace.id
                || workspace.name.trim().is_empty()
                || workspace.updated_at < workspace.created_at
        }) {
            return Err(LoomError::new(
                loom_core::ErrorCode::MalformedPayload,
                "persisted workspace records are inconsistent",
                false,
            ));
        }
        Ok(Self {
            workspaces: state.workspaces,
        })
    }

    pub fn export_state(&self) -> WorkspaceManagerState {
        WorkspaceManagerState {
            workspaces: self.workspaces.clone(),
        }
    }

    pub fn ensure_legacy(&mut self, id: WorkspaceId, name: impl Into<String>) -> WorkspaceRecord {
        if let Some(workspace) = self.workspaces.get(&id) {
            return workspace.clone();
        }
        let now = Timestamp::now();
        let workspace = WorkspaceRecord {
            id,
            name: name.into(),
            created_at: now,
            updated_at: now,
        };
        self.workspaces.insert(id, workspace.clone());
        workspace
    }

    pub fn create(&mut self, name: impl Into<String>) -> Result<WorkspaceRecord> {
        let name = validate_name(name.into())?;
        let now = Timestamp::now();
        let workspace = WorkspaceRecord {
            id: WorkspaceId::new(),
            name,
            created_at: now,
            updated_at: now,
        };
        self.workspaces.insert(workspace.id, workspace.clone());
        Ok(workspace)
    }

    pub fn get(&self, id: WorkspaceId) -> Result<WorkspaceRecord> {
        self.workspaces
            .get(&id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("workspace", id))
    }

    pub fn list(&self) -> Vec<WorkspaceRecord> {
        let mut workspaces = self.workspaces.values().cloned().collect::<Vec<_>>();
        workspaces.sort_by_key(|workspace| Reverse(workspace.updated_at));
        workspaces
    }

    pub fn rename(&mut self, id: WorkspaceId, name: impl Into<String>) -> Result<WorkspaceRecord> {
        let name = validate_name(name.into())?;
        let workspace = self
            .workspaces
            .get_mut(&id)
            .ok_or_else(|| LoomError::not_found("workspace", id))?;
        if workspace.name == name {
            return Err(LoomError::invalid_request(
                "workspace already has the requested name",
            ));
        }
        workspace.name = name;
        workspace.updated_at = Timestamp::now();
        Ok(workspace.clone())
    }
}

fn validate_name(name: String) -> Result<String> {
    if name.trim().is_empty() {
        return Err(LoomError::invalid_request(
            "workspace name must not be empty",
        ));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_lifecycle_is_independent_of_filesystem_paths() {
        let mut manager = WorkspaceManager::default();
        let workspace = manager.create("Research").unwrap();
        assert_eq!(manager.get(workspace.id).unwrap(), workspace);
        assert_eq!(manager.list(), vec![workspace.clone()]);

        let renamed = manager.rename(workspace.id, "Implementation").unwrap();
        assert_eq!(renamed.name, "Implementation");
        assert!(renamed.updated_at >= workspace.updated_at);
    }

    #[test]
    fn legacy_workspace_records_round_trip() {
        let mut manager = WorkspaceManager::default();
        let id = WorkspaceId::new();
        let workspace = manager.ensure_legacy(id, format!("Workspace {id}"));
        assert_eq!(
            WorkspaceManager::from_state(manager.export_state())
                .unwrap()
                .get(id)
                .unwrap(),
            workspace
        );
    }
}
