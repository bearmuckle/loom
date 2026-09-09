use loom_core::{ErrorCode, LoomError, Result, ToolCallId};
use loom_model::{
    FinishReason, MessageRole, ModelCapabilities, ModelDescriptor, ModelId, ModelMessage,
    ModelRequest, ModelStreamEvent, ProviderId, TokenUsage, ToolCall,
};

pub trait ModelProvider: Send {
    fn descriptor(&self) -> &ModelDescriptor;

    fn stream(&mut self, request: &ModelRequest) -> Result<Vec<ModelStreamEvent>>;
}

pub struct DeterministicProvider {
    descriptor: ModelDescriptor,
    steps: Vec<Vec<ModelStreamEvent>>,
    cursor: usize,
}

impl DeterministicProvider {
    pub fn demo() -> Self {
        Self {
            descriptor: deterministic_descriptor(),
            steps: vec![
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "I'll inspect the workspace before making changes.\n".to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: ToolCallId::new(),
                            name: "list_files".to_owned(),
                            arguments: serde_json::json!({"path": "."}),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::ToolCall,
                    },
                ],
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "I found the workspace. I'll apply a focused change next.\n"
                            .to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: ToolCallId::new(),
                            name: "apply_patch".to_owned(),
                            arguments: serde_json::json!({
                                "path": "loom-m1-demo.txt",
                                "old_text": "",
                                "new_text": "Loom M1 deterministic demo\n"
                            }),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::ToolCall,
                    },
                ],
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "The change is applied. I'll run validation now.\n".to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: ToolCallId::new(),
                            name: "run_command".to_owned(),
                            arguments: serde_json::json!({
                                "command": "rustc",
                                "args": ["--version"]
                            }),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::ToolCall,
                    },
                ],
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "The task is complete: the workspace change was applied and validation ran successfully."
                            .to_owned(),
                    },
                    ModelStreamEvent::Usage {
                        usage: TokenUsage {
                            input_tokens: 240,
                            output_tokens: 52,
                            cached_input_tokens: 0,
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::Stop,
                    },
                ],
            ],
            cursor: 0,
        }
    }
}

impl ModelProvider for DeterministicProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn stream(&mut self, _request: &ModelRequest) -> Result<Vec<ModelStreamEvent>> {
        let events = self.steps.get(self.cursor).cloned().unwrap_or_else(|| {
            vec![ModelStreamEvent::Completed {
                reason: FinishReason::Stop,
            }]
        });
        self.cursor = self.cursor.saturating_add(1);
        Ok(events)
    }
}

pub fn deterministic_descriptor() -> ModelDescriptor {
    ModelDescriptor {
        id: ModelId::new("deterministic/demo"),
        provider: ProviderId::new("deterministic"),
        display_name: "Deterministic M1 demo".to_owned(),
        context_window: Some(16_384),
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: false,
            json_mode: true,
        },
    }
}

pub struct OpenAiCompatibleProvider {
    endpoint: String,
    api_key: String,
    descriptor: ModelDescriptor,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
    ) -> Self {
        let model = model.into();
        Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            descriptor: ModelDescriptor {
                id: model,
                provider: ProviderId::new("openai-compatible"),
                display_name: "OpenAI-compatible model".to_owned(),
                context_window: None,
                capabilities: ModelCapabilities {
                    streaming: false,
                    tool_calling: true,
                    vision: false,
                    json_mode: true,
                },
            },
        }
    }
}

impl ModelProvider for OpenAiCompatibleProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn stream(&mut self, request: &ModelRequest) -> Result<Vec<ModelStreamEvent>> {
        let mut payload = serde_json::json!({
            "model": request.model.as_str(),
            "messages": request.messages.iter().map(message_json).collect::<Vec<_>>(),
            "stream": false
        });
        if !request.tools.is_empty() {
            payload["tools"] = serde_json::Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema
                            }
                        })
                    })
                    .collect(),
            );
        }
        if let Some(temperature) = request.options.temperature {
            payload["temperature"] = serde_json::json!(temperature);
        }
        if let Some(max_output_tokens) = request.options.max_output_tokens {
            payload["max_tokens"] = serde_json::json!(max_output_tokens);
        }
        if !request.options.stop_sequences.is_empty() {
            payload["stop"] = serde_json::json!(request.options.stop_sequences);
        }

        let response = ureq::post(&self.endpoint)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .send_json(payload)
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    format!("OpenAI-compatible request failed: {error}"),
                    true,
                )
            })?;
        let body: serde_json::Value = response.into_json().map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("OpenAI-compatible response was not valid JSON: {error}"),
                false,
            )
        })?;
        normalize_response(&body)
    }
}

fn message_json(message: &ModelMessage) -> serde_json::Value {
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
    value
}

fn normalize_response(body: &serde_json::Value) -> Result<Vec<ModelStreamEvent>> {
    let choice = body
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderUnavailable,
                "OpenAI-compatible response did not contain a choice",
                false,
            )
        })?;
    let message = choice.get("message").ok_or_else(|| {
        LoomError::new(
            ErrorCode::ProviderUnavailable,
            "OpenAI-compatible choice did not contain a message",
            false,
        )
    })?;
    let mut events = Vec::new();
    if let Some(content) = message.get("content").and_then(serde_json::Value::as_str) {
        if !content.is_empty() {
            events.push(ModelStreamEvent::TextDelta {
                text: content.to_owned(),
            });
        }
    }
    if let Some(tool_calls) = message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
    {
        for tool_call in tool_calls {
            let function = tool_call.get("function").ok_or_else(|| {
                LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    "OpenAI-compatible tool call did not contain a function",
                    false,
                )
            })?;
            let name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::ProviderUnavailable,
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
                                ErrorCode::ProviderUnavailable,
                                format!("tool arguments were not valid JSON: {error}"),
                                false,
                            )
                        })
                    } else {
                        Ok(arguments.clone())
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
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                cached_input_tokens: 0,
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
        Some(_) => FinishReason::Error,
    };
    events.push(ModelStreamEvent::Completed { reason });
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_provider_streams_tool_calls_and_completion() {
        let mut provider = DeterministicProvider::demo();
        let request = ModelRequest {
            model: ModelId::new("deterministic/demo"),
            messages: Vec::new(),
            tools: Vec::new(),
            options: Default::default(),
        };

        let first = provider.stream(&request).unwrap();
        assert!(matches!(
            first.get(1),
            Some(ModelStreamEvent::ToolCallDelta { call })
                if call.name == "list_files"
        ));
        let final_step = (0..3)
            .map(|_| provider.stream(&request).unwrap())
            .last()
            .unwrap();
        assert!(final_step.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::Completed {
                    reason: FinishReason::Stop
                }
            )
        }));
    }

    #[test]
    fn openai_compatible_descriptor_preserves_selected_model() {
        let provider = OpenAiCompatibleProvider::new(
            "https://example.invalid/v1/chat/completions",
            "secret",
            ModelId::new("example/model"),
        );

        assert_eq!(provider.descriptor().id.as_str(), "example/model");
        assert_eq!(provider.descriptor().provider.as_str(), "openai-compatible");
    }
}
