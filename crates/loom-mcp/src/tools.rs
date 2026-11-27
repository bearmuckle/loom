//! The first-slice tool surface: one tool per `loom-cli` verb.

use loom_cli::cli::Execution;
use serde_json::{Value, json};

use crate::config::AppState;
use crate::jsonrpc::RpcError;
use crate::runner;

/// Tool names that mutate the worker. They are hidden unless the operator
/// opted in with `--allow-mutating-tools`, and each carries a
/// `destructiveHint` so an MCP host can still require per-call approval.
pub const MUTATING_TOOLS: &[&str] = &[
    "run",
    "send",
    "approve",
    "deny",
    "archive",
    "copilot_login_start",
    "copilot_login_status",
];
const ALL_TOOLS: &[&str] = &[
    "ls",
    "status",
    "run",
    "send",
    "approve",
    "deny",
    "archive",
    "models_list",
    "providers_list",
    "copilot_login_start",
    "copilot_login_status",
];

/// One declared MCP tool.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
    pub mutating: bool,
}

impl ToolSpec {
    pub(crate) fn to_json(&self) -> Value {
        let annotations = if self.mutating {
            json!({ "destructiveHint": true })
        } else {
            json!({ "readOnlyHint": true })
        };
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.schema,
            "annotations": annotations,
        })
    }
}

/// Whether `name` is a mutating tool.
pub fn is_mutating(name: &str) -> bool {
    MUTATING_TOOLS.contains(&name)
}

/// Whether a tool may be called under the current policy: mutating tools are
/// opt-in via [`Config::allow_mutating`] and always hidden in read-only mode.
pub fn tool_allowed(name: &str, config: &crate::config::Config) -> bool {
    !is_mutating(name) || (config.allow_mutating && !config.read_only)
}

/// The declared tool set, in a stable order.
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "ls",
            description: "List workspaces, or the sessions of one workspace (loom-cli ls).",
            mutating: false,
            schema: json!({
                "type": "object",
                "properties": {
                    "workspace": {
                        "type": "string",
                        "description": "Workspace id; when omitted every workspace is listed."
                    },
                    "archived": {
                        "type": "boolean",
                        "description": "Include archived sessions when a workspace is named."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "status",
            description: "Report one session, its latest run, and any running tool call \
                          (loom-cli status).",
            mutating: false,
            schema: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id or unique prefix." },
                    "format": {
                        "type": "string",
                        "enum": ["text", "verb"],
                        "description": "text is the full report; verb prints one declaration line."
                    },
                    "tree": {
                        "type": "boolean",
                        "description": "Also project the session's project hierarchy."
                    }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "run",
            description: "Start one agent run in a workspace without waiting for it \
                          (loom-cli run).",
            mutating: true,
            schema: json!({
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Workspace id." },
                    "task": { "type": "string", "description": "Task text; inline only, never a server file path." },
                    "name": { "type": "string", "description": "Session name to reuse or create." },
                    "model": { "type": "string", "description": "Model id, or derive from the worker." },
                    "repo": { "type": "string", "description": "Repository source to attach to a new session." },
                    "revision": { "type": "string", "description": "Revision to clone; --repo only." },
                    "path": { "type": "string", "description": "Session path the input lands at." },
                    "approval_policy": { "type": "string", "enum": ["default"] },
                    "request_id": { "type": "string", "description": "Idempotency key of the run start." },
                    "new": { "type": "boolean", "description": "Always create a session." },
                    "project": { "type": "string", "description": "Project root the session is a sub project of." },
                    "child_model": { "type": "string", "description": "Default model for delegated children." },
                    "child_model_for": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Entries of the form kind=model for delegated children."
                    }
                },
                "required": ["workspace", "task"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "send",
            description: "Send follow-up direction to an active run (loom-cli send).",
            mutating: true,
            schema: json!({
                "type": "object",
                "properties": {
                    "run": { "type": "string", "description": "Run id." },
                    "text": { "type": "string", "description": "Direction text." },
                    "request_id": { "type": "string", "description": "Idempotency key." }
                },
                "required": ["run", "text"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "approve",
            description: "Approve the tool call that blocked a run (loom-cli approve).",
            mutating: true,
            schema: json!({
                "type": "object",
                "properties": {
                    "run": { "type": "string", "description": "Run id." },
                    "tool_call": { "type": "string", "description": "Tool call id." },
                    "request_id": { "type": "string", "description": "Idempotency key." }
                },
                "required": ["run", "tool_call"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "deny",
            description: "Deny the tool call that blocked a run (loom-cli deny).",
            mutating: true,
            schema: json!({
                "type": "object",
                "properties": {
                    "run": { "type": "string", "description": "Run id." },
                    "tool_call": { "type": "string", "description": "Tool call id." },
                    "reason": { "type": "string", "description": "Reason recorded with the denial." },
                    "request_id": { "type": "string", "description": "Idempotency key." }
                },
                "required": ["run", "tool_call"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "archive",
            description: "Archive one session, keeping its transcript and checkouts \
                          (loom-cli session archive).",
            mutating: true,
            schema: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id or unique prefix." },
                    "yes": {
                        "type": "boolean",
                        "description": "Confirm a project-root archive that cascades."
                    },
                    "request_id": { "type": "string", "description": "Idempotency key." }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "models_list",
            description: "List the models of the configured providers; prints no secrets \
                          (loom-cli models list).",
            mutating: false,
            schema: json!({
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "description": "Filter to one provider id." }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "providers_list",
            description: "List configured providers and whether each has a credential; prints no \
                          secrets (loom-cli providers list).",
            mutating: false,
            schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "copilot_login_start",
            description: "Start the GitHub Copilot device-login flow on the worker and optionally \
                          poll until it finishes; returns only the public verification URL, user \
                          code, and status, never a token (loom-cli providers login).",
            mutating: true,
            schema: json!({
                "type": "object",
                "properties": {
                    "provider": {
                        "type": "string",
                        "enum": ["github-copilot"],
                        "description": "Provider to sign in to (defaults to github-copilot)."
                    },
                    "no_wait": {
                        "type": "boolean",
                        "description": "Return the public verification URL, user code, and login_id immediately without polling."
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum seconds to poll for completion; omitted uses the worker's expires_in."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "copilot_login_status",
            description: "Poll one in-flight GitHub Copilot device login once; reports pending, \
                          configured, or failed and never a token (loom-cli providers login-status).",
            mutating: true,
            schema: json!({
                "type": "object",
                "properties": {
                    "login_id": {
                        "type": "string",
                        "description": "Login id returned by copilot_login_start."
                    }
                },
                "required": ["login_id"],
                "additionalProperties": false
            }),
        },
    ]
}

fn arguments_object<'a>(
    arguments: &'a Value,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_owned())?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown argument '{key}'"));
        }
    }
    Ok(object)
}

fn optional_string(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("argument '{key}' must be a string")),
    }
}

fn required_string(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, String> {
    match arguments.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        Some(Value::String(_)) => Err(format!("argument '{key}' must not be empty")),
        Some(_) => Err(format!("argument '{key}' must be a string")),
        None => Err(format!("missing required argument '{key}'")),
    }
}

fn optional_bool(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, String> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("argument '{key}' must be a boolean")),
    }
}

fn optional_positive_u64(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<u64>, String> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value) if value > 0 => Ok(Some(value)),
            Some(_) => Err(format!("argument '{key}' must be greater than zero")),
            None => Err(format!("argument '{key}' must be a positive integer")),
        },
        Some(_) => Err(format!("argument '{key}' must be a positive integer")),
    }
}

fn optional_string_array(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(value) => values.push(value.to_owned()),
                    None => return Err(format!("entries of '{key}' must be strings")),
                }
            }
            Ok(Some(values))
        }
        Some(_) => Err(format!("argument '{key}' must be an array of strings")),
    }
}

fn parse_workspace_id(value: &str) -> Result<String, String> {
    value
        .parse::<loom_core::WorkspaceId>()
        .map_err(|_| format!("'{value}' is not a valid workspace id"))?;
    Ok(value.to_owned())
}

fn parse_run_id(value: &str) -> Result<String, String> {
    value
        .parse::<loom_core::RunId>()
        .map_err(|_| format!("'{value}' is not a valid run id"))?;
    Ok(value.to_owned())
}

fn parse_tool_call_id(value: &str) -> Result<String, String> {
    value
        .parse::<loom_core::ToolCallId>()
        .map_err(|_| format!("'{value}' is not a valid tool call id"))?;
    Ok(value.to_owned())
}

fn parse_request_id(value: &str) -> Result<String, String> {
    value
        .parse::<loom_core::RequestId>()
        .map_err(|_| format!("'{value}' is not a valid request id"))?;
    Ok(value.to_owned())
}

/// A session input may be a full id or a unique prefix of one, matching the
/// `loom-cli` surface. A leading `-` is rejected here so it can never be read
/// as a flag once the value reaches the command-line parser.
fn parse_session(value: &str) -> Result<String, String> {
    if value.starts_with('-') {
        return Err(format!("'{value}' is not a valid session id"));
    }
    if value.parse::<loom_core::AgentSessionId>().is_ok() || is_session_prefix(value) {
        return Ok(value.to_owned());
    }
    Err(format!("'{value}' is not a valid session id"))
}

fn is_session_prefix(value: &str) -> bool {
    let mut digits = 0;
    for character in value.chars() {
        if character == '-' {
            continue;
        }
        if !character.is_ascii_hexdigit() {
            return false;
        }
        digits += 1;
    }
    (1..=32).contains(&digits)
}

/// Maps one MCP tool name and its arguments to the `loom-cli` command line.
///
/// Arguments are validated against the declared schema before a command line
/// is built: unknown keys, wrong JSON types, and invalid ids are rejected
/// here as JSON-RPC invalid params instead of being re-parsed as argv.
pub fn tool_argv(name: &str, arguments: &Value) -> Result<Vec<String>, String> {
    let mut argv: Vec<String> = Vec::new();
    match name {
        "ls" => {
            let arguments = arguments_object(arguments, &["workspace", "archived"])?;
            argv.push("ls".to_owned());
            if let Some(workspace) = optional_string(arguments, "workspace")? {
                argv.extend(["--workspace".to_owned(), parse_workspace_id(&workspace)?]);
            }
            if optional_bool(arguments, "archived")?.unwrap_or(false) {
                argv.push("--archived".to_owned());
            }
        }
        "status" => {
            let arguments = arguments_object(arguments, &["session", "format", "tree"])?;
            let session = parse_session(&required_string(arguments, "session")?)?;
            argv.extend(["status".to_owned(), "--session".to_owned(), session]);
            match optional_string(arguments, "format")?.as_deref() {
                Some("verb") => argv.extend(["--format".to_owned(), "verb".to_owned()]),
                Some("text") | None => {}
                Some(other) => return Err(format!("'format' must be text or verb, got '{other}'")),
            }
            if optional_bool(arguments, "tree")?.unwrap_or(false) {
                argv.push("--tree".to_owned());
            }
        }
        "run" => {
            let arguments = arguments_object(
                arguments,
                &[
                    "workspace",
                    "task",
                    "name",
                    "model",
                    "repo",
                    "revision",
                    "path",
                    "approval_policy",
                    "request_id",
                    "new",
                    "project",
                    "child_model",
                    "child_model_for",
                ],
            )?;
            let workspace = parse_workspace_id(&required_string(arguments, "workspace")?)?;
            argv.extend(["run".to_owned(), "--workspace".to_owned(), workspace]);
            let task = required_string(arguments, "task")?;
            argv.extend(["--task".to_owned(), task]);
            if let Some(name) = optional_string(arguments, "name")? {
                if name.trim().is_empty() {
                    return Err("argument 'name' must not be empty".to_owned());
                }
                argv.extend(["--name".to_owned(), name]);
            }
            if let Some(model) = optional_string(arguments, "model")? {
                argv.extend(["--model".to_owned(), model]);
            }
            let repo = optional_string(arguments, "repo")?;
            if let Some(repo) = &repo {
                argv.extend(["--repo".to_owned(), repo.clone()]);
            }
            if let Some(revision) = optional_string(arguments, "revision")? {
                if repo.is_none() {
                    return Err("argument 'revision' requires 'repo'".to_owned());
                }
                argv.extend(["--revision".to_owned(), revision]);
            }
            if let Some(path) = optional_string(arguments, "path")? {
                if repo.is_none() {
                    return Err("argument 'path' requires 'repo'".to_owned());
                }
                argv.extend(["--path".to_owned(), path]);
            }
            if let Some(policy) = optional_string(arguments, "approval_policy")? {
                if policy != "default" {
                    return Err("argument 'approval_policy' must be 'default'".to_owned());
                }
                argv.extend(["--approval-policy".to_owned(), policy]);
            }
            if let Some(request_id) = optional_string(arguments, "request_id")? {
                argv.extend(["--request-id".to_owned(), parse_request_id(&request_id)?]);
            }
            if optional_bool(arguments, "new")?.unwrap_or(false) {
                argv.push("--new".to_owned());
            }
            if let Some(project) = optional_string(arguments, "project")? {
                argv.extend(["--project".to_owned(), parse_session(&project)?]);
            }
            if let Some(child_model) = optional_string(arguments, "child_model")? {
                argv.extend(["--child-model".to_owned(), child_model]);
            }
            if let Some(entries) = optional_string_array(arguments, "child_model_for")? {
                for entry in entries {
                    argv.extend(["--child-model-for".to_owned(), entry]);
                }
            }
        }
        "send" => {
            let arguments = arguments_object(arguments, &["run", "text", "request_id"])?;
            let run = parse_run_id(&required_string(arguments, "run")?)?;
            let text = required_string(arguments, "text")?;
            argv.extend([
                "send".to_owned(),
                "--run".to_owned(),
                run,
                "--text".to_owned(),
                text,
            ]);
            if let Some(request_id) = optional_string(arguments, "request_id")? {
                argv.extend(["--request-id".to_owned(), parse_request_id(&request_id)?]);
            }
        }
        "approve" => {
            let arguments = arguments_object(arguments, &["run", "tool_call", "request_id"])?;
            let run = parse_run_id(&required_string(arguments, "run")?)?;
            let tool_call = parse_tool_call_id(&required_string(arguments, "tool_call")?)?;
            argv.extend([
                "approve".to_owned(),
                "--run".to_owned(),
                run,
                "--tool-call".to_owned(),
                tool_call,
            ]);
            if let Some(request_id) = optional_string(arguments, "request_id")? {
                argv.extend(["--request-id".to_owned(), parse_request_id(&request_id)?]);
            }
        }
        "deny" => {
            let arguments =
                arguments_object(arguments, &["run", "tool_call", "reason", "request_id"])?;
            let run = parse_run_id(&required_string(arguments, "run")?)?;
            let tool_call = parse_tool_call_id(&required_string(arguments, "tool_call")?)?;
            argv.extend([
                "deny".to_owned(),
                "--run".to_owned(),
                run,
                "--tool-call".to_owned(),
                tool_call,
            ]);
            if let Some(reason) = optional_string(arguments, "reason")? {
                argv.extend(["--reason".to_owned(), reason]);
            }
            if let Some(request_id) = optional_string(arguments, "request_id")? {
                argv.extend(["--request-id".to_owned(), parse_request_id(&request_id)?]);
            }
        }
        "archive" => {
            let arguments = arguments_object(arguments, &["session", "yes", "request_id"])?;
            let session = parse_session(&required_string(arguments, "session")?)?;
            argv.extend(["session".to_owned(), "archive".to_owned()]);
            if optional_bool(arguments, "yes")?.unwrap_or(false) {
                argv.push("--yes".to_owned());
            }
            if let Some(request_id) = optional_string(arguments, "request_id")? {
                argv.extend(["--request-id".to_owned(), parse_request_id(&request_id)?]);
            }
            // `--` keeps a session id that starts with `-` positional, never a
            // flag to the command-line parser.
            argv.push("--".to_owned());
            argv.push(session);
        }
        "models_list" => {
            let arguments = arguments_object(arguments, &["provider"])?;
            argv.extend(["models".to_owned(), "list".to_owned()]);
            if let Some(provider) = optional_string(arguments, "provider")? {
                argv.extend(["--provider".to_owned(), provider]);
            }
        }
        "providers_list" => {
            let _ = arguments_object(arguments, &[])?;
            argv.extend(["providers".to_owned(), "list".to_owned()]);
        }
        "copilot_login_start" => {
            let arguments = arguments_object(arguments, &["provider", "no_wait", "timeout"])?;
            let provider = optional_string(arguments, "provider")?
                .unwrap_or_else(|| "github-copilot".to_owned());
            if provider != "github-copilot" {
                return Err(format!(
                    "argument 'provider' must be 'github-copilot', got '{provider}'"
                ));
            }
            let no_wait = optional_bool(arguments, "no_wait")?.unwrap_or(false);
            let timeout = optional_positive_u64(arguments, "timeout")?;
            if no_wait && timeout.is_some() {
                return Err("argument 'timeout' does not apply when 'no_wait' is true".to_owned());
            }
            argv.extend([
                "providers".to_owned(),
                "login".to_owned(),
                "--provider".to_owned(),
                provider,
            ]);
            if no_wait {
                argv.push("--no-wait".to_owned());
            }
            if let Some(seconds) = timeout {
                argv.extend(["--timeout".to_owned(), seconds.to_string()]);
            }
        }
        "copilot_login_status" => {
            let arguments = arguments_object(arguments, &["login_id"])?;
            let login_id = required_string(arguments, "login_id")?;
            let login_id = login_id.trim().to_owned();
            if login_id.starts_with('-') {
                return Err(format!("'{login_id}' is not a valid login_id"));
            }
            argv.extend([
                "providers".to_owned(),
                "login-status".to_owned(),
                "--login-id".to_owned(),
                login_id,
            ]);
        }
        _ => return Err(format!("unknown tool '{name}'")),
    }
    Ok(argv)
}

/// The structured result of one verb execution, carrying the exit code so the
/// MCP caller reads exactly the same meaning `loom-cli` reports.
pub fn tool_result_json(execution: &Execution) -> Value {
    let ok = execution.exit_code == 0;
    let mut object = serde_json::Map::new();
    object.insert("verb".to_owned(), Value::String(execution.verb.clone()));
    object.insert("exit_code".to_owned(), Value::from(execution.exit_code));
    object.insert("ok".to_owned(), Value::Bool(ok));
    object.insert("output".to_owned(), Value::String(execution.stdout.clone()));
    object.insert("stderr".to_owned(), Value::String(execution.stderr.clone()));
    match &execution.result {
        Ok(outcome) => {
            object.insert("replayed".to_owned(), Value::Bool(outcome.replayed));
            object.insert("data".to_owned(), outcome.data.clone());
        }
        Err(failure) => {
            object.insert(
                "error".to_owned(),
                json!({
                    "code": failure.code(),
                    "message": failure.message(),
                    "retryable": failure.retryable(),
                }),
            );
        }
    }
    Value::Object(object)
}

/// Handles one `tools/call` request against the worker.
pub async fn call_tool(state: &AppState, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("tools/call requires a 'name'"))?;
    if !ALL_TOOLS.contains(&name) {
        return Err(RpcError::invalid_params(format!("unknown tool '{name}'")));
    }
    if !tool_allowed(name, &state.config) {
        return Err(RpcError::invalid_params(format!(
            "tool '{name}' is disabled; mutating tools require --allow-mutating-tools"
        )));
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let argv = tool_argv(name, &arguments).map_err(RpcError::invalid_params)?;
    let execution = runner::run_verb(&state.config, &argv).await;
    let result = tool_result_json(&execution);
    let text = result.to_string();
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": execution.exit_code != 0,
        "structuredContent": result,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_cli::cli::{Failure, Outcome};

    const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
    const RUN: &str = "22222222-2222-4222-8222-222222222222";
    const TOOL_CALL: &str = "33333333-3333-4333-8333-333333333333";
    const SESSION: &str = "55555555-5555-4555-8555-555555555555";
    const SESSION_PREFIX: &str = "08bb3935";

    fn config(read_only: bool, allow_mutating: bool) -> crate::config::Config {
        crate::config::Config {
            connect: "ws://127.0.0.1:1/ws".to_owned(),
            token: crate::config::Secret::new("token".to_owned()).unwrap(),
            ca_file: None,
            allow_insecure_remote: false,
            max_frame_bytes: None,
            bind: "127.0.0.1:8766".parse().unwrap(),
            tls: None,
            read_only,
            allow_mutating,
            allowed_hosts: Vec::new(),
        }
    }

    #[test]
    fn argv_maps_each_tool_to_its_cli_spelling() {
        assert_eq!(tool_argv("ls", &json!({})).unwrap(), vec!["ls"]);
        assert_eq!(
            tool_argv("ls", &json!({ "workspace": WORKSPACE, "archived": true })).unwrap(),
            vec!["ls", "--workspace", WORKSPACE, "--archived"]
        );
        assert_eq!(
            tool_argv(
                "status",
                &json!({ "session": SESSION_PREFIX, "format": "verb", "tree": true })
            )
            .unwrap(),
            vec![
                "status",
                "--session",
                SESSION_PREFIX,
                "--format",
                "verb",
                "--tree"
            ]
        );
        assert_eq!(
            tool_argv(
                "run",
                &json!({
                    "workspace": WORKSPACE,
                    "task": "hi",
                    "model": "deterministic/demo",
                    "new": true,
                    "child_model_for": ["vision=deterministic/demo"],
                })
            )
            .unwrap(),
            vec![
                "run",
                "--workspace",
                WORKSPACE,
                "--task",
                "hi",
                "--model",
                "deterministic/demo",
                "--new",
                "--child-model-for",
                "vision=deterministic/demo",
            ]
        );
        assert_eq!(
            tool_argv("send", &json!({ "run": RUN, "text": "go" })).unwrap(),
            vec!["send", "--run", RUN, "--text", "go"]
        );
        assert_eq!(
            tool_argv("approve", &json!({ "run": RUN, "tool_call": TOOL_CALL })).unwrap(),
            vec!["approve", "--run", RUN, "--tool-call", TOOL_CALL]
        );
        assert_eq!(
            tool_argv(
                "deny",
                &json!({ "run": RUN, "tool_call": TOOL_CALL, "reason": "no" })
            )
            .unwrap(),
            vec![
                "deny",
                "--run",
                RUN,
                "--tool-call",
                TOOL_CALL,
                "--reason",
                "no"
            ]
        );
        assert_eq!(
            tool_argv("archive", &json!({ "session": SESSION, "yes": true })).unwrap(),
            vec!["session", "archive", "--yes", "--", SESSION]
        );
        assert_eq!(
            tool_argv("models_list", &json!({ "provider": "openai" })).unwrap(),
            vec!["models", "list", "--provider", "openai"]
        );
        assert_eq!(
            tool_argv("providers_list", &json!({})).unwrap(),
            vec!["providers", "list"]
        );
        assert_eq!(
            tool_argv(
                "copilot_login_start",
                &json!({ "provider": "github-copilot", "no_wait": true })
            )
            .unwrap(),
            vec![
                "providers",
                "login",
                "--provider",
                "github-copilot",
                "--no-wait"
            ]
        );
        assert_eq!(
            tool_argv("copilot_login_start", &json!({ "timeout": 45 })).unwrap(),
            vec![
                "providers",
                "login",
                "--provider",
                "github-copilot",
                "--timeout",
                "45"
            ]
        );
        assert_eq!(
            tool_argv("copilot_login_status", &json!({ "login_id": "login-42" })).unwrap(),
            vec!["providers", "login-status", "--login-id", "login-42"]
        );
    }

    #[test]
    fn argv_requires_the_required_arguments() {
        assert!(tool_argv("status", &json!({})).is_err());
        assert!(tool_argv("run", &json!({ "workspace": WORKSPACE })).is_err());
        assert!(
            tool_argv(
                "run",
                &json!({ "workspace": WORKSPACE, "task": "t", "task_file": "f" })
            )
            .is_err()
        );
        assert!(tool_argv("send", &json!({ "run": RUN })).is_err());
        assert!(tool_argv("approve", &json!({ "run": RUN })).is_err());
        assert!(tool_argv("archive", &json!({})).is_err());
        assert!(tool_argv("copilot_login_status", &json!({})).is_err());
        assert!(
            tool_argv(
                "status",
                &json!({ "session": SESSION_PREFIX, "format": "bogus" })
            )
            .is_err()
        );
    }

    #[test]
    fn argv_rejects_unknown_wrong_typed_and_flag_like_arguments() {
        assert!(tool_argv("ls", &json!({ "bogus": true })).is_err());
        assert!(tool_argv("ls", &json!({ "archived": "yes" })).is_err());
        assert!(tool_argv("status", &json!({ "session": 7 })).is_err());
        assert!(tool_argv("send", &json!({ "run": RUN, "text": 1 })).is_err());
        assert!(tool_argv("run", &json!(["not", "an", "object"])).is_err());
        assert!(tool_argv("run", &json!({ "workspace": WORKSPACE, "task": "-h" })).is_ok());
        assert!(
            tool_argv(
                "run",
                &json!({ "workspace": WORKSPACE, "task": "t", "as_directory": "/etc" })
            )
            .is_err()
        );
        assert!(
            tool_argv(
                "run",
                &json!({ "workspace": WORKSPACE, "task": "t", "approval_policy": "auto" })
            )
            .is_err()
        );
        assert!(tool_argv("status", &json!({ "session": "-abc" })).is_err());
        assert!(tool_argv("send", &json!({ "run": "-h", "text": "go" })).is_err());
        assert!(tool_argv("copilot_login_start", &json!({ "provider": "openai" })).is_err());
        assert!(
            tool_argv(
                "copilot_login_start",
                &json!({ "no_wait": true, "timeout": 10 })
            )
            .is_err()
        );
        assert!(tool_argv("copilot_login_start", &json!({ "timeout": 0 })).is_err());
        assert!(tool_argv("copilot_login_start", &json!({ "timeout": -1 })).is_err());
        assert!(tool_argv("copilot_login_start", &json!({ "access_token": "x" })).is_err());
        assert!(tool_argv("copilot_login_status", &json!({ "login_id": "-h" })).is_err());
        assert!(tool_argv("copilot_login_status", &json!({ "login_id": "   " })).is_err());
    }

    #[test]
    fn mutating_tools_are_known_and_opt_in() {
        let hidden = config(false, false);
        let enabled = config(false, true);
        let read_only = config(true, true);
        for name in MUTATING_TOOLS {
            assert!(is_mutating(name), "{name}");
            assert!(!tool_allowed(name, &hidden), "{name} must be opt-in");
            assert!(tool_allowed(name, &enabled), "{name}");
            assert!(
                !tool_allowed(name, &read_only),
                "{name} must stay read-only"
            );
        }
        for name in ["ls", "status", "models_list", "providers_list"] {
            assert!(!is_mutating(name), "{name}");
            assert!(tool_allowed(name, &hidden), "{name} must stay readable");
            assert!(tool_allowed(name, &read_only), "{name} must stay readable");
        }
    }

    #[test]
    fn tool_result_carries_the_exit_meaning() {
        let ok = Execution {
            verb: "ls".to_owned(),
            exit_code: 0,
            result: Ok(Outcome::new(json!({ "workspaces": [] }), None)),
            stdout: "out\n".to_owned(),
            stderr: String::new(),
        };
        let value = tool_result_json(&ok);
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["ok"], true);
        assert_eq!(value["output"], "out\n");
        assert_eq!(value["data"]["workspaces"], json!([]));

        let failed = Execution {
            verb: "status".to_owned(),
            exit_code: 2,
            result: Err(Failure::Usage("bad".to_owned())),
            stdout: String::new(),
            stderr: "error invalid_request bad\n".to_owned(),
        };
        let value = tool_result_json(&failed);
        assert_eq!(value["exit_code"], 2);
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "invalid_request");
        assert_eq!(value["error"]["message"], "bad");
    }
}
