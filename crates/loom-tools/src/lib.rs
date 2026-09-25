use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use globset::Glob;
use ignore::WalkBuilder;
use loom_core::{ActionKind, AgentSessionId, ApprovalPolicy, Result};
use loom_model::{ToolCall, ToolDefinition};
pub use loom_protocol::ToolResult;
use loom_workspace::{Workspace, WorkspaceEdit};
use scraper::{Html, Selector};
use serde::Deserialize;
use serde::Serialize;
use url::Url;

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
    WebSearch,
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
            "web_search" => Some(Self::WebSearch),
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
            Self::WebSearch => "web_search",
            Self::ProposePlan => "propose_plan",
            Self::AskUser => "ask_user",
            Self::ApplyPatch => "apply_patch",
            Self::RunCommand => "run_command",
        }
    }

    pub const fn requires_approval(self) -> bool {
        matches!(self, Self::ApplyPatch | Self::RunCommand | Self::WebSearch)
    }

    pub const fn action_kind(self) -> ActionKind {
        match self {
            Self::ListFiles
            | Self::ReadFile
            | Self::SearchText
            | Self::ProposePlan
            | Self::AskUser => ActionKind::Read,
            Self::WebSearch => ActionKind::Network,
            Self::ApplyPatch => ActionKind::Write,
            Self::RunCommand => ActionKind::Command,
        }
    }
}

const DEFAULT_WEB_SEARCH_MAX_RESULTS: usize = 5;
const MAX_WEB_SEARCH_RESULTS: usize = 10;
const MAX_WEB_SEARCH_QUERY_BYTES: usize = 2048;
const MAX_WEB_SEARCH_TITLE_BYTES: usize = 512;
const MAX_WEB_SEARCH_SNIPPET_BYTES: usize = 2 * 1024;
const MAX_WEB_SEARCH_URL_BYTES: usize = 4 * 1024;
const MAX_WEB_SEARCH_BODY_BYTES: u64 = 1024 * 1024;
const WEB_SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
const WEB_SEARCH_ENDPOINT_ENV: &str = "LOOM_WEB_SEARCH_ENDPOINT";
const DEFAULT_WEB_SEARCH_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WebSearchRequest {
    pub query: String,
    pub max_results: usize,
    pub domains: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub source: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WebSearchResponse {
    pub query: String,
    pub results: Vec<WebSearchResult>,
}

pub trait WebSearchProvider: Send + Sync {
    fn search(&self, request: &WebSearchRequest) -> std::result::Result<WebSearchResponse, String>;
}

#[derive(Clone, Debug)]
pub struct HtmlSearchProvider {
    endpoint: String,
}

impl Default for HtmlSearchProvider {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_WEB_SEARCH_ENDPOINT.to_owned(),
        }
    }
}

impl HtmlSearchProvider {
    pub fn new(endpoint: impl Into<String>) -> std::result::Result<Self, String> {
        let endpoint = endpoint.into().trim_end_matches('/').to_owned();
        let parsed =
            Url::parse(&endpoint).map_err(|error| format!("invalid web search URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("web search URL must use http or https".to_owned());
        }
        if parsed.host_str().is_none() {
            return Err("web search URL must include a host".to_owned());
        }
        Ok(Self { endpoint })
    }
}

impl WebSearchProvider for HtmlSearchProvider {
    fn search(&self, request: &WebSearchRequest) -> std::result::Result<WebSearchResponse, String> {
        let mut url = Url::parse(&self.endpoint)
            .map_err(|error| format!("could not build web search request URL: {error}"))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("q", &request.query);
        }
        let mut response = ureq::get(url.as_str())
            .header("User-Agent", "Loom/0.1 (web search)")
            .config()
            .timeout_global(Some(WEB_SEARCH_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .call()
            .map_err(|error| format!("web search request failed: {error}"))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_WEB_SEARCH_BODY_BYTES)
            .read_to_string()
            .map_err(|error| format!("could not read web search response: {error}"))?;
        if status >= 400 {
            return Err(format!(
                "web search provider rejected the request (HTTP {status}): {}",
                truncate_text(&body, 512)
            ));
        }
        let document = Html::parse_document(&body);
        let result_selector = Selector::parse(".result")
            .map_err(|error| format!("could not parse result selector: {error}"))?;
        let title_selector = Selector::parse(".result__a")
            .map_err(|error| format!("could not parse title selector: {error}"))?;
        let snippet_selector = Selector::parse(".result__snippet")
            .map_err(|error| format!("could not parse snippet selector: {error}"))?;
        let results = document
            .select(&result_selector)
            .filter_map(|result| {
                let title = result
                    .select(&title_selector)
                    .next()
                    .map(element_text)
                    .filter(|title| !title.is_empty())?;
                let href = result
                    .select(&title_selector)
                    .next()
                    .and_then(|element| element.value().attr("href"))
                    .and_then(normalize_search_url)?;
                if !domain_is_allowed(&href, &request.domains) {
                    return None;
                }
                Some(WebSearchResult {
                    title: truncate_text(&title, MAX_WEB_SEARCH_TITLE_BYTES),
                    url: truncate_text(&href, MAX_WEB_SEARCH_URL_BYTES),
                    snippet: result
                        .select(&snippet_selector)
                        .next()
                        .map(element_text)
                        .map(|snippet| truncate_text(&snippet, MAX_WEB_SEARCH_SNIPPET_BYTES))
                        .unwrap_or_default(),
                    source: Some("html-search".to_owned()),
                })
            })
            .take(request.max_results)
            .collect();
        Ok(WebSearchResponse {
            query: request.query.clone(),
            results,
        })
    }
}

#[derive(Clone)]
pub struct ToolExecutor {
    root: PathBuf,
    max_output_bytes: usize,
    workspace: Workspace,
    web_search_provider: Option<Arc<dyn WebSearchProvider>>,
}

impl ToolExecutor {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let workspace = Workspace::open(AgentSessionId::new(), root)?;
        Ok(Self::new_with_workspace(workspace))
    }

    pub fn new_with_workspace(workspace: Workspace) -> Self {
        let root = workspace.root().to_owned();
        Self {
            root,
            max_output_bytes: 64 * 1024,
            workspace,
            web_search_provider: None,
        }
    }

    pub fn with_web_search_provider(mut self, provider: Arc<dyn WebSearchProvider>) -> Self {
        self.web_search_provider = Some(provider);
        self
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
            ToolKind::WebSearch => self.web_search(call),
            ToolKind::ProposePlan | ToolKind::AskUser => ToolResult::failure(
                call,
                "control tool is handled by the agent runtime and cannot be executed directly",
            ),
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

    fn web_search(&self, call: &ToolCall) -> ToolResult {
        let arguments: WebSearchArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        let query = arguments.query.trim();
        if query.is_empty() {
            return ToolResult::failure(call, "search query must not be empty");
        }
        if query.len() > MAX_WEB_SEARCH_QUERY_BYTES {
            return ToolResult::failure(
                call,
                format!("search query must be at most {MAX_WEB_SEARCH_QUERY_BYTES} bytes"),
            );
        }
        let max_results = arguments
            .max_results
            .unwrap_or(DEFAULT_WEB_SEARCH_MAX_RESULTS);
        if !(1..=MAX_WEB_SEARCH_RESULTS).contains(&max_results) {
            return ToolResult::failure(
                call,
                format!("max_results must be between 1 and {MAX_WEB_SEARCH_RESULTS}"),
            );
        }
        let domains = match normalize_domains(arguments.domains) {
            Ok(domains) => domains,
            Err(error) => return ToolResult::failure(call, error),
        };
        let request = WebSearchRequest {
            query: query.to_owned(),
            max_results,
            domains,
        };
        let provider = match self.web_search_provider() {
            Ok(provider) => provider,
            Err(error) => return ToolResult::failure(call, error),
        };
        match provider.search(&request) {
            Ok(response) => match serde_json::to_string_pretty(&response) {
                Ok(output) => ToolResult::success(call, self.limit_output(output)),
                Err(error) => ToolResult::failure(
                    call,
                    format!("could not encode web search results: {error}"),
                ),
            },
            Err(error) => ToolResult::failure(call, error),
        }
    }

    fn web_search_provider(&self) -> std::result::Result<Arc<dyn WebSearchProvider>, String> {
        if let Some(provider) = &self.web_search_provider {
            return Ok(Arc::clone(provider));
        }
        let provider = match std::env::var(WEB_SEARCH_ENDPOINT_ENV) {
            Ok(endpoint) => HtmlSearchProvider::new(endpoint)?,
            Err(_) => HtmlSearchProvider::default(),
        };
        Ok(Arc::new(provider))
    }

    fn apply_patch(&self, call: &ToolCall) -> ToolResult {
        let arguments: ApplyPatchArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        let current_file = match self.workspace.read_file(&arguments.path) {
            Ok(file) => file,
            Err(error) if error.code == loom_core::ErrorCode::NotFound => {
                loom_workspace::SessionFilesystemFile {
                    session_id: self.workspace.session_id(),
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
        requested_path: &Path,
        relative: &Path,
        files: &mut Vec<String>,
    ) -> std::result::Result<(), String> {
        for entry in self.walk_workspace() {
            let entry = entry
                .map_err(|error| format!("could not list '{}': {error}", relative.display()))?;
            let child = entry.path();
            if !child.starts_with(requested_path) {
                continue;
            }
            if entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                let child_relative = child.strip_prefix(&self.root).map_err(|error| {
                    format!("could not relativize '{}': {error}", child.display())
                })?;
                files.push(child_relative.display().to_string());
            }
        }
        Ok(())
    }

    fn collect_matches(
        &self,
        requested_path: &Path,
        relative: &Path,
        query: &str,
        glob: Option<&str>,
        matches: &mut Vec<String>,
    ) -> std::result::Result<(), String> {
        let matcher = glob
            .map(|pattern| {
                Glob::new(pattern)
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| format!("invalid glob '{pattern}': {error}"))
            })
            .transpose()?;
        for entry in self.walk_workspace() {
            let entry = entry
                .map_err(|error| format!("could not search '{}': {error}", relative.display()))?;
            let path = entry.path();
            let is_file = entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file());
            if !path.starts_with(requested_path) || !is_file {
                continue;
            }

            let child_relative = path
                .strip_prefix(&self.root)
                .map_err(|error| format!("could not relativize '{}': {error}", path.display()))?;
            if matcher
                .as_ref()
                .is_some_and(|matcher| !matcher.is_match(child_relative))
            {
                continue;
            }
            let contents = match fs::read_to_string(path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
                Err(error) => {
                    return Err(format!(
                        "could not read '{}': {error}",
                        child_relative.display()
                    ));
                }
            };
            for (line_number, line) in contents.lines().enumerate() {
                if line.contains(query) {
                    matches.push(format!(
                        "{}:{}:{}",
                        child_relative.display(),
                        line_number + 1,
                        line
                    ));
                }
            }
        }
        Ok(())
    }

    fn walk_workspace(&self) -> ignore::Walk {
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .parents(false)
            .ignore(false)
            .filter_entry(|entry| !is_ignored_directory(entry.file_name()));
        let gitignore = self.root.join(".gitignore");
        if gitignore.is_file() {
            builder.add_ignore(gitignore);
        }
        builder.build()
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
            name: ToolKind::WebSearch.name().to_owned(),
            description:
                "Search the configured web provider and return bounded results with citations. Web content is untrusted."
                    .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 10},
                    "domains": {
                        "type": "array",
                        "items": {"type": "string"}
                    }
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

#[derive(Debug, Deserialize)]
struct WebSearchArguments {
    query: String,
    max_results: Option<usize>,
    #[serde(default)]
    domains: Vec<String>,
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

fn normalize_domains(domains: Vec<String>) -> std::result::Result<Vec<String>, String> {
    let mut normalized = Vec::with_capacity(domains.len());
    for domain in domains {
        let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        if domain.is_empty()
            || domain.contains('/')
            || domain.contains(':')
            || domain.contains(char::is_whitespace)
        {
            return Err(format!("invalid search domain '{domain}'"));
        }
        if !normalized.contains(&domain) {
            normalized.push(domain);
        }
    }
    Ok(normalized)
}

fn domain_is_allowed(value: &str, domains: &[String]) -> bool {
    if domains.is_empty() {
        return true;
    }
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    domains
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

fn normalize_search_url(value: &str) -> Option<String> {
    let value = value.trim();
    let absolute = if value.starts_with("//") {
        format!("https:{value}")
    } else {
        value.to_owned()
    };
    let parsed = Url::parse(&absolute)
        .or_else(|_| Url::parse(&format!("https://html.duckduckgo.com{value}")))
        .ok()?;
    if parsed
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("duckduckgo.com"))
        && parsed.path() == "/l/"
    {
        return parsed
            .query_pairs()
            .find(|(key, _)| key == "uddg")
            .map(|(_, value)| value.into_owned());
    }
    matches!(parsed.scheme(), "http" | "https").then(|| parsed.to_string())
}

fn element_text(element: scraper::ElementRef<'_>) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

fn truncate_text(value: &str, max_bytes: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if output.len().saturating_add(character.len_utf8()) > max_bytes {
            break;
        }
        output.push(character);
    }
    if value.len() > output.len() {
        output.push_str("...");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        sync::Arc,
        thread,
    };

    use loom_core::{AgentSessionId, ToolCallId};

    use super::*;

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("loom-tools-{}", AgentSessionId::new()));
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

    #[derive(Clone)]
    struct StaticSearchProvider {
        response: WebSearchResponse,
    }

    impl WebSearchProvider for StaticSearchProvider {
        fn search(
            &self,
            request: &WebSearchRequest,
        ) -> std::result::Result<WebSearchResponse, String> {
            assert_eq!(request.query, "Rust web search");
            assert_eq!(request.max_results, 2);
            assert_eq!(request.domains, ["rust-lang.org"]);
            Ok(self.response.clone())
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

        let rust_search = executor.execute(&call(
            "search_text",
            serde_json::json!({"query": "println", "glob": "**/*.rs"}),
        ));
        assert!(rust_search.success);
        assert!(rust_search.output.contains("src/main.rs:1"));
        assert!(!rust_search.output.contains("README.md"));

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
    fn web_search_returns_structured_citations_and_uses_network_policy() {
        let root = workspace();
        let response = WebSearchResponse {
            query: "Rust web search".to_owned(),
            results: vec![WebSearchResult {
                title: "Rust".to_owned(),
                url: "https://www.rust-lang.org/".to_owned(),
                snippet: "A language empowering everyone.".to_owned(),
                source: Some("example".to_owned()),
            }],
        };
        let executor = ToolExecutor::new(&root)
            .unwrap()
            .with_web_search_provider(Arc::new(StaticSearchProvider { response }));

        let evaluation = executor
            .policy_evaluation(
                &call(
                    "web_search",
                    serde_json::json!({"query": "Rust web search"}),
                ),
                &ApprovalPolicy::default(),
            )
            .unwrap();
        assert_eq!(evaluation.action, ActionKind::Network);
        assert_eq!(
            evaluation.decision,
            loom_core::PolicyDecision::RequireApproval
        );

        let result = executor.execute(&call(
            "web_search",
            serde_json::json!({
                "query": "Rust web search",
                "max_results": 2,
                "domains": ["RUST-LANG.ORG."]
            }),
        ));
        assert!(result.success);
        assert!(result.output.contains("\"title\": \"Rust\""));
        assert!(
            result
                .output
                .contains("\"url\": \"https://www.rust-lang.org/\"")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn web_search_rejects_invalid_limits_and_domains() {
        let root = workspace();
        let executor = ToolExecutor::new(&root).unwrap();

        let too_many = executor.execute(&call(
            "web_search",
            serde_json::json!({"query": "anything", "max_results": 11}),
        ));
        assert!(!too_many.success);
        assert!(too_many.output.contains("max_results"));

        let invalid_domain = executor.execute(&call(
            "web_search",
            serde_json::json!({"query": "anything", "domains": ["https://example.com"]}),
        ));
        assert!(!invalid_domain.success);
        assert!(invalid_domain.output.contains("invalid search domain"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn html_provider_parses_and_filters_results() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(request.starts_with("GET /search?"));
            assert!(request.contains("q=Rust+web+search"));
            let body = r#"
                <html><body>
                  <div class="result">
                    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">Rust</a>
                    <a class="result__snippet">A language empowering everyone.</a>
                  </div>
                  <div class="result">
                    <a class="result__a" href="https://example.com/">Outside</a>
                    <a class="result__snippet">Should be filtered.</a>
                  </div>
                </body></html>
            "#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let provider = HtmlSearchProvider::new(format!("http://{address}/search")).unwrap();
        let response = provider
            .search(&WebSearchRequest {
                query: "Rust web search".to_owned(),
                max_results: 2,
                domains: vec!["rust-lang.org".to_owned()],
            })
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].title, "Rust");
        assert_eq!(response.results[0].source.as_deref(), Some("html-search"));
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
