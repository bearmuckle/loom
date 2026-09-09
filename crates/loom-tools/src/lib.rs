use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use loom_core::{ActionKind, ApprovalPolicy, ProjectId, Result, ToolCallId};
use loom_model::{ToolCall, ToolDefinition};
use loom_workspace::{Workspace, WorkspaceEdit};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    ListFiles,
    ReadFile,
    SearchText,
    ApplyPatch,
    RunCommand,
}

impl ToolKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "list_files" => Some(Self::ListFiles),
            "read_file" => Some(Self::ReadFile),
            "search_text" => Some(Self::SearchText),
            "apply_patch" => Some(Self::ApplyPatch),
            "run_command" => Some(Self::RunCommand),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::ListFiles => "list_files",
            Self::ReadFile => "read_file",
            Self::SearchText => "search_text",
            Self::ApplyPatch => "apply_patch",
            Self::RunCommand => "run_command",
        }
    }

    pub const fn requires_approval(self) -> bool {
        matches!(self, Self::ApplyPatch | Self::RunCommand)
    }

    pub const fn action_kind(self) -> ActionKind {
        match self {
            Self::ListFiles | Self::ReadFile | Self::SearchText => ActionKind::Read,
            Self::ApplyPatch => ActionKind::Write,
            Self::RunCommand => ActionKind::Command,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub success: bool,
    pub output: String,
}

impl ToolResult {
    fn success(call: &ToolCall, output: String) -> Self {
        Self {
            tool_call_id: call.id,
            name: call.name.clone(),
            success: true,
            output,
        }
    }

    fn failure(call: &ToolCall, output: impl Into<String>) -> Self {
        Self {
            tool_call_id: call.id,
            name: call.name.clone(),
            success: false,
            output: output.into(),
        }
    }
}

pub struct ToolExecutor {
    root: PathBuf,
    max_output_bytes: usize,
    workspace: Workspace,
}

impl ToolExecutor {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let workspace = Workspace::open(ProjectId::new(), root)?;
        Ok(Self::new_with_workspace(workspace))
    }

    pub fn new_with_workspace(workspace: Workspace) -> Self {
        let root = workspace.root().to_owned();
        Self {
            root,
            max_output_bytes: 64 * 1024,
            workspace,
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn policy_evaluation(
        &self,
        call: &ToolCall,
        policy: &ApprovalPolicy,
    ) -> Option<loom_core::PolicyEvaluation> {
        ToolKind::from_name(&call.name).map(|kind| {
            loom_core::PolicyEvaluation::evaluate(policy, kind.action_kind(), &call.name)
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_root(&self) -> &Path {
        &self.root
    }

    pub fn execute(&self, call: &ToolCall) -> ToolResult {
        let Some(kind) = ToolKind::from_name(&call.name) else {
            return ToolResult::failure(call, format!("unknown tool '{}'", call.name));
        };

        match kind {
            ToolKind::ListFiles => self.list_files(call),
            ToolKind::ReadFile => self.read_file(call),
            ToolKind::SearchText => self.search_text(call),
            ToolKind::ApplyPatch => self.apply_patch(call),
            ToolKind::RunCommand => self.run_command(call),
        }
    }

    fn list_files(&self, call: &ToolCall) -> ToolResult {
        let arguments: ListFilesArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        let relative = arguments.path.as_deref().unwrap_or(".");
        let path = match self.resolve_relative(relative) {
            Ok(path) => path,
            Err(error) => return ToolResult::failure(call, error),
        };
        let mut files = Vec::new();
        if let Err(error) = self.collect_files(&path, Path::new(relative), &mut files) {
            return ToolResult::failure(call, error);
        }
        files.sort();
        ToolResult::success(call, self.limit_output(files.join("\n")))
    }

    fn read_file(&self, call: &ToolCall) -> ToolResult {
        let arguments: ReadFileArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        let path = match self.resolve_relative(&arguments.path) {
            Ok(path) => path,
            Err(error) => return ToolResult::failure(call, error),
        };
        match fs::read_to_string(&path) {
            Ok(contents) => ToolResult::success(call, self.limit_output(contents)),
            Err(error) => ToolResult::failure(
                call,
                format!("could not read '{}': {error}", arguments.path),
            ),
        }
    }

    fn search_text(&self, call: &ToolCall) -> ToolResult {
        let arguments: SearchTextArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        if arguments.query.is_empty() {
            return ToolResult::failure(call, "search query must not be empty");
        }
        let relative = arguments.path.as_deref().unwrap_or(".");
        let path = match self.resolve_relative(relative) {
            Ok(path) => path,
            Err(error) => return ToolResult::failure(call, error),
        };
        let mut matches = Vec::new();
        if let Err(error) =
            self.collect_matches(&path, Path::new(relative), &arguments.query, &mut matches)
        {
            return ToolResult::failure(call, error);
        }
        ToolResult::success(call, self.limit_output(matches.join("\n")))
    }

    fn apply_patch(&self, call: &ToolCall) -> ToolResult {
        let arguments: ApplyPatchArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        let current_file = match self.workspace.read_file(&arguments.path) {
            Ok(file) => file,
            Err(error) if error.code == loom_core::ErrorCode::NotFound => {
                loom_workspace::WorkspaceFile {
                    path: arguments.path.clone(),
                    content: String::new(),
                    revision: String::new(),
                }
            }
            Err(error) => return ToolResult::failure(call, error.message),
        };
        match self.workspace.apply_edit(WorkspaceEdit {
            path: arguments.path.clone(),
            old_text: arguments.old_text,
            new_text: arguments.new_text,
            expected_revision: if current_file.revision.is_empty() {
                None
            } else {
                Some(current_file.revision)
            },
        }) {
            Ok(result) => ToolResult::success(
                call,
                self.limit_output(format!("updated {}\n{}", result.path, result.diff)),
            ),
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    fn run_command(&self, call: &ToolCall) -> ToolResult {
        let arguments: RunCommandArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        if arguments.command.trim().is_empty() {
            return ToolResult::failure(call, "command must not be empty");
        }
        let cwd = match arguments
            .cwd
            .as_deref()
            .map_or_else(|| Ok(self.root.clone()), |cwd| self.resolve_relative(cwd))
        {
            Ok(cwd) => cwd,
            Err(error) => return ToolResult::failure(call, error),
        };
        let output = match Command::new(&arguments.command)
            .args(&arguments.args)
            .current_dir(cwd)
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!("could not start '{}': {error}", arguments.command),
                );
            }
        };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&stderr);
        }
        if text.is_empty() {
            text = format!("command exited with {}", output.status);
        }
        let text = self.limit_output(text);
        if output.status.success() {
            ToolResult::success(call, text)
        } else {
            ToolResult::failure(call, text)
        }
    }

    fn resolve_relative(&self, relative: &str) -> std::result::Result<PathBuf, String> {
        let path = Path::new(relative);
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(format!(
                "path '{}' must stay inside the workspace root",
                relative
            ));
        }
        let mut resolved = self.root.clone();
        for component in path.components() {
            let Component::Normal(component) = component else {
                continue;
            };
            let candidate = resolved.join(component);
            match fs::symlink_metadata(&candidate) {
                Ok(_) => {
                    resolved = fs::canonicalize(&candidate)
                        .map_err(|error| format!("could not resolve '{}': {error}", relative))?;
                    if !resolved.starts_with(&self.root) {
                        return Err(format!(
                            "path '{}' must stay inside the workspace root",
                            relative
                        ));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(format!("workspace path '{}' was not found", relative));
                }
                Err(error) => {
                    return Err(format!("could not resolve '{}': {error}", relative));
                }
            }
        }
        Ok(resolved)
    }

    fn collect_files(
        &self,
        path: &Path,
        relative: &Path,
        files: &mut Vec<String>,
    ) -> std::result::Result<(), String> {
        let entries = fs::read_dir(path)
            .map_err(|error| format!("could not list '{}': {error}", relative.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("could not read directory entry: {error}"))?;
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let child_relative = relative.join(&name);
            let child = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| format!("could not inspect '{}': {error}", child.display()))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                self.collect_files(&child, &child_relative, files)?;
            } else if file_type.is_file() {
                files.push(child_relative.display().to_string());
            }
        }
        Ok(())
    }

    fn collect_matches(
        &self,
        path: &Path,
        relative: &Path,
        query: &str,
        matches: &mut Vec<String>,
    ) -> std::result::Result<(), String> {
        if path.is_dir() {
            let entries = fs::read_dir(path)
                .map_err(|error| format!("could not search '{}': {error}", relative.display()))?;
            for entry in entries {
                let entry =
                    entry.map_err(|error| format!("could not read directory entry: {error}"))?;
                let name = entry.file_name();
                if name == ".git" {
                    continue;
                }
                if entry
                    .file_type()
                    .map_err(|error| format!("could not inspect search entry: {error}"))?
                    .is_symlink()
                {
                    continue;
                }
                self.collect_matches(&entry.path(), &relative.join(name), query, matches)?;
            }
            return Ok(());
        }

        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => return Ok(()),
            Err(error) => {
                return Err(format!("could not read '{}': {error}", relative.display()));
            }
        };
        for (line_number, line) in contents.lines().enumerate() {
            if line.contains(query) {
                matches.push(format!(
                    "{}:{}:{}",
                    relative.display(),
                    line_number + 1,
                    line
                ));
            }
        }
        Ok(())
    }

    fn limit_output(&self, mut output: String) -> String {
        if output.len() > self.max_output_bytes {
            output.truncate(self.max_output_bytes);
            output.push_str("\n[output truncated]");
        }
        output
    }
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(
    call: &ToolCall,
) -> std::result::Result<T, String> {
    serde_json::from_value(call.arguments.clone())
        .map_err(|error| format!("invalid arguments for '{}': {error}", call.name))
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: ToolKind::ListFiles.name().to_owned(),
            description: "List files below a workspace-relative path.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::ReadFile.name().to_owned(),
            description: "Read a UTF-8 text file inside the workspace.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::SearchText.name().to_owned(),
            description: "Search text files for an exact string.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::ApplyPatch.name().to_owned(),
            description: "Apply one exact text replacement to a workspace file.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::RunCommand.name().to_owned(),
            description: "Run a command directly without a shell in the workspace.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "cwd": {"type": "string"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct ListFilesArguments {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadFileArguments {
    path: String,
}

#[derive(Debug, Deserialize)]
struct SearchTextArguments {
    query: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApplyPatchArguments {
    path: String,
    old_text: String,
    new_text: String,
}

#[derive(Debug, Deserialize)]
struct RunCommandArguments {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use loom_core::ProjectId;

    use super::*;

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("loom-tools-{}", ProjectId::new()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("README.md"), "Loom workspace\n").unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() { println!(\"hello\"); }\n",
        )
        .unwrap();
        root
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(),
            name: name.to_owned(),
            arguments,
        }
    }

    #[test]
    fn executes_workspace_tools_with_scoped_paths() {
        let root = workspace();
        let executor = ToolExecutor::new(&root).unwrap();

        let files = executor.execute(&call("list_files", serde_json::json!({})));
        assert!(files.success);
        assert!(files.output.contains("README.md"));
        assert!(files.output.contains("src/main.rs"));

        let read = executor.execute(&call("read_file", serde_json::json!({"path": "README.md"})));
        assert_eq!(read.output, "Loom workspace\n");

        let search = executor.execute(&call(
            "search_text",
            serde_json::json!({"query": "println"}),
        ));
        assert!(search.success);
        assert!(search.output.contains("src/main.rs:1"));

        let patch = executor.execute(&call(
            "apply_patch",
            serde_json::json!({
                "path": "README.md",
                "old_text": "Loom workspace",
                "new_text": "Loom M1 workspace"
            }),
        ));
        assert!(patch.success);
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "Loom M1 workspace\n"
        );

        let command = if cfg!(windows) {
            ("cmd", vec!["/C", "echo", "ok"])
        } else {
            ("printf", vec!["ok"])
        };
        let command_result = executor.execute(&call(
            "run_command",
            serde_json::json!({"command": command.0, "args": command.1}),
        ));
        assert!(command_result.success);
        assert!(command_result.output.contains("ok"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let root = workspace();
        let executor = ToolExecutor::new(&root).unwrap();

        let result = executor.execute(&call(
            "read_file",
            serde_json::json!({"path": "../secrets.txt"}),
        ));

        assert!(!result.success);
        assert!(result.output.contains("inside the workspace root"));
        fs::remove_dir_all(root).unwrap();
    }
}
