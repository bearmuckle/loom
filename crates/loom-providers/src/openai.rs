use futures_util::StreamExt;

use super::*;

pub struct OpenAiCompatibleProvider {
    pub endpoint: String,
    pub api_key: String,
    pub descriptor: ModelDescriptor,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
    ) -> Self {
        let model = model.into();
        Self::with_descriptor(
            endpoint,
            api_key,
            ModelDescriptor {
                id: model,
                provider: ProviderId::new("openai-compatible"),
                display_name: "OpenAI-compatible model".to_owned(),
                context_window: None,
                max_input_tokens: None,
                max_output_tokens: None,
                capabilities: ModelCapabilities {
                    streaming: false,
                    tool_calling: true,
                    vision: false,
                    json_mode: true,
                },
            },
        )
    }

    pub fn with_descriptor(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        descriptor: ModelDescriptor,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            descriptor,
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl ModelProvider for OpenAiCompatibleProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn stream(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        if request.model != self.descriptor.id {
            return Err(LoomError::invalid_request(format!(
                "request model '{}' does not match provider model '{}'",
                request.model.as_str(),
                self.descriptor.id.as_str()
            )));
        }
        cancel.check()?;
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

        let authorization = if self.api_key.is_empty() {
            String::new()
        } else {
            bearer_header(&self.api_key)
        };
        send_openai_request(
            &self.endpoint,
            &authorization,
            &[],
            payload,
            self.descriptor.provider.as_str(),
            None,
            cancel,
            sink,
        )
    }

    fn health_check(&mut self) -> Result<()> {
        let provider = self.descriptor.provider.as_str();
        let (status, _) = run_async(request_json(
            reqwest::Method::GET,
            &health_endpoint(&self.endpoint),
            &[],
            None,
        ))?;
        if status >= 400 {
            return Err(normalize_provider_error(provider, status));
        }
        Ok(())
    }
}

pub struct OllamaProvider {
    pub inner: OpenAiCompatibleProvider,
}

impl OllamaProvider {
    pub fn new(endpoint: impl Into<String>, model: impl Into<ModelId>) -> Self {
        let model = model.into();
        let descriptor = ModelDescriptor {
            id: model,
            provider: ProviderId::new("ollama"),
            display_name: "Ollama local model".to_owned(),
            context_window: Some(32_768),
            max_input_tokens: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities {
                streaming: false,
                tool_calling: true,
                vision: false,
                json_mode: true,
            },
        };
        Self::with_descriptor(endpoint, descriptor)
    }

    pub fn with_descriptor(endpoint: impl Into<String>, descriptor: ModelDescriptor) -> Self {
        let endpoint = ollama_chat_endpoint(endpoint.into());
        Self {
            inner: OpenAiCompatibleProvider::with_descriptor(endpoint, "", descriptor),
        }
    }

    pub fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }
}

impl ModelProvider for OllamaProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        self.inner.descriptor()
    }

    fn stream(
        &mut self,
        request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        self.inner.stream(request, cancel, sink)
    }

    fn count_tokens(&self, request: &ModelRequest) -> u64 {
        self.inner.count_tokens(request)
    }

    fn health_check(&mut self) -> Result<()> {
        self.inner.health_check()
    }
}

pub fn openai_request_payload(request: &ModelRequest) -> serde_json::Value {
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
    payload
}

pub fn uses_responses_endpoint(model: &str) -> bool {
    model.starts_with("gpt-5") || model.starts_with("gpt-6")
}

pub fn responses_request_payload(request: &ModelRequest) -> serde_json::Value {
    let input = request
        .messages
        .iter()
        .flat_map(|message| {
            if message.role == MessageRole::Tool {
                vec![serde_json::json!({
                    "type": "function_call_output",
                    "call_id": message
                        .tool_call_id
                        .map(|id| id.to_string())
                        .unwrap_or_default(),
                    "output": message.content
                })]
            } else if !message.tool_calls.is_empty() {
                message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        serde_json::json!({
                                "type": "function_call",
                                "call_id": call.id.to_string(),
                                "name": call.name,
                                "arguments": call.arguments.to_string()
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                let content_type = if message.role == MessageRole::Assistant {
                    "output_text"
                } else {
                    "input_text"
                };
                vec![serde_json::json!({
                    "role": match message.role {
                        MessageRole::System => "system",
                        MessageRole::User => "user",
                        MessageRole::Assistant => "assistant",
                        MessageRole::Tool => unreachable!(),
                    },
                    "content": [{
                        "type": content_type,
                        "text": message.content
                    }]
                })]
            }
        })
        .collect::<Vec<_>>();
    let mut payload = serde_json::json!({
        "model": request.model.as_str(),
        "input": input,
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
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema
                    })
                })
                .collect(),
        );
    }
    if let Some(max_output_tokens) = request.options.max_output_tokens {
        payload["max_output_tokens"] = serde_json::json!(max_output_tokens);
    }
    payload
}

/// Sends an OpenAI-shaped request and forwards every decoded event to `sink`.
///
/// The request asks for server-sent events. When the endpoint honours that, the
/// body is decoded chunk by chunk and text reaches the sink while the completion
/// is still being produced; the cancellation token is checked between chunks so
/// an interrupt does not have to wait for the completion. When the endpoint
/// answers with a complete JSON document instead (declared by its content type),
/// the document is normalized and emitted in one pass.
#[allow(clippy::too_many_arguments)]
pub fn send_openai_request(
    endpoint: &str,
    authorization: &str,
    headers: &[(&str, &str)],
    mut payload: serde_json::Value,
    provider: &str,
    call_ids: Option<&mut BTreeMap<String, ToolCallId>>,
    cancel: &CancellationToken,
    sink: &mut dyn ModelStreamSink,
) -> Result<()> {
    let responses_api = provider.contains("responses");
    payload["stream"] = serde_json::Value::Bool(true);
    if !responses_api {
        payload["stream_options"] = serde_json::json!({"include_usage": true});
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not start the provider runtime: {error}"),
                true,
            )
        })?;
    runtime.block_on(send_openai_request_async(
        endpoint,
        authorization,
        headers,
        payload,
        provider,
        responses_api,
        call_ids,
        cancel,
        sink,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn send_openai_request_async(
    endpoint: &str,
    authorization: &str,
    headers: &[(&str, &str)],
    payload: serde_json::Value,
    provider: &str,
    responses_api: bool,
    call_ids: Option<&mut BTreeMap<String, ToolCallId>>,
    cancel: &CancellationToken,
    sink: &mut dyn ModelStreamSink,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(PROVIDER_REQUEST_TIMEOUT)
        .build()
        .map_err(|error| normalize_transport_error(provider, &error.to_string()))?;
    let mut request = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .json(&payload);
    if !authorization.is_empty() {
        request = request.header("Authorization", authorization);
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    cancel.check()?;
    let response = request
        .send()
        .await
        .map_err(|error| normalize_transport_error(provider, &error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned);
        let detail = response.text().await.unwrap_or_default();
        let detail = detail
            .replace("Bearer ", "****** ")
            .replace("token ", "token [redacted] ")
            .chars()
            .take(512)
            .collect::<String>();
        // Keep the status-derived code and retryability — a 429 is
        // `ProviderRateLimited` and a 5xx is `ProviderUnavailable` — even when
        // the provider sends an explanation body, and append the redacted
        // detail and retry hint for diagnostics.
        let mut error = normalize_provider_error(provider, status.as_u16());
        if let Some(retry_after) = retry_after {
            error.message = format!("{}; retry after {retry_after}", error.message);
        }
        if !detail.trim().is_empty() {
            error.message = format!("{}: {detail}", error.message);
        }
        return Err(error);
    }
    let event_stream = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    let mut own_call_ids = BTreeMap::new();
    let call_ids = call_ids.unwrap_or(&mut own_call_ids);
    if !event_stream {
        let body: serde_json::Value = response.json().await.map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!("{provider} returned a response that was not valid JSON: {error}"),
                false,
            )
        })?;
        let events = if responses_api {
            normalize_responses_response(&body, call_ids)?
        } else {
            normalize_openai_response(&body)?
        };
        for event in events {
            cancel.check()?;
            if sink.emit(event)? == StreamFlow::Stop {
                break;
            }
        }
        return Ok(());
    }
    let mut decoder = if responses_api {
        StreamDecoder::responses(provider.to_owned())
    } else {
        StreamDecoder::chat_completions(provider.to_owned())
    };
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    'outer: while let Some(chunk) = stream.next().await {
        cancel.check()?;
        let chunk =
            chunk.map_err(|error| normalize_transport_error(provider, &error.to_string()))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = buffer.find('\n') {
            let line = buffer[..index].trim_end_matches('\r').to_owned();
            buffer.drain(..=index);
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            if payload == "[DONE]" {
                break 'outer;
            }
            let chunk: serde_json::Value = serde_json::from_str(payload).map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    format!("{provider} sent an event that was not valid JSON: {error}"),
                    false,
                )
            })?;
            if decoder.accept(&chunk, call_ids, sink)? == StreamFlow::Stop {
                return Ok(());
            }
        }
    }
    decoder.finish(sink)
}

/// Incremental decoder for the two OpenAI-shaped event streams Loom speaks.
pub struct StreamDecoder {
    pub provider: String,
    pub responses_api: bool,
    pub tool_calls: BTreeMap<u64, PartialToolCall>,
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<FinishReason>,
    pub completed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PartialToolCall {
    pub name: String,
    pub arguments: String,
}

impl StreamDecoder {
    pub fn chat_completions(provider: String) -> Self {
        Self {
            provider,
            responses_api: false,
            tool_calls: BTreeMap::new(),
            usage: None,
            finish_reason: None,
            completed: false,
        }
    }

    pub fn responses(provider: String) -> Self {
        Self {
            provider,
            responses_api: true,
            tool_calls: BTreeMap::new(),
            usage: None,
            finish_reason: None,
            completed: false,
        }
    }

    pub fn accept(
        &mut self,
        chunk: &serde_json::Value,
        call_ids: &mut BTreeMap<String, ToolCallId>,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<StreamFlow> {
        if self.responses_api {
            self.accept_responses_event(chunk, call_ids, sink)
        } else {
            self.accept_chat_chunk(chunk, sink)
        }
    }

    pub fn accept_chat_chunk(
        &mut self,
        chunk: &serde_json::Value,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<StreamFlow> {
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = Some(openai_usage(usage));
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(StreamFlow::Continue);
        };
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
        {
            self.finish_reason = Some(finish_reason_from_str(reason));
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(StreamFlow::Continue);
        };
        if let Some(fragments) = delta
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
        {
            for fragment in fragments {
                let index = fragment
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default();
                let entry = self.tool_calls.entry(index).or_default();
                if let Some(function) = fragment.get("function") {
                    if let Some(name) = function.get("name").and_then(serde_json::Value::as_str) {
                        entry.name.push_str(name);
                    }
                    if let Some(arguments) = function
                        .get("arguments")
                        .and_then(serde_json::Value::as_str)
                    {
                        entry.arguments.push_str(arguments);
                    }
                }
            }
        }
        // A reasoning field must be preserved even when empty: DeepSeek rejects a
        // request whose assistant turn omitted the `reasoning_content` it
        // returned, and it sometimes returns it as an empty string.
        if let Some(text) = delta
            .get("reasoning_content")
            .and_then(serde_json::Value::as_str)
        {
            return sink.emit(ModelStreamEvent::ReasoningDelta {
                text: text.to_owned(),
            });
        }
        if let Some(text) = delta.get("reasoning").and_then(serde_json::Value::as_str) {
            return sink.emit(ModelStreamEvent::ReasoningDelta {
                text: text.to_owned(),
            });
        }
        if let Some(text) = delta.get("content").and_then(serde_json::Value::as_str)
            && !text.is_empty()
        {
            return sink.emit(ModelStreamEvent::TextDelta {
                text: text.to_owned(),
            });
        }
        Ok(StreamFlow::Continue)
    }

    pub fn accept_responses_event(
        &mut self,
        chunk: &serde_json::Value,
        call_ids: &mut BTreeMap<String, ToolCallId>,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<StreamFlow> {
        match chunk.get("type").and_then(serde_json::Value::as_str) {
            Some("response.output_text.delta") => {
                let text = chunk
                    .get("delta")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    return sink.emit(ModelStreamEvent::TextDelta {
                        text: text.to_owned(),
                    });
                }
                Ok(StreamFlow::Continue)
            }
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
                let text = chunk
                    .get("delta")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    return sink.emit(ModelStreamEvent::ReasoningDelta {
                        text: text.to_owned(),
                    });
                }
                Ok(StreamFlow::Continue)
            }
            Some("response.output_item.done") => {
                let Some(item) = chunk.get("item") else {
                    return Ok(StreamFlow::Continue);
                };
                if item.get("type").and_then(serde_json::Value::as_str) != Some("function_call") {
                    return Ok(StreamFlow::Continue);
                }
                let call = responses_function_call(item, call_ids)?;
                sink.emit(ModelStreamEvent::ToolCallDelta { call })
            }
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                let response = chunk.get("response");
                if let Some(usage) = response
                    .and_then(|response| response.get("usage"))
                    .filter(|usage| !usage.is_null())
                {
                    self.usage = Some(responses_usage(usage));
                }
                let status = response
                    .and_then(|response| response.get("status"))
                    .and_then(serde_json::Value::as_str);
                self.finish_reason = Some(match status {
                    Some("completed") => FinishReason::Stop,
                    Some("incomplete") => {
                        let reason = response
                            .and_then(|response| response.get("incomplete_details"))
                            .and_then(|details| details.get("reason"))
                            .and_then(serde_json::Value::as_str);
                        log::error!(
                            "{} response incomplete: status={status:?}, reason={reason:?}",
                            self.provider
                        );
                        FinishReason::ErrorWithMessage {
                            message: reason.map_or_else(
                                || format!("{} response was incomplete", self.provider),
                                |reason| {
                                    format!("{} response was incomplete: {reason}", self.provider)
                                },
                            ),
                        }
                    }
                    _ => {
                        let error = response.and_then(|response| response.get("error"));
                        log::error!(
                            "{} response failed: status={status:?}, error={}",
                            self.provider,
                            error.map_or_else(
                                || "<missing>".to_owned(),
                                serde_json::Value::to_string
                            )
                        );
                        let message = error
                            .and_then(|error| error.get("message"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                format!("{} request failed without further details", self.provider)
                            });
                        FinishReason::ErrorWithMessage {
                            message: format!("{} request failed: {message}", self.provider),
                        }
                    }
                });
                self.completed = true;
                Ok(StreamFlow::Continue)
            }
            Some("error") => Err(LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!(
                    "{} reported a stream error: {}",
                    self.provider,
                    chunk
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("no detail")
                ),
                false,
            )),
            _ => Ok(StreamFlow::Continue),
        }
    }

    /// Emits the events that are only known once the stream ends.
    pub fn finish(self, sink: &mut dyn ModelStreamSink) -> Result<()> {
        let mut saw_tool_call = false;
        for (_, partial) in self.tool_calls {
            if partial.name.trim().is_empty() {
                return Err(LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    format!("{} streamed a tool call without a name", self.provider),
                    false,
                ));
            }
            let arguments = parse_tool_arguments(&partial.arguments)?;
            saw_tool_call = true;
            if sink.emit(ModelStreamEvent::ToolCallDelta {
                call: ToolCall {
                    id: ToolCallId::new(),
                    name: partial.name,
                    arguments,
                },
            })? == StreamFlow::Stop
            {
                return Ok(());
            }
        }
        if let Some(usage) = self.usage
            && sink.emit(ModelStreamEvent::Usage { usage })? == StreamFlow::Stop
        {
            return Ok(());
        }
        let reason = self.finish_reason.unwrap_or(if saw_tool_call {
            FinishReason::ToolCall
        } else {
            FinishReason::Stop
        });
        sink.emit(ModelStreamEvent::Completed { reason })?;
        Ok(())
    }
}

pub fn parse_tool_arguments(arguments: &str) -> Result<serde_json::Value> {
    if arguments.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(arguments).map_err(|error| {
        LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!("tool arguments were not valid JSON: {error}"),
            false,
        )
    })
}

pub fn finish_reason_from_str(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCall,
        "length" => FinishReason::Length,
        "cancelled" => FinishReason::Cancelled,
        _ => FinishReason::Error,
    }
}

pub fn openai_usage(usage: &serde_json::Value) -> TokenUsage {
    TokenUsage {
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
    }
}

pub fn responses_usage(usage: &serde_json::Value) -> TokenUsage {
    TokenUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    }
}

pub fn responses_function_call(
    item: &serde_json::Value,
    call_ids: &mut BTreeMap<String, ToolCallId>,
) -> Result<ToolCall> {
    let name = item
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                "Copilot Responses function call did not contain a name",
                false,
            )
        })?;
    let arguments = item
        .get("arguments")
        .and_then(serde_json::Value::as_str)
        .map(|arguments| {
            serde_json::from_str(arguments).map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    format!("Copilot Responses tool arguments were invalid: {error}"),
                    false,
                )
            })
        })
        .transpose()?
        .unwrap_or_else(|| serde_json::json!({}));
    let remote_call_id = item
        .get("call_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let id = *call_ids.entry(remote_call_id).or_default();
    Ok(ToolCall {
        id,
        name: name.to_owned(),
        arguments,
    })
}

pub fn normalize_responses_response(
    body: &serde_json::Value,
    call_ids: &mut BTreeMap<String, ToolCallId>,
) -> Result<Vec<ModelStreamEvent>> {
    let mut events = Vec::new();
    if let Some(output) = body.get("output").and_then(serde_json::Value::as_array) {
        for item in output {
            match item.get("type").and_then(serde_json::Value::as_str) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(serde_json::Value::as_array)
                    {
                        for part in content {
                            if let Some(text) = part.get("text").and_then(serde_json::Value::as_str)
                            {
                                events.push(ModelStreamEvent::TextDelta {
                                    text: text.to_owned(),
                                });
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let name = item
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::ProviderInvalidResponse,
                                "Copilot Responses function call did not contain a name",
                                false,
                            )
                        })?;
                    let arguments = item
                        .get("arguments")
                        .and_then(serde_json::Value::as_str)
                        .map(|arguments| {
                            serde_json::from_str(arguments).map_err(|error| {
                                LoomError::new(
                                    ErrorCode::ProviderInvalidResponse,
                                    format!(
                                        "Copilot Responses tool arguments were invalid: {error}"
                                    ),
                                    false,
                                )
                            })
                        })
                        .transpose()?
                        .unwrap_or_else(|| serde_json::json!({}));
                    let remote_call_id = item
                        .get("call_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let call_id = call_ids.entry(remote_call_id).or_default().to_owned();
                    events.push(ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: call_id,
                            name: name.to_owned(),
                            arguments,
                        },
                    });
                }
                _ => {}
            }
        }
    }
    if let Some(usage) = body.get("usage") {
        events.push(ModelStreamEvent::Usage {
            usage: TokenUsage {
                input_tokens: usage
                    .get("input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                output_tokens: usage
                    .get("output_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                cached_input_tokens: usage
                    .get("input_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            },
        });
    }
    events.push(ModelStreamEvent::Completed {
        reason: if body.get("status").and_then(serde_json::Value::as_str) == Some("completed") {
            FinishReason::Stop
        } else {
            FinishReason::Error
        },
    });
    Ok(events)
}
