//! The first-slice tool surface: one tool per `loom-cli` verb.

use loom_cli::cli::Execution;
use serde_json::{Value, json};

use crate::config::AppState;
use crate::jsonrpc::RpcError;
use crate::runner;

/// Tool names that mutate the worker, kept out of read-only mode and annotated
/// so an MCP host can require per-call approval.
pub const MUTATING_TOOLS: &[&str] = &["run", "send", "approve", "deny", "archive"];
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

/// Whether a tool may be called under the current read-only policy.
pub fn tool_allowed(name: &str, read_only: bool) -> bool {
    !read_only || !is_mutating(name)
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
                    "task": { "type": "string", "description": "Task text." },
                    "task_file": { "type": "string", "description": "File holding the task text." },
                    "name": { "type": "string", "description": "Session name to reuse or create." },
                    "model": { "type": "string", "description": "Model id, or derive from the worker." },
                    "repo": { "type": "string", "description": "Repository source to attach to a new session." },
                    "revision": { "type": "string", "description": "Revision to clone; --repo only." },
                    "path": { "type": "string", "description": "Session path the input lands at." },
                    "as_directory": { "type": "string", "description": "Local directory to mount as the input." },
                    "approval_policy": { "type": "string", "enum": ["default", "auto"] },
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
                "required": ["workspace"],
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
    ]
}

fn required(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("missing required argument '{key}'"))
}

fn string(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn boolean(arguments: &Value, key: &str) -> bool {
    arguments.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn push_flag(out: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        out.push(flag.to_owned());
        out.push(value.to_owned());
    }
}

/// Maps one MCP tool name and its arguments to the `loom-cli` command line.
pub fn tool_argv(name: &str, arguments: &Value) -> Result<Vec<String>, String> {
    let mut argv: Vec<String> = Vec::new();
    match name {
        "ls" => {
            argv.push("ls".to_owned());
            push_flag(
                &mut argv,
                "--workspace",
                string(arguments, "workspace").as_deref(),
            );
            if boolean(arguments, "archived") {
                argv.push("--archived".to_owned());
            }
        }
        "status" => {
            let session = required(arguments, "session")?;
            argv.extend(["status".to_owned(), "--session".to_owned(), session]);
            match string(arguments, "format").as_deref() {
                Some("verb") => argv.extend(["--format".to_owned(), "verb".to_owned()]),
                Some("text") | None => {}
                Some(other) => return Err(format!("'format' must be text or verb, got '{other}'")),
            }
            if boolean(arguments, "tree") {
                argv.push("--tree".to_owned());
            }
        }
        "run" => {
            let workspace = required(arguments, "workspace")?;
            argv.extend(["run".to_owned(), "--workspace".to_owned(), workspace]);
            let task = string(arguments, "task");
            let task_file = string(arguments, "task_file");
            match (task, task_file) {
                (Some(task), None) => argv.extend(["--task".to_owned(), task]),
                (None, Some(path)) => argv.extend(["--task-file".to_owned(), path]),
                (None, None) => return Err("run requires 'task' or 'task_file'".to_owned()),
                (Some(_), Some(_)) => {
                    return Err("run accepts 'task' or 'task_file', not both".to_owned());
                }
            }
            push_flag(&mut argv, "--name", string(arguments, "name").as_deref());
            push_flag(&mut argv, "--model", string(arguments, "model").as_deref());
            push_flag(&mut argv, "--repo", string(arguments, "repo").as_deref());
            push_flag(
                &mut argv,
                "--revision",
                string(arguments, "revision").as_deref(),
            );
            push_flag(&mut argv, "--path", string(arguments, "path").as_deref());
            push_flag(
                &mut argv,
                "--as-directory",
                string(arguments, "as_directory").as_deref(),
            );
            push_flag(
                &mut argv,
                "--approval-policy",
                string(arguments, "approval_policy").as_deref(),
            );
            push_flag(
                &mut argv,
                "--request-id",
                string(arguments, "request_id").as_deref(),
            );
            if boolean(arguments, "new") {
                argv.push("--new".to_owned());
            }
            push_flag(
                &mut argv,
                "--project",
                string(arguments, "project").as_deref(),
            );
            push_flag(
                &mut argv,
                "--child-model",
                string(arguments, "child_model").as_deref(),
            );
            if let Some(entries) = arguments.get("child_model_for").and_then(Value::as_array) {
                for entry in entries {
                    match entry.as_str() {
                        Some(value) => {
                            argv.extend(["--child-model-for".to_owned(), value.to_owned()])
                        }
                        None => return Err("'child_model_for' entries must be strings".to_owned()),
                    }
                }
            }
        }
        "send" => {
            let run = required(arguments, "run")?;
            let text = required(arguments, "text")?;
            argv.extend([
                "send".to_owned(),
                "--run".to_owned(),
                run,
                "--text".to_owned(),
                text,
            ]);
            push_flag(
                &mut argv,
                "--request-id",
                string(arguments, "request_id").as_deref(),
            );
        }
        "approve" => {
            let run = required(arguments, "run")?;
            let tool_call = required(arguments, "tool_call")?;
            argv.extend([
                "approve".to_owned(),
                "--run".to_owned(),
                run,
                "--tool-call".to_owned(),
                tool_call,
            ]);
            push_flag(
                &mut argv,
                "--request-id",
                string(arguments, "request_id").as_deref(),
            );
        }
        "deny" => {
            let run = required(arguments, "run")?;
            let tool_call = required(arguments, "tool_call")?;
            argv.extend([
                "deny".to_owned(),
                "--run".to_owned(),
                run,
                "--tool-call".to_owned(),
                tool_call,
            ]);
            push_flag(
                &mut argv,
                "--reason",
                string(arguments, "reason").as_deref(),
            );
            push_flag(
                &mut argv,
                "--request-id",
                string(arguments, "request_id").as_deref(),
            );
        }
        "archive" => {
            let session = required(arguments, "session")?;
            argv.extend(["session".to_owned(), "archive".to_owned(), session]);
            if boolean(arguments, "yes") {
                argv.push("--yes".to_owned());
            }
            push_flag(
                &mut argv,
                "--request-id",
                string(arguments, "request_id").as_deref(),
            );
        }
        "models_list" => {
            argv.extend(["models".to_owned(), "list".to_owned()]);
            push_flag(
                &mut argv,
                "--provider",
                string(arguments, "provider").as_deref(),
            );
        }
        "providers_list" => {
            argv.extend(["providers".to_owned(), "list".to_owned()]);
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
    if !tool_allowed(name, state.config.read_only) {
        return Err(RpcError::invalid_params(format!(
            "tool '{name}' is disabled in read-only mode"
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

    #[test]
    fn argv_maps_each_tool_to_its_cli_spelling() {
        assert_eq!(tool_argv("ls", &json!({})).unwrap(), vec!["ls"]);
        assert_eq!(
            tool_argv("ls", &json!({ "workspace": "w1", "archived": true })).unwrap(),
            vec!["ls", "--workspace", "w1", "--archived"]
        );
        assert_eq!(
            tool_argv(
                "status",
                &json!({ "session": "s1", "format": "verb", "tree": true })
            )
            .unwrap(),
            vec!["status", "--session", "s1", "--format", "verb", "--tree"]
        );
        assert_eq!(
            tool_argv(
                "run",
                &json!({
                    "workspace": "w1",
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
                "w1",
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
            tool_argv("send", &json!({ "run": "r1", "text": "go" })).unwrap(),
            vec!["send", "--run", "r1", "--text", "go"]
        );
        assert_eq!(
            tool_argv("approve", &json!({ "run": "r1", "tool_call": "t1" })).unwrap(),
            vec!["approve", "--run", "r1", "--tool-call", "t1"]
        );
        assert_eq!(
            tool_argv(
                "deny",
                &json!({ "run": "r1", "tool_call": "t1", "reason": "no" })
            )
            .unwrap(),
            vec!["deny", "--run", "r1", "--tool-call", "t1", "--reason", "no"]
        );
        assert_eq!(
            tool_argv("archive", &json!({ "session": "s1", "yes": true })).unwrap(),
            vec!["session", "archive", "s1", "--yes"]
        );
        assert_eq!(
            tool_argv("models_list", &json!({ "provider": "openai" })).unwrap(),
            vec!["models", "list", "--provider", "openai"]
        );
        assert_eq!(
            tool_argv("providers_list", &json!({})).unwrap(),
            vec!["providers", "list"]
        );
    }

    #[test]
    fn argv_requires_the_required_arguments() {
        assert!(tool_argv("status", &json!({})).is_err());
        assert!(tool_argv("run", &json!({ "workspace": "w1" })).is_err());
        assert!(
            tool_argv(
                "run",
                &json!({ "workspace": "w1", "task": "t", "task_file": "f" })
            )
            .is_err()
        );
        assert!(tool_argv("send", &json!({ "run": "r1" })).is_err());
        assert!(tool_argv("approve", &json!({ "run": "r1" })).is_err());
        assert!(tool_argv("archive", &json!({})).is_err());
        assert!(tool_argv("status", &json!({ "session": "s1", "format": "bogus" })).is_err());
    }

    #[test]
    fn mutating_tools_are_known_and_gated() {
        for name in MUTATING_TOOLS {
            assert!(is_mutating(name), "{name}");
            assert!(!tool_allowed(name, true), "{name} must be gated read-only");
            assert!(tool_allowed(name, false), "{name}");
        }
        for name in ["ls", "status", "models_list", "providers_list"] {
            assert!(!is_mutating(name), "{name}");
            assert!(tool_allowed(name, true), "{name} must stay readable");
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
