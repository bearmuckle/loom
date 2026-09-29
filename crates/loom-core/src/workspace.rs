use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Timestamp, WorkspaceId};

/// Serializable snapshot of the workspace manager, owned by the neutral domain
/// layer so storage does not depend on the session crate.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceManagerState {
    pub workspaces: BTreeMap<WorkspaceId, WorkspaceRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    pub name: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
