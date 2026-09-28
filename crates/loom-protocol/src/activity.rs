use loom_core::{ActivityId, RunId, StepId, Timestamp};
use loom_model::{ModelId, ToolCall};
use serde::{Deserialize, Serialize};

use crate::ToolResult;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivityKind {
    ModelTurn,
    ToolCall,
    File,
    Search,
    Command,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivityStatus {
    Started,
    Completed,
    Failed,
    AwaitingApproval,
    AwaitingInput,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileActivityOperation {
    List,
    Read,
    Write,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentActivityData {
    ModelTurn {
        model: ModelId,
    },
    ToolCall {
        call: ToolCall,
        result: Option<ToolResult>,
    },
    File {
        call: ToolCall,
        operation: FileActivityOperation,
        path: Option<String>,
        result: Option<ToolResult>,
    },
    Search {
        call: ToolCall,
        query: String,
        path: Option<String>,
        result: Option<ToolResult>,
    },
    Command {
        call: ToolCall,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        result: Option<ToolResult>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentActivityRecord {
    pub id: ActivityId,
    pub run_id: RunId,
    /// Stable run-wide order assigned when this activity first starts.
    /// Status updates keep the original value.
    #[serde(default)]
    pub timeline_ordinal: u64,
    pub parent_id: Option<ActivityId>,
    pub step_id: Option<StepId>,
    pub kind: AgentActivityKind,
    pub status: AgentActivityStatus,
    pub started_at: Timestamp,
    pub completed_at: Option<Timestamp>,
    pub elapsed_ms: Option<u64>,
    pub data: AgentActivityData,
}
