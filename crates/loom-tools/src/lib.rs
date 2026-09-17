use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use loom_core::{ActionKind, ApprovalPolicy, ProjectId, Result};
use loom_model::{ToolCall, ToolDefinition};
pub use loom_protocol::ToolResult;
use loom_workspace::{Workspace, WorkspaceEdit};
use serde::Deserialize;

fn is_ignored_directory(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | "target" | "node_modules" | ".venv" | "vendor")
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    ListFiles,
    ReadFile,
    SearchText,
    ProposePlan,
    AskUser,
    ApplyPatch,
    RunCommand,
}

impl ToolKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "list_files" => Some(Self::ListFiles),
            "read_file" => Some(Self::ReadFile),
            "search_text" => Some(Self::SearchText),
            "propose_plan" => Some(Self::ProposePlan),
            "ask_user" => Some(Self::AskUser),
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
            Self::ProposePlan => "propose_plan",
            Self::AskUser => "ask_user",
            Self::ApplyPatch => "apply_patch",
            Self::RunCommand => "run_command",
        }
    }

    pub const fn requires_approval(self) -> bool {
        matches!(self, Self::ApplyPatch | Self::RunCommand)
    }

    pub const fn action_kind(self) -> ActionKind {
        match self {
            Self::ListFiles
            | Self::ReadFile
            | Self::SearchText
            | Self::ProposePlan
            | Self::AskUser => ActionKind::Read,
            Self::ApplyPatch => ActionKind::Write,
            Self::RunCommand => ActionKind::Command,
        }
    }
}

#[derive(Clone)]
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
            ToolKind::ProposePlan | ToolKind::AskUser => ToolResult::failure(
                call,
                "control tool is handled by the agent runtime and cannot be executed directly",
            ),
            ToolKind::ApplyPatch => self.apply_patch(call),
            ToolKind::RunCommand => self.run_command(call),
        }
    }

    /// Executes independent tool calls concurrently and returns results in request order.
    pub fn execute_many(&self, calls: &[ToolCall]) -> Vec<ToolResult> {
        std::thread::scope(|scope| {
            let workers = calls
                .iter()
                .map(|call| scope.spawn(move || self.execute(call)))
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("tool worker panicked"))
                .collect()
        })
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
            Ok(contents) => {
                if arguments.line_start.is_none() && arguments.line_end.is_none() {
                    return ToolResult::success(call, self.limit_output(contents));
                }
                let start = arguments.line_start.unwrap_or(1).max(1) as usize;
                let end = arguments.line_end.unwrap_or(u64::MAX) as usize;
                let ranged = contents
                    .lines()
                    .enumerate()
                    .filter(|(index, _)| *index + 1 >= start && *index < end)
                    .map(|(index, line)| format!("{}:{}", index + 1, line))
                    .collect::<Vec<_>>()
                    .join("\n");
                ToolResult::success(call, self.limit_output(ranged))
            }
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
        if let Err(error) = self.collect_matches(
            &path,
            Path::new(relative),
            &arguments.query,
            arguments.glob.as_deref(),
            &mut matches,
        ) {
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
        if path.is_absolute() {
            let resolved = fs::canonicalize(path)
                .map_err(|error| format!("could not resolve '{}': {error}", relative))?;
            if !resolved.starts_with(&self.root) {
                return Err(format!(
                    "path '{}' must stay inside the workspace root",
                    relative
                ));
            }
            return Ok(resolved);
        }
        if path
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
            let child_relative = relative.join(&name);
            if is_ignored_directory(&name) || self.is_ignored(&child_relative) {
                continue;
            }
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
        glob: Option<&str>,
        matches: &mut Vec<String>,
    ) -> std::result::Result<(), String> {
        if path.is_dir() {
            let entries = fs::read_dir(path)
                .map_err(|error| format!("could not search '{}': {error}", relative.display()))?;
            for entry in entries {
                let entry =
                    entry.map_err(|error| format!("could not read directory entry: {error}"))?;
                let name = entry.file_name();
                let child_relative = relative.join(&name);
                if is_ignored_directory(&name) || self.is_ignored(&child_relative) {
                    continue;
                }
                if entry
                    .file_type()
                    .map_err(|error| format!("could not inspect search entry: {error}"))?
                    .is_symlink()
                {
                    continue;
                }
                self.collect_matches(&entry.path(), &child_relative, query, glob, matches)?;
            }
            return Ok(());
        }

        if glob.is_some_and(|pattern| !glob_matches(pattern, relative)) {
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

    fn is_ignored(&self, relative: &Path) -> bool {
        let Ok(contents) = fs::read_to_string(self.root.join(".gitignore")) else {
            return false;
        };
        contents
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                (!line.is_empty() && !line.starts_with('#')).then_some(line.trim_end_matches('/'))
            })
            .any(|pattern| {
                let path = relative.to_string_lossy();
                let pattern = pattern.trim_start_matches('/');
                path == pattern
                    || path.starts_with(&format!("{pattern}/"))
                    || (!pattern.contains('/')
                        && relative
                            .components()
                            .any(|component| component.as_os_str() == pattern))
            })
    }

    fn limit_output(&self, mut output: String) -> String {
        if output.len() > self.max_output_bytes {
            let mut boundary = self.max_output_bytes;
            while boundary > 0 && !output.is_char_boundary(boundary) {
                boundary -= 1;
            }
            output.truncate(boundary);
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
                "properties": {
                    "path": {"type": "string"},
                    "line_start": {"type": "integer", "minimum": 1},
                    "line_end": {"type": "integer", "minimum": 1}
                },
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
                    "path": {"type": "string"},
                    "glob": {"type": "string"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::ProposePlan.name().to_owned(),
            description: "Propose an ordered plan before changing the workspace.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": {"type": "string"}
                    }
                },
                "required": ["steps"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::AskUser.name().to_owned(),
            description: "Pause the run and ask the user for information needed to continue."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"prompt": {"type": "string"}},
                "required": ["prompt"],
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
    #[serde(default)]
    line_start: Option<u64>,
    #[serde(default)]
    line_end: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SearchTextArguments {
    query: String,
    path: Option<String>,
    glob: Option<String>,
}

fn glob_matches(pattern: &str, path: &Path) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.to_string_lossy().replace('\\', "/");
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return false;
    };
    if !path.starts_with(first) {
        return false;
    }
    let mut offset = first.len();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        let Some(found) = path[offset..].find(part) else {
            return false;
        };
        offset += found + part.len();
    }
    pattern.ends_with('*') || offset == path.len()
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

    use loom_core::{ProjectId, ToolCallId};

    use super::*;

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("loom-tools-{}", ProjectId::new()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("README.md"), "Loom workspace\n").unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join("ignored/generated.txt"), "hidden\n").unwrap();
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
        assert!(!files.output.contains("ignored/generated.txt"));

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
        fs::write(root.join("README.md"), "one\ntwo\nthree\n").unwrap();
        let ranged = executor.execute(&call(
            "read_file",
            serde_json::json!({"path": "README.md", "line_start": 2, "line_end": 3}),
        ));
        assert_eq!(ranged.output, "2:two\n3:three");

        let command = if cfg!(windows) {
            ("cmd", vec!["/C", "echo", "ok"])
        } else {
            ("printf", vec!["ok"])
        };
        let command_result = executor.execute(&call(
            "run_command",
            serde_json::json!({
                "command": command.0,
                "args": command.1,
                "cwd": root.display().to_string()
            }),
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

    #[test]
    fn truncates_unicode_without_panicking() {
        let root = workspace();
        let executor = ToolExecutor::new(&root).unwrap();
        let output = executor.limit_output(format!("{}x", "😀".repeat(20_000)));
        assert!(output.ends_with("[output truncated]"));
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}
