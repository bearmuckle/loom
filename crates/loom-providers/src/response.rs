use super::*;

pub fn message_json(message: &ModelMessage) -> serde_json::Value {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };
    let mut value = serde_json::json!({
        "role": role,
        "content": message.content
    });
    if let Some(name) = &message.name {
        value["name"] = serde_json::json!(name);
    }
    if let Some(tool_call_id) = message.tool_call_id {
        value["tool_call_id"] = serde_json::json!(tool_call_id.to_string());
    }
    if message.role == MessageRole::Assistant
        && let Some(reasoning) = &message.reasoning_content
    {
        value["reasoning_content"] = serde_json::json!(reasoning);
    }
    if !message.tool_calls.is_empty() {
        value["tool_calls"] = serde_json::Value::Array(
            message
                .tool_calls
                .iter()
                .map(|call| {
                    serde_json::json!({
                        "id": call.id.to_string(),
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments.to_string()
                        }
                    })
                })
                .collect(),
        );
    }
    value
}

pub fn normalize_openai_response(body: &serde_json::Value) -> Result<Vec<ModelStreamEvent>> {
    let choice = body
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                "OpenAI-compatible response did not contain a choice",
                false,
            )
        })?;
    let message = choice.get("message").ok_or_else(|| {
        LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            "OpenAI-compatible choice did not contain a message",
            false,
        )
    })?;
    let mut events = Vec::new();
    // Keep the reasoning field even when empty: a thinking-mode provider rejects
    // a follow-up request that drops the `reasoning_content` it returned.
    if let Some(reasoning) = message
        .get("reasoning_content")
        .and_then(serde_json::Value::as_str)
    {
        events.push(ModelStreamEvent::ReasoningDelta {
            text: reasoning.to_owned(),
        });
    }
    if let Some(content) = message.get("content").and_then(serde_json::Value::as_str)
        && !content.is_empty()
    {
        events.push(ModelStreamEvent::TextDelta {
            text: content.to_owned(),
        });
    }
    if let Some(tool_calls) = message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
    {
        for tool_call in tool_calls {
            let function = tool_call.get("function").ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    "OpenAI-compatible tool call did not contain a function",
                    false,
                )
            })?;
            let name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::ProviderInvalidResponse,
                        "OpenAI-compatible tool call did not contain a name",
                        false,
                    )
                })?;
            let arguments = function
                .get("arguments")
                .map(|arguments| {
                    if let Some(arguments) = arguments.as_str() {
                        serde_json::from_str(arguments).map_err(|error| {
                            LoomError::new(
                                ErrorCode::ProviderInvalidResponse,
                                format!("tool arguments were not valid JSON: {error}"),
                                false,
                            )
                        })
                    } else if arguments.is_object() {
                        Ok(arguments.clone())
                    } else {
                        Err(LoomError::new(
                            ErrorCode::ProviderInvalidResponse,
                            "tool arguments must be a JSON object",
                            false,
                        ))
                    }
                })
                .transpose()?
                .unwrap_or_else(|| serde_json::json!({}));
            events.push(ModelStreamEvent::ToolCallDelta {
                call: ToolCall {
                    id: ToolCallId::new(),
                    name: name.to_owned(),
                    arguments,
                },
            });
        }
    }
    if let Some(usage) = body.get("usage") {
        events.push(ModelStreamEvent::Usage {
            usage: TokenUsage {
                input_tokens: usage
                    .get("prompt_tokens")
                    .or_else(|| usage.get("input_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                output_tokens: usage
                    .get("completion_tokens")
                    .or_else(|| usage.get("output_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                cached_input_tokens: usage
                    .get("prompt_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            },
        });
    }
    let reason = match choice
        .get("finish_reason")
        .and_then(serde_json::Value::as_str)
    {
        Some("stop") | None => FinishReason::Stop,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCall,
        Some("length") => FinishReason::Length,
        Some("cancelled") => FinishReason::Cancelled,
        Some(_) => FinishReason::Error,
    };
    events.push(ModelStreamEvent::Completed { reason });
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_reasoning_is_sent_back_for_thinking_providers() {
        let mut message = ModelMessage::new(MessageRole::Assistant, "");
        message.tool_calls = vec![ToolCall {
            id: ToolCallId::new(),
            name: "search".to_owned(),
            arguments: serde_json::json!({"q": "loom"}),
        }];
        message.reasoning_content = Some("first I searched".to_owned());
        let value = message_json(&message);
        assert_eq!(value["reasoning_content"], "first I searched");
        assert_eq!(value["tool_calls"][0]["function"]["name"], "search");

        // Reasoning is only replayed on assistant turns that actually have it.
        let mut user = ModelMessage::new(MessageRole::User, "hi");
        user.reasoning_content = Some("ignored".to_owned());
        assert!(message_json(&user).get("reasoning_content").is_none());

        let assistant = ModelMessage::new(MessageRole::Assistant, "done");
        assert!(message_json(&assistant).get("reasoning_content").is_none());
    }

    #[test]
    fn empty_reasoning_is_preserved_and_replayed() {
        // A provider that returns an empty reasoning string still requires it to
        // be echoed back; dropping the empty field triggers a 400.
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "reasoning_content": ""
                }
            }]
        });
        let events = normalize_openai_response(&body).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ReasoningDelta { text } if text.is_empty()
        )));

        let mut assistant = ModelMessage::new(MessageRole::Assistant, "");
        assistant.reasoning_content = Some(String::new());
        assert_eq!(message_json(&assistant)["reasoning_content"], "");
    }
}
