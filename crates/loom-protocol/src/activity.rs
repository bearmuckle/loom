use loom_core::{ActivityId, RunId, StepId, Timestamp, ToolCallId};
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
        tool_call_id: ToolCallId,
        operation: FileActivityOperation,
        path: Option<String>,
        result: Option<ToolResult>,
    },
    Search {
        tool_call_id: ToolCallId,
        query: String,
        path: Option<String>,
        result: Option<ToolResult>,
    },
    Command {
        tool_call_id: ToolCallId,
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
    pub parent_id: Option<ActivityId>,
    pub step_id: Option<StepId>,
    pub kind: AgentActivityKind,
    pub status: AgentActivityStatus,
    pub started_at: Timestamp,
    pub completed_at: Option<Timestamp>,
    pub elapsed_ms: Option<u64>,
    pub data: AgentActivityData,
}
