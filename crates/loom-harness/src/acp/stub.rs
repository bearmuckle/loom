//! Reusable stdio JSON-RPC 2.0 stub ACP harness for tests.
//!
//! Speaks newline-delimited ACP v1 JSON-RPC 2.0 over stdin/stdout so both
//! `loom-harness` and `loom-server` tests can drive a real subprocess without
//! requiring a live `agy` binary or real Google sign-in.

use std::{
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Write},
};

use serde_json::{Value, json};

/// Runs the stub ACP agent loop on stdin/stdout.
///
/// Behavior is controlled via environment variables:
/// - `LOOM_ACP_STUB_RECORD`: optional file path where every received JSON-RPC
///   request (plus initial cwd/profile env) is appended as JSON lines.
/// - `LOOM_ACP_STUB_AUTH_METHODS`: comma-separated advertised auth method ids
///   (default: `"oauth-personal,gemini-api-key"`).
/// - `LOOM_ACP_STUB_FAIL_AUTH`: if set, `authenticate` returns a JSON-RPC error.
/// - `LOOM_ACP_STUB_LOAD_SESSION`: if `"1"`, advertises `loadSession: true`.
/// - `LOOM_ACP_STUB_SCENARIO`:
///   - `"basic"` (default): streams thought + plan + tool_call + tool_call_update
///     + usage_update + message chunks → `end_turn`.
///   - `"permission"`: streams tool_call → sends `session/request_permission`
///     (options `allow-once` / `reject-once`), waits for host response, then
///     streams outcome text and completes.
///   - `"cancel"`: streams a chunk, then waits until `session/cancel` arrives
///     and replies with `stopReason: "cancelled"`.
///   - `"crash"`: writes stderr, emits a chunk, then exits immediately with code 42.
///   - `"stderr"`: writes a stderr line during prompt before completing.
pub fn run_stub_stdio() {
    let record_path = std::env::var("LOOM_ACP_STUB_RECORD").ok();
    let auth_methods_raw = std::env::var("LOOM_ACP_STUB_AUTH_METHODS")
        .unwrap_or_else(|_| "oauth-personal,gemini-api-key".to_owned());
    let fail_auth = std::env::var("LOOM_ACP_STUB_FAIL_AUTH").ok();
    let load_session = std::env::var("LOOM_ACP_STUB_LOAD_SESSION").as_deref() == Ok("1");
    let scenario = std::env::var("LOOM_ACP_STUB_SCENARIO").unwrap_or_else(|_| "basic".to_owned());

    let record = |entry: &Value| {
        if let Some(path) = &record_path
            && let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = writeln!(file, "{entry}");
        }
    };

    record(&json!({
        "stub_started": true,
        "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
        "profile_dir": std::env::var("LOOM_ACP_PROFILE_DIR").ok(),
    }));

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut line = String::new();
    let mut session_id = "acp-stub-sess-1".to_owned();

    let send = |stdout: &mut io::Stdout, msg: &Value| {
        let _ = writeln!(stdout, "{msg}");
        let _ = stdout.flush();
    };

    loop {
        line.clear();
        let Ok(read) = reader.read_line(&mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        record(&msg);

        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            Some("initialize") => {
                let methods = auth_methods_raw
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|m| json!({ "id": m, "name": m }))
                    .collect::<Vec<_>>();
                send(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": 1,
                            "agentInfo": {
                                "name": "antigravity-stub",
                                "version": "0.0.1-test"
                            },
                            "agentCapabilities": {
                                "loadSession": load_session,
                                "promptCapabilities": {
                                    "image": false,
                                    "audio": false,
                                    "embeddedContext": false
                                },
                                "sessionCapabilities": {}
                            },
                            "authMethods": methods
                        }
                    }),
                );
            }
            Some("authenticate") => {
                let method_id = params
                    .get("methodId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(reason) = &fail_auth {
                    let _ = writeln!(stderr, "stub auth failed for {method_id}: {reason}");
                    let _ = stderr.flush();
                    send(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32000,
                                "message": reason
                            }
                        }),
                    );
                } else {
                    send(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {}
                        }),
                    );
                }
            }
            Some("session/new") => {
                send(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "sessionId": session_id,
                            "models": {
                                "currentModelId": "gemini-catalog-pro",
                                "availableModels": [
                                    {
                                        "modelId": "gemini-catalog-pro",
                                        "name": "Gemini Catalog Pro",
                                        "description": "Reported from harness catalog"
                                    },
                                    {
                                        "modelId": "gemini-catalog-flash",
                                        "name": "Gemini Catalog Flash"
                                    }
                                ]
                            }
                        }
                    }),
                );
            }
            Some("session/load") | Some("session/resume") => {
                if let Some(requested) = params.get("sessionId").and_then(Value::as_str) {
                    session_id = requested.to_owned();
                }
                send(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "sessionId": session_id
                        }
                    }),
                );
            }
            Some("session/set_model") | Some("session/set_config_option") => {
                send(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {}
                    }),
                );
            }
            Some("session/prompt") => {
                let prompt_text = params
                    .get("prompt")
                    .and_then(Value::as_array)
                    .and_then(|arr| arr.first())
                    .and_then(|p| p.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();

                match scenario.as_str() {
                    "crash" => {
                        let _ = writeln!(stderr, "fatal harness crash mid-turn");
                        let _ = stderr.flush();
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": "partial before crash" }
                                    }
                                }
                            }),
                        );
                        std::process::exit(42);
                    }
                    "hang" => {
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": "wedged..." }
                                    }
                                }
                            }),
                        );
                        // Deliberately ignore session/cancel and wait until killed
                        // so cancel-grace kill and turn-timeout paths are testable.
                        loop {
                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) | Err(_) => return,
                                Ok(_) => {
                                    if let Ok(incoming) = serde_json::from_str::<Value>(line.trim())
                                    {
                                        record(&incoming);
                                    }
                                }
                            }
                        }
                    }
                    "unsupported_req" => {
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": "client-req-42",
                                "method": "custom/unsupported_ping",
                                "params": {}
                            }),
                        );
                        loop {
                            line.clear();
                            let Ok(n) = reader.read_line(&mut line) else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            if let Ok(incoming) = serde_json::from_str::<Value>(line.trim()) {
                                record(&incoming);
                                if incoming.get("id") == Some(&json!("client-req-42")) {
                                    break;
                                }
                            }
                        }
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "stopReason": "end_turn" }
                            }),
                        );
                    }
                    "prompt_error" => {
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32001,
                                    "message": "stub prompt failure"
                                }
                            }),
                        );
                    }
                    "secret_stderr" => {
                        let _ = writeln!(
                            stderr,
                            "Login failed: Bearer ya29.a0AfH6SM_secret_oauth_token_123 \
                             key=AIzaSyD1234567890abcdef1234567890 \
                             url=https://accounts.google.com/o/oauth2/auth?client_id=ok&code=4/0AX_secret_code&access_token=tok_xyz \
                             GEMINI_API_KEY=AIzaSyAnotherSecretKey9999999"
                        );
                        let _ = stderr.flush();
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": "done after stderr" }
                                    }
                                }
                            }),
                        );
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "stopReason": "end_turn" }
                            }),
                        );
                    }
                    "cancel" => {
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": "working..." }
                                    }
                                }
                            }),
                        );
                        loop {
                            line.clear();
                            let Ok(n) = reader.read_line(&mut line) else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            if let Ok(incoming) = serde_json::from_str::<Value>(line.trim()) {
                                record(&incoming);
                                if incoming.get("method").and_then(Value::as_str)
                                    == Some("session/cancel")
                                {
                                    send(
                                        &mut stdout,
                                        &json!({
                                            "jsonrpc": "2.0",
                                            "id": id,
                                            "result": { "stopReason": "cancelled" }
                                        }),
                                    );
                                    break;
                                }
                            }
                        }
                    }
                    "permission" => {
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "tool_call",
                                        "toolCallId": "tc-perm-1",
                                        "title": "Run shell command",
                                        "kind": "execute",
                                        "status": "pending",
                                        "rawInput": { "command": "cargo test" }
                                    }
                                }
                            }),
                        );
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": "perm-req-1",
                                "method": "session/request_permission",
                                "params": {
                                    "sessionId": session_id,
                                    "toolCall": {
                                        "toolCallId": "tc-perm-1",
                                        "title": "Run shell command",
                                        "kind": "execute",
                                        "rawInput": { "command": "cargo test" }
                                    },
                                    "options": [
                                        {
                                            "optionId": "opt-allow-once",
                                            "name": "Allow once",
                                            "kind": "allow_once"
                                        },
                                        {
                                            "optionId": "opt-reject-once",
                                            "name": "Reject",
                                            "kind": "reject_once"
                                        }
                                    ]
                                }
                            }),
                        );
                        let mut reply_text = String::from("permission cancelled");
                        let mut final_status = "failed";
                        let mut cancelled = false;
                        loop {
                            line.clear();
                            let Ok(n) = reader.read_line(&mut line) else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            if let Ok(incoming) = serde_json::from_str::<Value>(line.trim()) {
                                record(&incoming);
                                if incoming.get("method").and_then(Value::as_str)
                                    == Some("session/cancel")
                                {
                                    cancelled = true;
                                    break;
                                }
                                if incoming.get("id") == Some(&json!("perm-req-1")) {
                                    let outcome = incoming
                                        .pointer("/result/outcome/outcome")
                                        .and_then(Value::as_str)
                                        .unwrap_or("cancelled");
                                    let option_id = incoming
                                        .pointer("/result/outcome/optionId")
                                        .and_then(Value::as_str);
                                    if outcome == "selected" && option_id == Some("opt-allow-once")
                                    {
                                        reply_text = "command allowed and executed".to_owned();
                                        final_status = "completed";
                                    } else if outcome == "selected"
                                        && option_id == Some("opt-reject-once")
                                    {
                                        reply_text = "command rejected by operator".to_owned();
                                        final_status = "failed";
                                    } else {
                                        cancelled = true;
                                    }
                                    break;
                                }
                            }
                        }
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "tool_call_update",
                                        "toolCallId": "tc-perm-1",
                                        "status": final_status,
                                        "rawOutput": reply_text
                                    }
                                }
                            }),
                        );
                        if !cancelled {
                            send(
                                &mut stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "method": "session/update",
                                    "params": {
                                        "sessionId": session_id,
                                        "update": {
                                            "sessionUpdate": "agent_message_chunk",
                                            "content": { "type": "text", "text": reply_text }
                                        }
                                    }
                                }),
                            );
                        }
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "stopReason": if cancelled { "cancelled" } else { "end_turn" }
                                }
                            }),
                        );
                    }
                    _ => {
                        if scenario == "stderr" {
                            let _ = writeln!(stderr, "harness diagnostic notice");
                            let _ = stderr.flush();
                        }
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "plan",
                                        "entries": [
                                            { "content": "Inspect workspace", "priority": "high", "status": "completed" },
                                            { "content": "Answer prompt", "priority": "medium", "status": "in_progress" }
                                        ]
                                    }
                                }
                            }),
                        );
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "agent_thought_chunk",
                                        "content": { "type": "text", "text": "thinking through request" }
                                    }
                                }
                            }),
                        );
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "tool_call",
                                        "toolCallId": "tc-read-1",
                                        "title": "Read README.md",
                                        "kind": "read",
                                        "status": "in_progress",
                                        "rawInput": { "path": "README.md" }
                                    }
                                }
                            }),
                        );
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "tool_call_update",
                                        "toolCallId": "tc-read-1",
                                        "status": "completed",
                                        "rawOutput": "README contents"
                                    }
                                }
                            }),
                        );
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "usage_update",
                                        "inputTokens": 42,
                                        "outputTokens": 17,
                                        "used": 59,
                                        "size": 100000,
                                        "cost": { "amount": 0.001, "currency": "USD" }
                                    }
                                }
                            }),
                        );
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": format!("Echo: {prompt_text}") }
                                    }
                                }
                            }),
                        );
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "stopReason": "end_turn"
                                }
                            }),
                        );
                    }
                }
            }
            Some("session/cancel") => {}
            Some(other) => {
                if let Some(id) = id {
                    send(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32601,
                                "message": format!("unsupported method: {other}")
                            }
                        }),
                    );
                }
            }
            None => {}
        }
    }
}
