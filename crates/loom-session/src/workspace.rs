use std::{cmp::Reverse, collections::BTreeMap};

use loom_core::{
    LoomError, Result, Timestamp, WorkspaceId, WorkspaceManagerState, WorkspaceRecord,
};

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

    pub fn register(&mut self, workspace: WorkspaceRecord) -> Result<WorkspaceRecord> {
        if workspace.name.trim().is_empty() || workspace.updated_at < workspace.created_at {
            return Err(LoomError::invalid_request(
                "workspace record is inconsistent",
            ));
        }
        match self.workspaces.get(&workspace.id) {
            Some(current) if current.updated_at > workspace.updated_at => Ok(current.clone()),
            Some(current) if current.updated_at == workspace.updated_at => {
                if current.name == workspace.name {
                    Ok(current.clone())
                } else {
                    Err(LoomError::invalid_request(
                        "workspace record conflicts with the registered workspace",
                    ))
                }
            }
            _ => {
                self.workspaces.insert(workspace.id, workspace.clone());
                Ok(workspace)
            }
        }
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
    fn workspace_registration_is_idempotent_and_keeps_the_newest_record() {
        let mut manager = WorkspaceManager::default();
        let workspace = manager.create("Research").unwrap();
        assert_eq!(manager.register(workspace.clone()).unwrap(), workspace);

        let newer = WorkspaceRecord {
            name: "Implementation".to_owned(),
            updated_at: Timestamp::from_unix_millis(
                workspace.updated_at.as_unix_millis().saturating_add(1),
            ),
            ..workspace.clone()
        };
        assert_eq!(manager.register(newer.clone()).unwrap(), newer);
        assert_eq!(manager.register(workspace.clone()).unwrap(), newer);
    }
}
