use serde::{Deserialize, Serialize};

use crate::{Timestamp, WorkspaceId};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    pub name: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
