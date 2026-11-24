use super::*;

// Lines trimmed from a real `claude -p --output-format stream-json` run.
const INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/work","session_id":"s-1","tools":["Bash"],"model":"claude-sonnet-5-5","permissionMode":"default"}"#;
const TEXT_DELTA: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}},"session_id":"s-1","parent_tool_use_id":null}"#;
const THINKING_DELTA: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}},"parent_tool_use_id":"toolu_p"}"#;
const MESSAGE_STOP: &str = r#"{"type":"stream_event","event":{"type":"message_stop"}}"#;
const ASSISTANT_TOOL: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"running"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"touch x"}}]},"parent_tool_use_id":null}"#;
const TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"done","is_error":false}]}}"#;
const TOOL_RESULT_PARTS: &str = r#"{"type":"user","message":{"content":[{"tool_use_id":"toolu_2","type":"tool_result","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}],"is_error":true}]}}"#;
const PERMISSION: &str = r#"{"type":"control_request","request_id":"r-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch x"},"description":"Create x"}}"#;
const DENIED: &str = r#"{"type":"system","subtype":"permission_denied","tool_name":"Bash","tool_use_id":"toolu_3","message":"needs approval"}"#;
const RATE_LIMIT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","isUsingOverage":false}}"#;
const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","stop_reason":"end_turn","session_id":"s-1","total_cost_usd":0.01,"num_turns":2,"duration_ms":2265,"usage":{"input_tokens":2,"cache_creation_input_tokens":2662,"cache_read_input_tokens":649,"output_tokens":4}}"#;

fn one(line: &str) -> ClaudeEvent {
    let mut events = parse_line(line).unwrap();
    assert_eq!(events.len(), 1, "{line}");
    events.remove(0)
}

#[test]
fn lines_decode_into_events() {
    assert_eq!(
        one(INIT),
        ClaudeEvent::Init {
            session_id: "s-1".into(),
            model: Some("claude-sonnet-5-5".into()),
            tools: vec!["Bash".into()],
            permission_mode: Some("default".into()),
        }
    );
    assert_eq!(
        one(TEXT_DELTA),
        ClaudeEvent::TextDelta {
            text: "ok".into(),
            parent_tool_use_id: None
        }
    );
    assert_eq!(
        one(THINKING_DELTA),
        ClaudeEvent::ThinkingDelta {
            text: "hm".into(),
            parent_tool_use_id: Some("toolu_p".into())
        }
    );
    assert_eq!(
        one(PERMISSION),
        ClaudeEvent::PermissionRequest {
            request_id: "r-1".into(),
            tool_name: "Bash".into(),
            input: json!({"command": "touch x"}),
            description: Some("Create x".into()),
        }
    );
    assert_eq!(
        one(DENIED),
        ClaudeEvent::PermissionDenied {
            tool_use_id: Some("toolu_3".into()),
            tool_name: "Bash".into(),
            message: "needs approval".into()
        }
    );
    assert_eq!(
        one(RATE_LIMIT),
        ClaudeEvent::RateLimit {
            status: "allowed".into(),
            window: Some("five_hour".into()),
            is_using_overage: false
        }
    );
    assert_eq!(
        one(r#"{"type":"control_cancel_request","request_id":"r-1"}"#),
        ClaudeEvent::PermissionCancelled {
            request_id: "r-1".into()
        }
    );
}

#[test]
fn an_assistant_message_reports_only_its_tool_calls() {
    // Its text already arrived as deltas.
    assert_eq!(
        one(ASSISTANT_TOOL),
        ClaudeEvent::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({"command": "touch x"}),
            parent_tool_use_id: None,
        }
    );
    assert!(
        parse_line(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"x"}]}}"#)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn tool_results_read_string_and_part_content() {
    assert_eq!(
        one(TOOL_RESULT),
        ClaudeEvent::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "done".into(),
            is_error: false,
            parent_tool_use_id: None,
        }
    );
    assert_eq!(
        one(TOOL_RESULT_PARTS),
        ClaudeEvent::ToolResult {
            tool_use_id: "toolu_2".into(),
            content: "a\nb".into(),
            is_error: true,
            parent_tool_use_id: None,
        }
    );
}

#[test]
fn a_result_carries_usage_and_cost() {
    assert_eq!(
        one(RESULT),
        ClaudeEvent::Result(ResultSummary {
            is_error: false,
            subtype: "success".into(),
            result: Some("done".into()),
            stop_reason: Some("end_turn".into()),
            session_id: Some("s-1".into()),
            total_cost_usd: Some(0.01),
            usage: TokenCounts {
                input: 2,
                output: 4,
                cache_read: 649,
                cache_creation: 2662
            },
            num_turns: Some(2),
            duration_ms: Some(2265),
        })
    );
}

#[test]
fn unused_lines_are_other_and_garbage_is_an_error() {
    assert_eq!(
        one(MESSAGE_STOP),
        ClaudeEvent::Other {
            kind: "stream_event".into()
        }
    );
    assert_eq!(
        one(r#"{"type":"system","subtype":"status"}"#),
        ClaudeEvent::Other {
            kind: "system/status".into()
        }
    );
    assert_eq!(
        one(r#"{"type":"control_request","request_id":"1","request":{"subtype":"initialize"}}"#),
        ClaudeEvent::Other {
            kind: "control_request".into()
        }
    );
    assert_eq!(
        parse_line("not json").unwrap_err().code,
        ErrorCode::MalformedPayload
    );
    assert_eq!(
        parse_line(r#"{"no":"type"}"#).unwrap_err().code,
        ErrorCode::MalformedPayload
    );
    assert_eq!(
        parse_line(r#"{"type":"control_request","request":{"subtype":"can_use_tool"}}"#)
            .unwrap_err()
            .code,
        ErrorCode::MalformedPayload
    );
}

#[test]
fn the_command_line_asks_for_streaming_json_with_host_permissions() {
    let mut options = ClaudeCodeOptions::new("/work");
    let args = options.args();
    for pair in [
        ["--input-format", "stream-json"],
        ["--output-format", "stream-json"],
        ["--permission-prompt-tool", "stdio"],
        ["--permission-mode", "default"],
    ] {
        assert!(args.windows(2).any(|window| window == pair), "{pair:?}");
    }
    assert!(args.contains(&"--include-partial-messages".to_owned()));
    assert!(!args.contains(&"--model".to_owned()));
    assert!(!args.contains(&"--tools".to_owned()));

    options.model = Some("opus".into());
    options.resume = Some("s-1".into());
    options.permission_mode = PermissionMode::AcceptEdits;
    options.tools = Some(vec![]);
    options.append_system_prompt = Some("be brief".into());
    options.mcp_config = Some("{}".into());
    options.setting_sources = Some("project".into());
    let args = options.args();
    for pair in [
        ["--model", "opus"],
        ["--resume", "s-1"],
        ["--permission-mode", "acceptEdits"],
        ["--tools", ""],
        ["--append-system-prompt", "be brief"],
        ["--mcp-config", "{}"],
        ["--setting-sources", "project"],
    ] {
        assert!(args.windows(2).any(|window| window == pair), "{pair:?}");
    }
    for (mode, flag) in [
        (PermissionMode::Plan, "plan"),
        (PermissionMode::DontAsk, "dontAsk"),
    ] {
        options.permission_mode = mode;
        assert!(
            options
                .args()
                .windows(2)
                .any(|window| window == ["--permission-mode", flag])
        );
    }
}

#[test]
fn a_missing_binary_names_what_to_install() {
    let mut options = ClaudeCodeOptions::new(std::env::temp_dir());
    options.binary = PathBuf::from("/nonexistent/claude");
    let error = ClaudeCodeSession::spawn(&options).err().unwrap();
    assert_eq!(error.code, ErrorCode::ProviderUnavailable);
    assert!(error.message.contains("/nonexistent/claude"));
}

#[test]
fn stderr_keeps_only_its_tail() {
    let tail = Mutex::new(String::new());
    let text = "é".repeat(STDERR_TAIL_BYTES);
    drain_stderr(text.as_bytes(), &tail);
    let tail = tail.lock().unwrap();
    assert!(tail.len() <= STDERR_TAIL_BYTES);
    assert!(tail.chars().all(|character| character == 'é'));
}

/// A stand-in `claude` that plays one scripted turn. Run in one test because a
/// freshly written script must not race another thread's `fork`.
#[cfg(unix)]
#[test]
fn a_session_streams_a_turn_and_carries_the_permission_answer() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("loom-harness-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    let body = format!(
        r#"#!/bin/sh
read first
printf '%s\n' '{INIT}'
printf '%s\n' '{TEXT_DELTA}'
printf '%s\n' '{ASSISTANT_TOOL}'
printf '%s\n' '{PERMISSION}'
read answer
case "$answer" in
  *'"behavior":"allow"'*) printf '%s\n' '{TOOL_RESULT}' ;;
  *) printf '%s\n' '{DENIED}' ;;
esac
echo "saw: $first" >&2
printf '%s\n' '{RESULT}'
read interrupt
echo "$interrupt" >&2
"#
    );
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    for (decision, expect_result) in [
        (
            PermissionDecision::Allow {
                input: json!({"command": "touch x"}),
            },
            true,
        ),
        (
            PermissionDecision::Deny {
                message: "no".into(),
            },
            false,
        ),
    ] {
        let mut options = ClaudeCodeOptions::new(&dir);
        options.binary = script.clone();
        let mut session = ClaudeCodeSession::spawn(&options).unwrap();
        session.send_user_message("hello").unwrap();

        let mut events = Vec::new();
        loop {
            match session.next_event(Duration::from_secs(10)) {
                Next::Event(ClaudeEvent::PermissionRequest { request_id, .. }) => {
                    assert_eq!(request_id, "r-1");
                    session.respond_permission(&request_id, &decision).unwrap();
                }
                Next::Event(event @ ClaudeEvent::Result(_)) => {
                    events.push(event);
                    break;
                }
                Next::Event(event) => events.push(event),
                other => panic!("the turn ended early: {other:?}"),
            }
        }
        assert!(matches!(events[0], ClaudeEvent::Init { .. }));
        assert!(matches!(events[1], ClaudeEvent::TextDelta { .. }));
        assert!(matches!(events[2], ClaudeEvent::ToolUse { .. }));
        assert_eq!(
            matches!(events[3], ClaudeEvent::ToolResult { .. }),
            expect_result
        );
        assert_eq!(
            matches!(events[3], ClaudeEvent::PermissionDenied { .. }),
            !expect_result
        );

        // The interrupt request reaches the child as a control request.
        session.interrupt().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !session.stderr_tail().contains("\"subtype\":\"interrupt\"") {
            assert!(std::time::Instant::now() < deadline, "no interrupt seen");
            thread::sleep(Duration::from_millis(10));
        }
        let seen = session.stderr_tail();
        assert!(seen.contains("saw: ") && seen.contains("\"content\":\"hello\""));
        session.kill();
        assert!(session.write_line(&json!({})).is_err());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
