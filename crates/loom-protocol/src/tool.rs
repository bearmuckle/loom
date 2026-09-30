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

/// What a tool result is worth keeping and showing.
///
/// A tool result has two audiences. The model needs the exact bytes to reason
/// about, so the transcript message always keeps the full output. A person
/// reading the transcript usually needs only to know what happened, so the
/// attempt record — the copy used purely to present the call — does not have to
/// repeat data that already exists in the workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResultKind {
    /// The result exists nowhere else: web results, diffs, or remote API
    /// payloads. Keep and show it.
    Content,
    /// The result restates data already in the workspace: file contents, search
    /// hits, or path lists. Show a pointer and stop keeping the full copy.
    WorkspaceDerived,
    /// The result is a command's stdout and stderr. What matters is that a
    /// command ran and which one, so the output is not kept or shown unless the
    /// command failed.
    Command,
    /// The result is structured server state that is rewritten into a short
    /// human summary before display.
    Structured,
    /// The tool is a control action whose value is the action itself.
    Control,
}

impl ToolResultKind {
    /// Whether the result body carries information the action does not, so it
    /// must be both kept and shown.
    pub const fn keeps_body(self) -> bool {
        matches!(self, Self::Content | Self::Structured | Self::Control)
    }
}

/// Classifies a tool by the nature of its result.
pub fn tool_result_kind(name: &str) -> ToolResultKind {
    match name {
        "read_file" | "search_text" | "glob" | "list_files" => ToolResultKind::WorkspaceDerived,
        "run_command" => ToolResultKind::Command,
        "propose_plan" | "ask_user" => ToolResultKind::Control,
        "delegate_project_task"
        | "delegate_project_code_task"
        | "wait_for_project_children"
        | "control_project_child"
        | "send_project_agent_message"
        | "list_project_message_recipients"
        | "list_project_children"
        | "integrate_project_child" => ToolResultKind::Structured,
        // Includes `web_search`, `apply_patch`, the GitHub tools,
        // `review_project_child`, and any extension or unknown tool: their
        // output is not recoverable from the workspace.
        _ => ToolResultKind::Content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_kinds_group_tools_by_where_their_output_lives() {
        for name in ["read_file", "search_text", "glob", "list_files"] {
            assert_eq!(
                tool_result_kind(name),
                ToolResultKind::WorkspaceDerived,
                "{name} restates the workspace"
            );
            assert!(!tool_result_kind(name).keeps_body());
        }
        assert_eq!(tool_result_kind("run_command"), ToolResultKind::Command);
        assert!(!tool_result_kind("run_command").keeps_body());
        for name in [
            "web_search",
            "apply_patch",
            "github_get_pull_request",
            "review_project_child",
        ] {
            assert_eq!(
                tool_result_kind(name),
                ToolResultKind::Content,
                "{name} has an output that exists nowhere else"
            );
            assert!(tool_result_kind(name).keeps_body());
        }
        for name in ["propose_plan", "ask_user"] {
            assert_eq!(tool_result_kind(name), ToolResultKind::Control, "{name}");
        }
        assert_eq!(
            tool_result_kind("delegate_project_code_task"),
            ToolResultKind::Structured
        );
        assert_eq!(
            tool_result_kind("some_extension_tool"),
            ToolResultKind::Content
        );
    }
}
