use super::*;
use serde_json::json;

fn descriptor(alias: &str) -> ModelDescriptor {
    ProviderConfig::claude_code("claude")
        .models
        .into_iter()
        .find(|model| model.id.as_str() == format!("{MODEL_PREFIX}{alias}"))
        .unwrap()
}

fn request(messages: Vec<ModelMessage>) -> ModelRequest {
    ModelRequest {
        model: ModelId::new("claude-code:sonnet"),
        messages,
        tools: Vec::new(),
        options: Default::default(),
    }
}

fn message(role: MessageRole, text: &str) -> ModelMessage {
    ModelMessage::new(role, text)
}

#[test]
fn the_model_id_names_the_cli_alias() {
    let provider = |alias| ClaudeCodeProvider::new(descriptor(alias), "claude");
    assert_eq!(provider("sonnet").alias().as_deref(), Some("sonnet"));
    assert_eq!(provider("opus").alias().as_deref(), Some("opus"));
    assert_eq!(provider("default").alias(), None);
}

#[test]
fn a_first_turn_sends_the_user_message_and_the_system_text() {
    let mut provider = ClaudeCodeProvider::new(descriptor("sonnet"), "claude");
    let (prompt, system) = provider.next_prompt(&request(vec![
        message(MessageRole::System, "be brief"),
        message(MessageRole::User, "fix it"),
    ]));
    assert_eq!(prompt, "fix it");
    assert_eq!(system.as_deref(), Some("be brief"));
}

#[test]
fn a_fresh_conversation_with_history_gets_a_labelled_recap() {
    let mut provider = ClaudeCodeProvider::new(descriptor("sonnet"), "claude");
    let (prompt, system) = provider.next_prompt(&request(vec![
        message(MessageRole::User, "one"),
        message(MessageRole::Assistant, "two"),
        message(MessageRole::User, "three"),
    ]));
    assert!(prompt.contains("[User] one") && prompt.contains("[Assistant] two"));
    assert!(prompt.ends_with("Latest message:\nthree"));
    assert_eq!(system, None);
}

#[test]
fn a_continued_conversation_sends_only_what_the_cli_has_not_seen() {
    let mut provider = ClaudeCodeProvider::new(descriptor("sonnet"), "claude");
    provider.claude_session = Some("s-1".to_owned());
    provider.delivered = 2;
    let messages = vec![
        message(MessageRole::User, "one"),
        message(MessageRole::Assistant, "two"),
        message(MessageRole::User, "three"),
        message(MessageRole::Tool, "ignored"),
        message(MessageRole::User, "four"),
    ];
    let (prompt, system) = provider.next_prompt(&request(messages));
    assert_eq!(prompt, "three\n\nfour");
    assert_eq!(system, None);
    assert_eq!(provider.claude_session.as_deref(), Some("s-1"));

    // Messages the CLI had seen are gone: start over from the journal.
    let (prompt, _) = provider.next_prompt(&request(vec![message(MessageRole::User, "x")]));
    assert_eq!(prompt, "x");
    assert_eq!(provider.claude_session, None);
}

#[test]
fn tool_calls_are_summarised_on_one_short_line() {
    assert_eq!(
        tool_summary("Bash", &json!({"command": "ls -la\nrm x"})),
        "→ Bash: ls -la\n"
    );
    assert_eq!(tool_summary("Task", &json!({})), "→ Task\n");
    let long = "é".repeat(SUMMARY_BYTES);
    let summary = tool_summary("Read", &json!({"file_path": long}));
    assert!(summary.ends_with("…\n") && summary.len() < SUMMARY_BYTES + 16);
}

#[test]
fn a_provider_without_a_workspace_refuses_to_stream() {
    let mut provider = ClaudeCodeProvider::new(descriptor("sonnet"), "claude");
    let error = provider
        .stream(
            &request(vec![message(MessageRole::User, "hi")]),
            &CancellationToken::default(),
            &mut |_| Ok(StreamFlow::Continue),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);

    let mut provider = ClaudeCodeProvider::new(descriptor("sonnet"), "claude")
        .with_workspace(std::env::temp_dir());
    let error = provider
        .stream(
            &request(vec![message(MessageRole::System, "only system")]),
            &CancellationToken::default(),
            &mut |_| Ok(StreamFlow::Continue),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
}

#[test]
fn the_registry_offers_the_provider_only_when_the_operator_names_a_binary() {
    let credentials = Arc::new(InMemoryCredentialStore::default());
    let without = ProviderRegistry::configured_from(credentials.clone(), |_| None).unwrap();
    assert!(
        without
            .list_providers()
            .unwrap()
            .iter()
            .all(|provider| provider.id.as_str() != CLAUDE_CODE_PROVIDER_ID)
    );

    let with = ProviderRegistry::configured_from(credentials, |name| {
        (name == CLAUDE_CODE_BINARY_ENV).then(|| " /opt/claude ".to_owned())
    })
    .unwrap();
    let provider = with
        .list_providers()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id.as_str() == CLAUDE_CODE_PROVIDER_ID)
        .expect("registered");
    assert_eq!(provider.kind, ProviderKind::ClaudeCode);
    assert!(provider.is_usable());
    let models = with.list_models().unwrap();
    assert!(
        models.iter().any(
            |model| model.id.as_str() == "claude-code:opus" && !model.capabilities.tool_calling
        )
    );
    // The registry builds the provider that runs the configured binary.
    let built = with
        .create_provider(&ModelId::new("claude-code:opus"))
        .unwrap();
    assert_eq!(built.descriptor().id.as_str(), "claude-code:opus");
}

/// A stand-in `claude` that plays one scripted turn. One test, because a script
/// written moments ago must not race another thread's `fork`.
#[cfg(unix)]
#[test]
fn a_turn_streams_text_reasoning_usage_and_resumes_the_conversation() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("loom-claude-provider-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let args_file = dir.join("args.txt");
    let script = dir.join("claude");
    let body = format!(
        r#"#!/bin/sh
echo "$@" >> '{args}'
read first
echo "$first" >> '{args}'
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"s-9","tools":[]}}'
printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"thinking_delta","thinking":"hm"}}}}}}'
printf '%s\n' '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"ls"}}}}]}}}}'
printf '%s\n' '{{"type":"control_request","request_id":"r1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{}}}}}}'
read answer
echo "$answer" >> '{args}'
printf '%s\n' '{{"type":"user","message":{{"content":[{{"tool_use_id":"t1","type":"tool_result","content":"boom","is_error":true}}]}}}}'
printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"done"}}}},"parent_tool_use_id":null}}'
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s-9","usage":{{"input_tokens":3,"output_tokens":5,"cache_read_input_tokens":7,"cache_creation_input_tokens":11}}}}'
"#,
        args = args_file.display()
    );
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut provider = ClaudeCodeProvider::new(descriptor("opus"), &script);
    provider.bind_workspace(&dir);
    let mut sink = loom_model::CollectingSink::default();
    provider
        .stream(
            &request(vec![message(MessageRole::User, "list files")]),
            &CancellationToken::default(),
            &mut sink,
        )
        .unwrap();

    let text: String = sink
        .events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "done");
    let reasoning: String = sink
        .events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::ReasoningDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(reasoning.contains("hm"), "{reasoning}");
    assert!(reasoning.contains("→ Bash: ls"), "{reasoning}");
    assert!(reasoning.contains("needs approval; refused"), "{reasoning}");
    assert!(reasoning.contains("✗ boom"), "{reasoning}");
    assert!(sink.events.contains(&ModelStreamEvent::Usage {
        usage: TokenUsage {
            input_tokens: 14,
            output_tokens: 5,
            cached_input_tokens: 7,
        },
    }));
    assert_eq!(
        sink.events.last(),
        Some(&ModelStreamEvent::Completed {
            reason: FinishReason::Stop
        })
    );
    assert_eq!(provider.claude_session.as_deref(), Some("s-9"));
    assert_eq!(provider.delivered, 2);

    // The request the script saw: the alias, the first prompt, and a refusal.
    let seen = std::fs::read_to_string(&args_file).unwrap();
    assert!(seen.contains("--model opus"), "{seen}");
    assert!(seen.contains("--permission-mode acceptEdits"), "{seen}");
    assert!(seen.contains("list files"), "{seen}");
    assert!(seen.contains("\"behavior\":\"deny\""), "{seen}");

    // A second turn continues the CLI's conversation.
    let mut sink = loom_model::CollectingSink::default();
    provider
        .stream(
            &request(vec![
                message(MessageRole::User, "list files"),
                message(MessageRole::Assistant, "done"),
                message(MessageRole::User, "again"),
            ]),
            &CancellationToken::default(),
            &mut sink,
        )
        .unwrap();
    let seen = std::fs::read_to_string(&args_file).unwrap();
    assert!(seen.contains("--resume s-9"), "{seen}");
    assert!(seen.contains("again"), "{seen}");

    // A cancelled token interrupts the turn and ends it as cancelled.
    let token = CancellationToken::default();
    token.cancel();
    let error = provider
        .stream(
            &request(vec![
                message(MessageRole::User, "list files"),
                message(MessageRole::Assistant, "done"),
                message(MessageRole::User, "again"),
                message(MessageRole::Assistant, "done"),
                message(MessageRole::User, "third"),
            ]),
            &token,
            &mut |_| Ok(StreamFlow::Continue),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::RequestCancelled);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_binary_is_reported_by_the_health_check() {
    let mut provider = ClaudeCodeProvider::new(descriptor("sonnet"), "/nonexistent/claude");
    let error = provider.health_check().unwrap_err();
    assert_eq!(error.code, ErrorCode::ProviderUnavailable);
    provider.reset();
    assert_eq!(provider.delivered, 0);
}
