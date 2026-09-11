use loom_core::ToolCallId;
use loom_model::ToolCall;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub success: bool,
    pub output: String,
}

impl ToolResult {
    pub fn success(call: &ToolCall, output: String) -> Self {
        Self {
            tool_call_id: call.id,
            name: call.name.clone(),
            success: true,
            output,
        }
    }

    pub fn failure(call: &ToolCall, output: impl Into<String>) -> Self {
        Self {
            tool_call_id: call.id,
            name: call.name.clone(),
            success: false,
            output: output.into(),
        }
    }
}
