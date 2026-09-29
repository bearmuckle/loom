use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use globset::Glob;
use loom_core::{ActionKind, AgentSessionId, ApprovalPolicy, PolicyEvaluation, Result};
use loom_model::{ToolCall, ToolDefinition};
pub use loom_protocol::ToolResult;
use loom_workspace::{Workspace, WorkspaceEdit};
use regex::RegexBuilder;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde::Serialize;

fn run_async<T>(
    future: impl std::future::Future<Output = std::result::Result<T, String>>,
) -> std::result::Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the HTTP runtime: {error}"))?
        .block_on(future)
}
use url::Url;

/// A server-owned set of tools attached to one agent session. Extensions can
/// expose bounded operations without giving the agent arbitrary backend access.
pub trait ToolExtension: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn action_kind(&self, call: &ToolCall) -> Option<ActionKind>;
    fn execute(&self, call: &ToolCall) -> ToolResult;

    /// Identifies a tool call that must be parked by the agent runtime instead
    /// of executed synchronously. The returned key is persisted as part of the
    /// runtime continuation and can be completed by the owning server later.
    /// Implementations must keep this check side-effect free; durable state is
    /// written with the run checkpoint after the runtime parks the call.
    fn prepare_deferred(&self, _call: &ToolCall) -> Option<String> {
        None
    }

    /// Explains why this agent cannot report completion yet. Project-aware
    /// extensions use this to keep a manager active while children or reviewed
    /// code results still need attention.
    fn completion_blocker(&self) -> Option<String> {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    ListFiles,
    ReadFile,
    SearchText,
    Glob,
    WebSearch,
    ProposePlan,
    AskUser,
    ApplyPatch,
    RunCommand,
    GitHubListPullRequests,
    GitHubGetPullRequest,
    GitHubCreatePullRequest,
}

impl ToolKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "list_files" => Some(Self::ListFiles),
            "read_file" => Some(Self::ReadFile),
            "search_text" => Some(Self::SearchText),
            "glob" => Some(Self::Glob),
            "web_search" => Some(Self::WebSearch),
            "propose_plan" => Some(Self::ProposePlan),
            "ask_user" => Some(Self::AskUser),
            "apply_patch" => Some(Self::ApplyPatch),
            "run_command" => Some(Self::RunCommand),
            "github_list_pull_requests" => Some(Self::GitHubListPullRequests),
            "github_get_pull_request" => Some(Self::GitHubGetPullRequest),
            "github_create_pull_request" => Some(Self::GitHubCreatePullRequest),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::ListFiles => "list_files",
            Self::ReadFile => "read_file",
            Self::SearchText => "search_text",
            Self::Glob => "glob",
            Self::WebSearch => "web_search",
            Self::ProposePlan => "propose_plan",
            Self::AskUser => "ask_user",
            Self::ApplyPatch => "apply_patch",
            Self::RunCommand => "run_command",
            Self::GitHubListPullRequests => "github_list_pull_requests",
            Self::GitHubGetPullRequest => "github_get_pull_request",
            Self::GitHubCreatePullRequest => "github_create_pull_request",
        }
    }

    pub const fn requires_approval(self) -> bool {
        matches!(
            self,
            Self::ApplyPatch
                | Self::RunCommand
                | Self::WebSearch
                | Self::GitHubListPullRequests
                | Self::GitHubGetPullRequest
                | Self::GitHubCreatePullRequest
        )
    }

    pub const fn action_kind(self) -> ActionKind {
        match self {
            Self::ListFiles
            | Self::ReadFile
            | Self::SearchText
            | Self::Glob
            | Self::ProposePlan
            | Self::AskUser => ActionKind::Read,
            Self::WebSearch | Self::GitHubListPullRequests | Self::GitHubGetPullRequest => {
                ActionKind::Network
            }
            Self::GitHubCreatePullRequest => ActionKind::Write,
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

/// Default tool result budget. Keeping both ends of an oversized result matters
/// for build logs, where the failing summary is usually at the tail.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Upper bound for the truncation marker so the final result cannot exceed the
/// configured budget.
const TRUNCATION_MARKER_RESERVE: usize = 64;

const DEFAULT_SEARCH_MAX_RESULTS: usize = 100;
const MAX_SEARCH_MAX_RESULTS: usize = 1_000;
const DEFAULT_SEARCH_CONTEXT_LINES: usize = 0;
const MAX_SEARCH_CONTEXT_LINES: usize = 5;

const DEFAULT_LIST_MAX_ENTRIES: usize = 1_000;
const MAX_LIST_MAX_ENTRIES: usize = 10_000;
const MAX_LIST_DEPTH: usize = 32;

const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
const MIN_COMMAND_TIMEOUT_MS: u64 = 100;
const MAX_COMMAND_TIMEOUT_MS: u64 = 600_000;
/// Poll interval while waiting for a child process to exit.
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);

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
        let (status, body) = run_async(async {
            let client = reqwest::Client::builder()
                .timeout(WEB_SEARCH_TIMEOUT)
                .build()
                .map_err(|error| format!("web search request failed: {error}"))?;
            let response = client
                .get(url.as_str())
                .header("User-Agent", "Loom/0.1 (web search)")
                .send()
                .await
                .map_err(|error| format!("web search request failed: {error}"))?;
            let status = response.status().as_u16();
            let text = response
                .text()
                .await
                .map_err(|error| format!("could not read web search response: {error}"))?;
            let text = text
                .chars()
                .take(MAX_WEB_SEARCH_BODY_BYTES as usize)
                .collect::<String>();
            Ok::<_, String>((status, text))
        })?;
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
    github_token: Option<String>,
    extension: Option<Arc<dyn ToolExtension>>,
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
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            workspace,
            web_search_provider: None,
            github_token: None,
            extension: None,
        }
    }

    pub fn with_web_search_provider(mut self, provider: Arc<dyn WebSearchProvider>) -> Self {
        self.web_search_provider = Some(provider);
        self
    }

    pub fn with_github_token(mut self, token: Option<String>) -> Self {
        self.github_token = token.filter(|token| !token.trim().is_empty());
        self
    }

    pub fn with_extension(mut self, extension: Arc<dyn ToolExtension>) -> Self {
        self.extension = Some(extension);
        self
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = tool_definitions();
        if let Some(extension) = &self.extension {
            let mut names = definitions
                .iter()
                .map(|definition| definition.name.clone())
                .collect::<BTreeSet<_>>();
            definitions.extend(
                extension
                    .definitions()
                    .into_iter()
                    .filter(|definition| names.insert(definition.name.clone())),
            );
        }
        definitions
    }

    pub fn action_kind(&self, call: &ToolCall) -> Option<ActionKind> {
        ToolKind::from_name(&call.name)
            .map(ToolKind::action_kind)
            .or_else(|| {
                self.extension
                    .as_ref()
                    .and_then(|extension| extension.action_kind(call))
            })
    }

    /// Returns the stable continuation key for an extension tool that must
    /// suspend the current agent run. Built-in tools are always synchronous.
    pub fn prepare_deferred(&self, call: &ToolCall) -> Option<String> {
        if ToolKind::from_name(&call.name).is_some() {
            return None;
        }
        self.extension
            .as_ref()
            .filter(|extension| extension.action_kind(call).is_some())
            .and_then(|extension| extension.prepare_deferred(call))
    }

    /// Returns a server-owned completion blocker supplied by the active tool
    /// extension, if any.
    pub fn completion_blocker(&self) -> Option<String> {
        self.extension
            .as_ref()
            .and_then(|extension| extension.completion_blocker())
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn policy_evaluation(
        &self,
        call: &ToolCall,
        policy: &ApprovalPolicy,
    ) -> Option<loom_core::PolicyEvaluation> {
        self.action_kind(call)
            .map(|action| PolicyEvaluation::evaluate(policy, action, &call.name))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_root(&self) -> &Path {
        &self.root
    }

    pub fn execute(&self, call: &ToolCall) -> ToolResult {
        let Some(kind) = ToolKind::from_name(&call.name) else {
            return self
                .extension
                .as_ref()
                .filter(|extension| extension.action_kind(call).is_some())
                .map_or_else(
                    || ToolResult::failure(call, format!("unknown tool '{}'", call.name)),
                    |extension| extension.execute(call),
                );
        };

        match kind {
            ToolKind::ListFiles => self.list_files(call),
            ToolKind::ReadFile => self.read_file(call),
            ToolKind::SearchText => self.search_text(call),
            ToolKind::Glob => self.glob(call),
            ToolKind::WebSearch => self.web_search(call),
            ToolKind::ProposePlan | ToolKind::AskUser => ToolResult::failure(
                call,
                "control tool is handled by the agent runtime and cannot be executed directly",
            ),
            ToolKind::ApplyPatch => self.apply_patch(call),
            ToolKind::RunCommand => self.run_command(call),
            ToolKind::GitHubListPullRequests => self.github_list_pull_requests(call),
            ToolKind::GitHubGetPullRequest => self.github_get_pull_request(call),
            ToolKind::GitHubCreatePullRequest => self.github_create_pull_request(call),
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
        let depth = match arguments.depth {
            Some(depth) if depth == 0 || depth as usize > MAX_LIST_DEPTH => {
                return ToolResult::failure(
                    call,
                    format!("depth must be between 1 and {MAX_LIST_DEPTH}"),
                );
            }
            Some(depth) => Some(depth as usize),
            None => None,
        };
        let matcher = match compile_glob(arguments.glob.as_deref()) {
            Ok(matcher) => matcher,
            Err(error) => return ToolResult::failure(call, error),
        };
        let max_entries = match parse_limit(
            arguments.max_entries,
            DEFAULT_LIST_MAX_ENTRIES,
            MAX_LIST_MAX_ENTRIES,
            "max_entries",
        ) {
            Ok(max_entries) => max_entries,
            Err(error) => return ToolResult::failure(call, error),
        };
        let (mut files, truncated) =
            match self.collect_files(&path, relative, depth, matcher.as_ref(), max_entries) {
                Ok(collected) => collected,
                Err(error) => return ToolResult::failure(call, error),
            };
        files.sort();
        let mut output = files.join("\n");
        if truncated {
            output.push_str(&format!(
                "\n[... showing first {max_entries} entries; add a glob, lower depth, or narrow the path ...]"
            ));
        }
        ToolResult::success(call, self.limit_output(output))
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
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!("could not read '{}': {error}", arguments.path),
                );
            }
        };
        let total_lines = contents.lines().count();
        if arguments.line_start.is_none() && arguments.line_end.is_none() {
            if contents.len() <= self.max_output_bytes {
                return ToolResult::success(call, contents);
            }
            let note = format!(
                "\n[file has {total_lines} lines; read a section with line_start and line_end]"
            );
            let budget = self.max_output_bytes.saturating_sub(note.len());
            let mut output = truncate_middle(&contents, budget);
            output.push_str(&note);
            return ToolResult::success(call, output);
        }
        let start = arguments.line_start.unwrap_or(1).max(1);
        let end = arguments
            .line_end
            .map_or(total_lines as u64, |end| end.min(total_lines as u64));
        let ranged = contents
            .lines()
            .enumerate()
            .filter(|(index, _)| {
                let number = *index as u64 + 1;
                number >= start && number <= end
            })
            .map(|(index, line)| format!("{}:{}", index + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        let note = if end < total_lines as u64 {
            format!(
                "\n[showing lines {start}-{end} of {total_lines}; continue with line_start={}]",
                end + 1
            )
        } else {
            String::new()
        };
        let budget = self.max_output_bytes.saturating_sub(note.len());
        let mut output = truncate_middle(&ranged, budget);
        output.push_str(&note);
        ToolResult::success(call, output)
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
        let context = arguments
            .context
            .unwrap_or(DEFAULT_SEARCH_CONTEXT_LINES)
            .min(MAX_SEARCH_CONTEXT_LINES);
        let max_results = match parse_limit(
            arguments.max_results,
            DEFAULT_SEARCH_MAX_RESULTS,
            MAX_SEARCH_MAX_RESULTS,
            "max_results",
        ) {
            Ok(max_results) => max_results,
            Err(error) => return ToolResult::failure(call, error),
        };
        let matcher = match SearchMatcher::new(
            &arguments.query,
            arguments.regex.unwrap_or(false),
            arguments.case_sensitive.unwrap_or(true),
        ) {
            Ok(matcher) => matcher,
            Err(error) => return ToolResult::failure(call, error),
        };
        match self.collect_matches(
            &path,
            relative,
            &matcher,
            arguments.glob.as_deref(),
            context,
            max_results,
        ) {
            Ok(output) => ToolResult::success(call, self.limit_output(output)),
            Err(error) => ToolResult::failure(call, error),
        }
    }

    fn glob(&self, call: &ToolCall) -> ToolResult {
        let arguments: GlobArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        if arguments.pattern.trim().is_empty() {
            return ToolResult::failure(call, "glob pattern must not be empty");
        }
        let relative = arguments.path.as_deref().unwrap_or(".");
        let path = match self.resolve_relative(relative) {
            Ok(path) => path,
            Err(error) => return ToolResult::failure(call, error),
        };
        let matcher = match compile_glob(Some(&arguments.pattern)) {
            Ok(Some(matcher)) => matcher,
            Ok(None) => return ToolResult::failure(call, "glob pattern must not be empty"),
            Err(error) => return ToolResult::failure(call, error),
        };
        let max_entries = match parse_limit(
            arguments.max_entries,
            DEFAULT_LIST_MAX_ENTRIES,
            MAX_LIST_MAX_ENTRIES,
            "max_entries",
        ) {
            Ok(max_entries) => max_entries,
            Err(error) => return ToolResult::failure(call, error),
        };
        let mut files = Vec::new();
        let mut truncated = false;
        let entries = match self.workspace.snapshot() {
            Ok(snapshot) => snapshot.entries,
            Err(error) => return ToolResult::failure(call, error.message),
        };
        for entry in entries {
            if entry.kind != loom_workspace::WorkspaceEntryKind::File {
                continue;
            }
            match self.entry_in_scope(&entry.path, relative, &path) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => return ToolResult::failure(call, error),
            }
            if !matcher.is_match(Path::new(&entry.path)) {
                continue;
            }
            if files.len() >= max_entries {
                truncated = true;
                break;
            }
            files.push(entry.path);
        }
        files.sort();
        let mut output = files.join("\n");
        if truncated {
            output.push_str(&format!(
                "\n[... showing first {max_entries} entries; narrow the pattern or path ...]"
            ));
        }
        ToolResult::success(call, self.limit_output(output))
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
        let edits = match arguments.edits() {
            Ok(edits) => edits,
            Err(error) => return ToolResult::failure(call, error),
        };
        let path = arguments.path.clone();
        let mut diffs = Vec::with_capacity(edits.len());
        for (old_text, new_text) in edits {
            match self.apply_single_edit(&path, old_text, new_text) {
                Ok(diff) => diffs.push(diff),
                Err(error) => return ToolResult::failure(call, error),
            }
        }
        let summary = if diffs.len() == 1 {
            format!("updated {path}")
        } else {
            format!("updated {path} ({} edits)", diffs.len())
        };
        ToolResult::success(
            call,
            self.limit_output(format!("{summary}\n{}", diffs.join("\n"))),
        )
    }

    fn apply_single_edit(
        &self,
        path: &str,
        old_text: String,
        new_text: String,
    ) -> std::result::Result<String, String> {
        let current_file = match self.workspace.read_file(path) {
            Ok(file) => file,
            Err(error) if error.code == loom_core::ErrorCode::NotFound => {
                loom_workspace::SessionFilesystemFile {
                    session_id: self.workspace.session_id(),
                    path: path.to_owned(),
                    content: String::new(),
                    revision: String::new(),
                }
            }
            Err(error) => return Err(error.message),
        };
        match self.workspace.apply_edit(WorkspaceEdit {
            path: path.to_owned(),
            old_text,
            new_text,
            expected_revision: if current_file.revision.is_empty() {
                None
            } else {
                Some(current_file.revision)
            },
        }) {
            Ok(result) => Ok(result.diff),
            Err(error) => Err(error.message),
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
        let timeout_ms = arguments.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS);
        if !(MIN_COMMAND_TIMEOUT_MS..=MAX_COMMAND_TIMEOUT_MS).contains(&timeout_ms) {
            return ToolResult::failure(
                call,
                format!(
                    "timeout_ms must be between {MIN_COMMAND_TIMEOUT_MS} and {MAX_COMMAND_TIMEOUT_MS}"
                ),
            );
        }
        let cwd = match arguments
            .cwd
            .as_deref()
            .map_or_else(|| Ok(self.root.clone()), |cwd| self.resolve_relative(cwd))
        {
            Ok(cwd) => cwd,
            Err(error) => return ToolResult::failure(call, error),
        };
        let output =
            match self.execute_command(&arguments.command, &arguments.args, &cwd, timeout_ms) {
                Ok(output) => output,
                Err(error) => return ToolResult::failure(call, error),
            };
        let text = self.limit_output(output.render());
        if output.success() {
            ToolResult::success(call, text)
        } else {
            ToolResult::failure(call, text)
        }
    }

    fn execute_command(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        timeout_ms: u64,
    ) -> std::result::Result<CommandOutput, String> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not start '{program}': {error}"))?;
        let cap = self.max_output_bytes.max(1);
        let stdout_reader = child
            .stdout
            .take()
            .map(|stdout| thread::spawn(move || read_capped(stdout, cap)));
        let stderr_reader = child
            .stderr
            .take()
            .map(|stderr| thread::spawn(move || read_capped(stderr, cap)));
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut status = None;
        loop {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    break;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    thread::sleep(COMMAND_POLL_INTERVAL);
                }
                Err(error) => return Err(format!("could not wait for '{program}': {error}")),
            }
        }
        let (stdout, stdout_truncated) = stdout_reader.map_or((Vec::new(), false), |reader| {
            reader.join().unwrap_or_default()
        });
        let (stderr, stderr_truncated) = stderr_reader.map_or((Vec::new(), false), |reader| {
            reader.join().unwrap_or_default()
        });
        Ok(CommandOutput {
            status,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }

    fn github_list_pull_requests(&self, call: &ToolCall) -> ToolResult {
        let arguments: GitHubPullRequestListArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        let state = arguments.state.as_deref().unwrap_or("open");
        if !matches!(state, "open" | "closed" | "all") {
            return ToolResult::failure(call, "state must be open, closed, or all");
        }
        match self.github_api(
            "GET",
            &arguments.repository,
            &format!("pulls?state={state}&per_page=30"),
            None,
        ) {
            Ok(value) => ToolResult::success(call, self.limit_output(value.to_string())),
            Err(error) => ToolResult::failure(call, error),
        }
    }

    fn github_get_pull_request(&self, call: &ToolCall) -> ToolResult {
        let arguments: GitHubPullRequestArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        let pull_request = match self.github_api(
            "GET",
            &arguments.repository,
            &format!("pulls/{}", arguments.number),
            None,
        ) {
            Ok(value) => value,
            Err(error) => return ToolResult::failure(call, error),
        };
        let Some(head_sha) = pull_request
            .pointer("/head/sha")
            .and_then(serde_json::Value::as_str)
        else {
            return ToolResult::failure(
                call,
                "GitHub pull request response did not include its head commit",
            );
        };
        let check_runs = match self.github_api(
            "GET",
            &arguments.repository,
            &format!("commits/{head_sha}/check-runs"),
            None,
        ) {
            Ok(value) => value,
            Err(error) => return ToolResult::failure(call, error),
        };
        let commit_status = match self.github_api(
            "GET",
            &arguments.repository,
            &format!("commits/{head_sha}/status"),
            None,
        ) {
            Ok(value) => value,
            Err(error) => return ToolResult::failure(call, error),
        };
        let result = serde_json::json!({
            "pull_request": pull_request,
            "check_runs": check_runs,
            "commit_status": commit_status,
        });
        ToolResult::success(call, self.limit_output(result.to_string()))
    }

    fn github_create_pull_request(&self, call: &ToolCall) -> ToolResult {
        let arguments: GitHubCreatePullRequestArguments = match parse_arguments(call) {
            Ok(arguments) => arguments,
            Err(error) => return ToolResult::failure(call, error),
        };
        if arguments.title.trim().is_empty()
            || arguments.head.trim().is_empty()
            || arguments.base.trim().is_empty()
        {
            return ToolResult::failure(call, "title, head, and base must not be empty");
        }
        let body = serde_json::json!({
            "title": arguments.title,
            "body": arguments.body.unwrap_or_default(),
            "head": arguments.head,
            "base": arguments.base,
            "draft": arguments.draft.unwrap_or(false),
        });
        match self.github_api("POST", &arguments.repository, "pulls", Some(body)) {
            Ok(value) => ToolResult::success(call, self.limit_output(value.to_string())),
            Err(error) => ToolResult::failure(call, error),
        }
    }

    fn github_api(
        &self,
        method: &str,
        repository: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> std::result::Result<serde_json::Value, String> {
        let token = self.github_token.as_deref().ok_or_else(|| {
            "GitHub is not connected; connect a GitHub account in Loom settings".to_owned()
        })?;
        if !valid_github_repository(repository) {
            return Err("repository must be in owner/name format".to_owned());
        }
        let url = format!("https://api.github.com/repos/{repository}/{path}");
        let (status, body) = run_async(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(|error| format!("GitHub request failed: {error}"))?;
            let request = match method {
                "GET" => client.get(&url),
                "POST" => client
                    .post(&url)
                    .json(&body.unwrap_or(serde_json::Value::Null)),
                _ => return Err("unsupported GitHub API method".to_owned()),
            };
            let response = request
                .header("Accept", "application/vnd.github+json")
                .header("Authorization", format!("Bearer {token}"))
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("User-Agent", "Loom")
                .send()
                .await
                .map_err(|error| format!("GitHub request failed: {error}"))?;
            let status = response.status().as_u16();
            let text = response
                .text()
                .await
                .map_err(|error| format!("GitHub request failed: {error}"))?;
            Ok::<_, String>((status, text))
        })?;
        if status >= 400 {
            return Err(format!(
                "GitHub API returned HTTP {status}: {}",
                truncate_text(&body, 1024)
            ));
        }
        serde_json::from_str(&body)
            .map_err(|error| format!("GitHub returned an invalid response: {error}"))
    }

    fn resolve_relative(&self, relative: &str) -> std::result::Result<PathBuf, String> {
        let path = Path::new(relative);
        if path.is_absolute() {
            let resolved = fs::canonicalize(path)
                .map_err(|error| format!("could not resolve '{}': {error}", relative))?;
            let mounted = self
                .workspace
                .mounted_directories()
                .map_err(|error| error.message)?;
            if !resolved.starts_with(&self.root)
                && !mounted
                    .iter()
                    .any(|(_, source)| resolved.starts_with(source))
            {
                return Err(format!(
                    "path '{}' must stay inside the workspace root",
                    relative
                ));
            }
            return Ok(resolved);
        }
        self.workspace
            .resolve_path(relative, false)
            .map_err(|error| error.message)
    }

    /// True when a workspace entry is inside the requested path, either through
    /// a relative prefix or through a mounted directory resolved to an absolute
    /// path.
    fn entry_in_scope(
        &self,
        entry_path: &str,
        requested_relative: &str,
        requested_path: &Path,
    ) -> std::result::Result<bool, String> {
        if requested_relative == "." || Path::new(entry_path).starts_with(requested_relative) {
            return Ok(true);
        }
        let child = self
            .workspace
            .resolve_path(entry_path, false)
            .map_err(|error| error.message)?;
        Ok(child.starts_with(requested_path))
    }

    fn collect_files(
        &self,
        requested_path: &Path,
        requested_relative: &str,
        depth: Option<usize>,
        glob: Option<&globset::GlobMatcher>,
        max_entries: usize,
    ) -> std::result::Result<(Vec<String>, bool), String> {
        let mut files = Vec::new();
        let mut truncated = false;
        for entry in self
            .workspace
            .snapshot()
            .map_err(|error| error.message)?
            .entries
        {
            if entry.kind != loom_workspace::WorkspaceEntryKind::File {
                continue;
            }
            if !self.entry_in_scope(&entry.path, requested_relative, requested_path)? {
                continue;
            }
            if depth.is_some_and(|depth| entry_depth(&entry.path, requested_relative) > depth) {
                continue;
            }
            if glob.is_some_and(|glob| !glob.is_match(Path::new(&entry.path))) {
                continue;
            }
            if files.len() >= max_entries {
                truncated = true;
                break;
            }
            files.push(entry.path);
        }
        Ok((files, truncated))
    }

    fn collect_matches(
        &self,
        requested_path: &Path,
        requested_relative: &str,
        matcher: &SearchMatcher,
        glob: Option<&str>,
        context: usize,
        max_results: usize,
    ) -> std::result::Result<String, String> {
        let glob = compile_glob(glob)?;
        let mut rendered = Vec::new();
        let mut matched = 0usize;
        let mut truncated = false;
        for entry in self
            .workspace
            .snapshot()
            .map_err(|error| error.message)?
            .entries
        {
            if entry.kind != loom_workspace::WorkspaceEntryKind::File {
                continue;
            }
            if !self.entry_in_scope(&entry.path, requested_relative, requested_path)? {
                continue;
            }
            let child_relative = Path::new(&entry.path);
            if glob
                .as_ref()
                .is_some_and(|glob| !glob.is_match(child_relative))
            {
                continue;
            }
            let path = self
                .workspace
                .resolve_path(&entry.path, false)
                .map_err(|error| error.message)?;
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
                Err(error) => {
                    return Err(format!(
                        "could not read '{}': {error}",
                        child_relative.display()
                    ));
                }
            };
            let lines = contents.lines().collect::<Vec<_>>();
            let indices = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| matcher.is_match(line))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if indices.is_empty() {
                continue;
            }
            let remaining = max_results.saturating_sub(matched);
            let take = indices.len().min(remaining);
            rendered.extend(render_file_matches(
                &entry.path,
                &lines,
                &indices[..take],
                context,
            ));
            matched += take;
            if take < indices.len() {
                truncated = true;
                break;
            }
            if matched >= max_results {
                truncated = true;
                break;
            }
        }
        let mut output = rendered.join("\n");
        if truncated {
            output.push_str(&format!(
                "\n[... stopped at {max_results} matches; refine the query or raise max_results ...]"
            ));
        }
        Ok(output)
    }

    fn limit_output(&self, output: String) -> String {
        truncate_middle(&output, self.max_output_bytes)
    }
}

fn entry_depth(entry_path: &str, requested_relative: &str) -> usize {
    let entry = Path::new(entry_path);
    let relative = if requested_relative == "." {
        entry
    } else {
        entry.strip_prefix(requested_relative).unwrap_or(entry)
    };
    relative
        .components()
        .filter(|component| matches!(component, std::path::Component::Normal(_)))
        .count()
}

fn render_file_matches(
    path: &str,
    lines: &[&str],
    indices: &[usize],
    context: usize,
) -> Vec<String> {
    let matched = indices.iter().copied().collect::<BTreeSet<_>>();
    let mut intervals: Vec<(usize, usize)> = Vec::new();
    for &index in indices {
        let start = index.saturating_sub(context);
        let end = index
            .saturating_add(context)
            .min(lines.len().saturating_sub(1));
        match intervals.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => intervals.push((start, end)),
        }
    }
    let mut output = Vec::new();
    for (start, end) in intervals {
        for (index, line) in lines.iter().enumerate().take(end + 1).skip(start) {
            let separator = if matched.contains(&index) { ':' } else { '-' };
            output.push(format!("{path}{separator}{}{separator}{line}", index + 1));
        }
    }
    output
}

#[derive(Debug)]
enum SearchMatcher {
    Literal {
        needle: String,
        case_sensitive: bool,
    },
    Regex(regex::Regex),
}

impl SearchMatcher {
    fn new(query: &str, regex: bool, case_sensitive: bool) -> std::result::Result<Self, String> {
        if regex {
            let matcher = RegexBuilder::new(query)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(|error| format!("invalid regular expression '{query}': {error}"))?;
            Ok(Self::Regex(matcher))
        } else if case_sensitive {
            Ok(Self::Literal {
                needle: query.to_owned(),
                case_sensitive,
            })
        } else {
            Ok(Self::Literal {
                needle: query.to_lowercase(),
                case_sensitive,
            })
        }
    }

    fn is_match(&self, line: &str) -> bool {
        match self {
            Self::Literal {
                needle,
                case_sensitive: true,
            } => line.contains(needle),
            Self::Literal {
                needle,
                case_sensitive: false,
            } => line.to_lowercase().contains(needle),
            Self::Regex(regex) => regex.is_match(line),
        }
    }
}

fn compile_glob(
    pattern: Option<&str>,
) -> std::result::Result<Option<globset::GlobMatcher>, String> {
    pattern
        .map(|pattern| {
            Glob::new(pattern)
                .map(|glob| glob.compile_matcher())
                .map_err(|error| format!("invalid glob '{pattern}': {error}"))
        })
        .transpose()
}

fn parse_limit(
    value: Option<usize>,
    default: usize,
    max: usize,
    name: &str,
) -> std::result::Result<usize, String> {
    let value = value.unwrap_or(default);
    if value == 0 || value > max {
        return Err(format!("{name} must be between 1 and {max}"));
    }
    Ok(value)
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Truncate to a byte budget while preserving both ends. Build and test output
/// usually puts the meaningful summary at the tail, so a head-only cut hides
/// the signal that tells the model whether the command passed.
fn truncate_middle(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_owned();
    }
    if max_bytes <= TRUNCATION_MARKER_RESERVE {
        return output[..floor_char_boundary(output, max_bytes)].to_owned();
    }
    let available = max_bytes - TRUNCATION_MARKER_RESERVE;
    let head_budget = available - available / 3;
    let tail_budget = available - head_budget;
    let head_end = floor_char_boundary(output, head_budget);
    let tail_start = ceil_char_boundary(output, output.len() - tail_budget);
    if tail_start <= head_end {
        return output[..floor_char_boundary(output, max_bytes)].to_owned();
    }
    let omitted = tail_start - head_end;
    let marker = format!("\n[... {omitted} bytes omitted ...]\n");
    let mut result = String::with_capacity(head_end + marker.len() + (output.len() - tail_start));
    result.push_str(&output[..head_end]);
    result.push_str(&marker);
    result.push_str(&output[tail_start..]);
    result
}

#[derive(Debug)]
struct CommandOutput {
    status: Option<std::process::ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl CommandOutput {
    fn success(&self) -> bool {
        self.status.is_some_and(|status| status.success())
    }

    fn render(&self) -> String {
        let stdout = String::from_utf8_lossy(&self.stdout);
        let stderr = String::from_utf8_lossy(&self.stderr);
        let mut text = String::new();
        if !stdout.is_empty() {
            text.push_str("stdout:\n");
            text.push_str(&stdout);
            if self.stdout_truncated {
                text.push_str("\n[stdout truncated]");
            }
        }
        if !stderr.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("stderr:\n");
            text.push_str(&stderr);
            if self.stderr_truncated {
                text.push_str("\n[stderr truncated]");
            }
        }
        let status = match self.status {
            Some(status) => format!("exit status: {status}"),
            None => "command timed out and was killed".to_owned(),
        };
        if text.is_empty() {
            status
        } else {
            format!("{text}\n{status}")
        }
    }
}

fn read_capped(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut stored = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if stored.len() < cap {
                    let take = (cap - stored.len()).min(read);
                    stored.extend_from_slice(&buffer[..take]);
                    if take < read {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (stored, truncated)
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(
    call: &ToolCall,
) -> std::result::Result<T, String> {
    serde_json::from_value(call.arguments.clone())
        .map_err(|error| format!("invalid arguments for '{}': {error}", call.name))
}

fn valid_github_repository(repository: &str) -> bool {
    let mut parts = repository.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(name) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !owner.is_empty()
        && !name.is_empty()
        && [owner, name].iter().all(|part| {
            part.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

#[derive(Debug, Deserialize)]
struct GitHubPullRequestListArguments {
    repository: String,
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubPullRequestArguments {
    repository: String,
    number: u64,
}

#[derive(Debug, Deserialize)]
struct GitHubCreatePullRequestArguments {
    repository: String,
    title: String,
    head: String,
    base: String,
    body: Option<String>,
    draft: Option<bool>,
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: ToolKind::ListFiles.name().to_owned(),
            description: "List workspace files below a path, optionally limited by depth and filtered by a glob. Results are bounded; narrow with path, depth, or glob rather than listing a large tree.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "depth": {"type": "integer", "minimum": 1, "maximum": 32},
                    "glob": {"type": "string"},
                    "max_entries": {"type": "integer", "minimum": 1, "maximum": 10_000}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::ReadFile.name().to_owned(),
            description: "Read a UTF-8 text file inside the workspace. Returns the whole file, or an inclusive line range with line numbers when line_start/line_end are given. Oversized reads keep both ends and report the total line count so you can request a range.".to_owned(),
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
            description: "Search workspace text files and return bounded matching lines as path:line:content. `query` is a literal string unless `regex` is true. Prefer this over reading whole files when locating code; refine the query or raise max_results instead of searching broadly.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": "string"},
                    "glob": {"type": "string"},
                    "regex": {"type": "boolean"},
                    "case_sensitive": {"type": "boolean"},
                    "context": {"type": "integer", "minimum": 0, "maximum": 5},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 1_000}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::Glob.name().to_owned(),
            description: "Find workspace files whose path matches a glob pattern (for example `**/*.rs`). Returns bounded, sorted paths. Use this to discover files before reading or searching them.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"},
                    "max_entries": {"type": "integer", "minimum": 1, "maximum": 10_000}
                },
                "required": ["pattern"],
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
            description: "Edit a workspace file with one or more exact text replacements. Provide `edits` (each an old_text/new_text pair), or the single old_text/new_text pair. Each old_text must match exactly once. Returns a unified diff.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {"type": "string"},
                                "new_text": {"type": "string"}
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::RunCommand.name().to_owned(),
            description: "Run a program directly without a shell in the workspace, with an optional timeout and working directory. Returns stdout and stderr separately with the exit status; output is bounded.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "cwd": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 600_000}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::GitHubListPullRequests.name().to_owned(),
            description: "List pull requests for a GitHub repository. Requires a connected GitHub account and approval for network access.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "repository": {"type": "string", "description": "Repository in owner/name format."},
                    "state": {"type": "string", "enum": ["open", "closed", "all"]}
                },
                "required": ["repository"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::GitHubGetPullRequest.name().to_owned(),
            description: "Read a pull request's details and merge status. Requires a connected GitHub account and approval for network access.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "repository": {"type": "string", "description": "Repository in owner/name format."},
                    "number": {"type": "integer", "minimum": 1}
                },
                "required": ["repository", "number"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: ToolKind::GitHubCreatePullRequest.name().to_owned(),
            description: "Create a pull request in a GitHub repository. The branch must already be pushed. Requires explicit write approval.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "repository": {"type": "string", "description": "Repository in owner/name format."},
                    "title": {"type": "string"},
                    "head": {"type": "string", "description": "Source branch, optionally owner:branch."},
                    "base": {"type": "string", "description": "Target branch."},
                    "body": {"type": "string"},
                    "draft": {"type": "boolean"}
                },
                "required": ["repository", "title", "head", "base"],
                "additionalProperties": false
            }),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct ListFilesArguments {
    path: Option<String>,
    depth: Option<u32>,
    glob: Option<String>,
    max_entries: Option<usize>,
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
    regex: Option<bool>,
    case_sensitive: Option<bool>,
    context: Option<usize>,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct GlobArguments {
    pattern: String,
    path: Option<String>,
    max_entries: Option<usize>,
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
    old_text: Option<String>,
    new_text: Option<String>,
    #[serde(default)]
    edits: Vec<ApplyPatchEdit>,
}

#[derive(Debug, Deserialize)]
struct ApplyPatchEdit {
    old_text: String,
    new_text: String,
}

impl ApplyPatchArguments {
    fn edits(&self) -> std::result::Result<Vec<(String, String)>, String> {
        let legacy = self.old_text.is_some() || self.new_text.is_some();
        if !self.edits.is_empty() && legacy {
            return Err(
                "apply_patch accepts either edits or old_text/new_text, not both".to_owned(),
            );
        }
        if !self.edits.is_empty() {
            return Ok(self
                .edits
                .iter()
                .map(|edit| (edit.old_text.clone(), edit.new_text.clone()))
                .collect());
        }
        match (&self.old_text, &self.new_text) {
            (Some(old_text), Some(new_text)) => Ok(vec![(old_text.clone(), new_text.clone())]),
            _ => Err("apply_patch requires edits, or both old_text and new_text".to_owned()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RunCommandArguments {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
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

    #[cfg(unix)]
    #[test]
    fn lists_and_reads_attached_directory_in_place() {
        let root = workspace();
        let source =
            std::env::temp_dir().join(format!("loom-tools-source-{}", AgentSessionId::new()));
        fs::create_dir(&source).unwrap();
        fs::write(source.join("note.txt"), "attached content").unwrap();
        let workspace = Workspace::open(AgentSessionId::new(), &root).unwrap();
        workspace.mount_directory("sources/local", &source).unwrap();
        let executor = ToolExecutor::new_with_workspace(workspace);
        let listed = executor.execute(&call("list_files", serde_json::json!({"path": "."})));
        assert!(listed.output.contains("sources/local/note.txt"));
        let read = executor.execute(&call(
            "read_file",
            serde_json::json!({"path": "sources/local/note.txt"}),
        ));
        assert_eq!(read.output, "attached content");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(source).unwrap();
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
    fn truncates_unicode_while_keeping_both_ends() {
        let root = workspace();
        let executor = ToolExecutor::new(&root).unwrap();
        let output = executor.limit_output(format!("head{}tail", "😀".repeat(20_000)));
        assert!(output.starts_with("head"));
        assert!(output.ends_with("tail"));
        assert!(output.contains("bytes omitted"));
        assert!(output.len() <= 64 * 1024);
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_text_supports_literal_regex_context_and_limits() {
        let root = workspace();
        fs::write(
            root.join("src/main.rs"),
            "fn main() {}\nlet Needle = 1;\nlet other = 2;\n",
        )
        .unwrap();
        let executor = ToolExecutor::new(&root).unwrap();

        let literal =
            executor.execute(&call("search_text", serde_json::json!({"query": "Needle"})));
        assert!(literal.success);
        assert!(literal.output.contains("src/main.rs:2:let Needle = 1;"));

        let regex = executor.execute(&call(
            "search_text",
            serde_json::json!({
                "query": "needle = \\d+",
                "regex": true,
                "case_sensitive": false,
                "context": 1
            }),
        ));
        assert!(regex.success);
        assert!(regex.output.contains("src/main.rs:2:let Needle = 1;"));
        assert!(regex.output.contains("src/main.rs-1-fn main() {}"));
        assert!(regex.output.contains("src/main.rs-3-let other = 2;"));

        let invalid = executor.execute(&call(
            "search_text",
            serde_json::json!({"query": "(", "regex": true}),
        ));
        assert!(!invalid.success);
        assert!(invalid.output.contains("invalid regular expression"));

        let empty = executor.execute(&call(
            "search_text",
            serde_json::json!({"query": "", "regex": true}),
        ));
        assert!(!empty.success);

        let capped = executor.execute(&call(
            "search_text",
            serde_json::json!({"query": "let", "max_results": 1}),
        ));
        assert!(capped.success);
        assert!(capped.output.contains("stopped at 1 matches"));
        assert_eq!(capped.output.matches("src/main.rs").count(), 1);

        let zero = executor.execute(&call(
            "search_text",
            serde_json::json!({"query": "let", "max_results": 0}),
        ));
        assert!(!zero.success);
        assert!(zero.output.contains("max_results must be between"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_file_reports_line_ranges_and_file_size() {
        let root = workspace();
        fs::write(
            root.join("src/main.rs"),
            (1..=100)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let executor = ToolExecutor::new(&root).unwrap();

        let whole = executor.execute(&call(
            "read_file",
            serde_json::json!({"path": "src/main.rs"}),
        ));
        assert!(whole.success);
        assert!(whole.output.starts_with("line 1\n"));
        assert!(whole.output.ends_with("line 100"));

        let ranged = executor.execute(&call(
            "read_file",
            serde_json::json!({"path": "src/main.rs", "line_start": 10, "line_end": 20}),
        ));
        assert!(ranged.success);
        assert!(ranged.output.starts_with("10:line 10\n"));
        assert!(
            ranged
                .output
                .contains("showing lines 10-20 of 100; continue with line_start=21")
        );

        let past_end = executor.execute(&call(
            "read_file",
            serde_json::json!({"path": "src/main.rs", "line_start": 2, "line_end": 3}),
        ));
        assert!(past_end.output.starts_with("2:line 2\n3:line 3\n"));
        assert!(
            past_end
                .output
                .contains("of 100; continue with line_start=4")
        );

        let large = executor.execute(&call("read_file", serde_json::json!({"path": "README.md"})));
        assert_eq!(large.output, "Loom workspace\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn list_files_supports_depth_glob_and_entry_limits() {
        let root = workspace();
        let executor = ToolExecutor::new(&root).unwrap();

        let shallow = executor.execute(&call("list_files", serde_json::json!({"depth": 1})));
        assert!(shallow.success);
        assert!(shallow.output.contains("README.md"));
        assert!(!shallow.output.contains("src/main.rs"));

        let globbed = executor.execute(&call("list_files", serde_json::json!({"glob": "**/*.rs"})));
        assert!(globbed.success);
        assert!(globbed.output.contains("src/main.rs"));
        assert!(!globbed.output.contains("README.md"));

        let capped = executor.execute(&call("list_files", serde_json::json!({"max_entries": 1})));
        assert!(capped.success);
        assert!(capped.output.contains("showing first 1 entries"));

        let invalid_depth = executor.execute(&call("list_files", serde_json::json!({"depth": 0})));
        assert!(!invalid_depth.success);
        assert!(invalid_depth.output.contains("depth must be between"));

        let invalid_glob = executor.execute(&call("list_files", serde_json::json!({"glob": "["})));
        assert!(!invalid_glob.success);
        assert!(invalid_glob.output.contains("invalid glob"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn glob_returns_bounded_matching_paths() {
        let root = workspace();
        let executor = ToolExecutor::new(&root).unwrap();

        let result = executor.execute(&call(
            "glob",
            serde_json::json!({"pattern": "**/*.rs", "max_entries": 5}),
        ));
        assert!(result.success);
        assert!(result.output.contains("src/main.rs"));
        assert!(!result.output.contains("README.md"));

        let empty = executor.execute(&call("glob", serde_json::json!({"pattern": ""})));
        assert!(!empty.success);

        let capped = executor.execute(&call(
            "glob",
            serde_json::json!({"pattern": "**/*", "max_entries": 1}),
        ));
        assert!(capped.success);
        assert!(capped.output.contains("showing first 1 entries"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn apply_patch_supports_multiple_edits_and_rejects_mixed_forms() {
        let root = workspace();
        fs::write(root.join("src/main.rs"), "alpha\nbeta\ngamma\n").unwrap();
        let executor = ToolExecutor::new(&root).unwrap();

        let applied = executor.execute(&call(
            "apply_patch",
            serde_json::json!({
                "path": "src/main.rs",
                "edits": [
                    {"old_text": "alpha", "new_text": "ALPHA"},
                    {"old_text": "gamma", "new_text": "GAMMA"}
                ]
            }),
        ));
        assert!(applied.success);
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "ALPHA\nbeta\nGAMMA\n"
        );

        let mixed = executor.execute(&call(
            "apply_patch",
            serde_json::json!({
                "path": "src/main.rs",
                "old_text": "ALPHA",
                "new_text": "x",
                "edits": [{"old_text": "beta", "new_text": "y"}]
            }),
        ));
        assert!(!mixed.success);
        assert!(mixed.output.contains("not both"));

        let missing = executor.execute(&call(
            "apply_patch",
            serde_json::json!({"path": "src/main.rs"}),
        ));
        assert!(!missing.success);
        assert!(missing.output.contains("requires edits"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn run_command_separates_streams_and_enforces_timeout() {
        let root = workspace();
        let executor = ToolExecutor::new(&root).unwrap();

        let streams = executor.execute(&call(
            "run_command",
            serde_json::json!({
                "command": "sh",
                "args": ["-c", "echo out; echo err 1>&2"]
            }),
        ));
        assert!(streams.success);
        assert!(streams.output.contains("stdout:\nout"));
        assert!(streams.output.contains("stderr:\nerr"));
        assert!(streams.output.contains("exit status:"));

        let timed_out = executor.execute(&call(
            "run_command",
            serde_json::json!({"command": "sleep", "args": ["5"], "timeout_ms": 200}),
        ));
        assert!(!timed_out.success);
        assert!(timed_out.output.contains("timed out"));

        let invalid = executor.execute(&call(
            "run_command",
            serde_json::json!({"command": "true", "timeout_ms": 50}),
        ));
        assert!(!invalid.success);
        assert!(invalid.output.contains("timeout_ms must be between"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tool_definitions_include_context_controls() {
        let definitions = tool_definitions();
        let glob = definitions
            .iter()
            .find(|definition| definition.name == "glob")
            .expect("glob tool definition");
        assert!(glob.input_schema["required"][0] == "pattern");
        let search = definitions
            .iter()
            .find(|definition| definition.name == "search_text")
            .expect("search tool definition");
        assert!(search.input_schema["properties"].get("regex").is_some());
        assert!(
            search.input_schema["properties"]
                .get("max_results")
                .is_some()
        );
    }
}
