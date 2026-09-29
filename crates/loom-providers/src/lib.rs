use std::{
    collections::BTreeMap,
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use loom_core::{ErrorCode, LoomError, Result, Timestamp, ToolCallId};
use loom_model::{
    CancellationToken, FinishReason, MessageRole, ModelCapabilities, ModelDescriptor, ModelId,
    ModelMessage, ModelRequest, ModelStreamEvent, ModelStreamSink, ProviderId, StreamFlow,
    TokenUsage, ToolCall,
};
pub use loom_model::{
    CredentialRef, CredentialReference, DEEPSEEK_API_ENDPOINT, DEEPSEEK_DEFAULT_MODEL,
    DEEPSEEK_PROVIDER_ID, GITHUB_COPILOT_API_ENDPOINT, GITHUB_COPILOT_CREDENTIAL_REF,
    GITHUB_COPILOT_DEFAULT_MODEL, GITHUB_COPILOT_PROVIDER_ID, ModelProvider, OPENAI_API_ENDPOINT,
    OPENAI_DEFAULT_MODEL, OPENAI_PROVIDER_ID, ProviderConfig, ProviderDescriptor, ProviderHealth,
    ProviderHealthState, ProviderKind, ProviderSummary, ProviderUsageKey, ProviderUsageRecord,
    ProviderUsageSummary, UnavailableProvider, UsageLedger, deterministic_descriptor,
    estimate_tokens, github_copilot_descriptor,
};
use serde::Deserialize;

mod credential;
mod deterministic;
mod errors;
mod github;
mod http;
mod openai;
mod registry;
mod response;

pub use credential::*;
pub use deterministic::*;
pub use errors::*;
pub use github::*;
pub use http::*;
pub use openai::*;
pub use registry::*;
pub use response::*;

const GITHUB_OAUTH_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const GITHUB_COPILOT_EDITOR_VERSION: &str = "vscode/1.96.2";
const GITHUB_COPILOT_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
const GITHUB_COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
const GITHUB_API_VERSION: &str = "2025-04-01";
const PROVIDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

fn internal_lock_error(resource: &str) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("{resource} lock was poisoned"),
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
        time::Duration,
    };

    use loom_model::CollectingSink;

    use super::*;

    #[test]
    fn github_copilot_gpt_6_models_use_the_responses_endpoint() {
        assert!(uses_responses_endpoint("gpt-5.6-luna"));
        assert!(uses_responses_endpoint("gpt-6-luna"));
        assert!(uses_responses_endpoint("gpt-6-astra"));
        assert!(!uses_responses_endpoint("gpt-4o"));
    }

    #[test]
    fn request_payloads_preserve_options_and_convert_tool_messages() {
        let call = ToolCall {
            id: ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path":"README.md"}),
        };
        let mut assistant = ModelMessage::new(MessageRole::Assistant, "");
        assistant.tool_calls.push(call.clone());
        let mut tool = ModelMessage::new(MessageRole::Tool, "file contents");
        tool.tool_call_id = Some(call.id);
        let mut request = ModelRequest {
            model: ModelId::new("gpt-6-luna"),
            messages: vec![
                ModelMessage::new(MessageRole::System, "system"),
                ModelMessage::new(MessageRole::User, "hello"),
                assistant,
                tool,
            ],
            tools: Vec::new(),
            options: Default::default(),
        };
        request.options.max_output_tokens = Some(128);

        let payload = responses_request_payload(&request);
        assert_eq!(payload["max_output_tokens"], 128);
        assert_eq!(payload["input"][0]["role"], "system");
        assert_eq!(payload["input"][1]["content"][0]["type"], "input_text");
        assert_eq!(payload["input"][2]["type"], "function_call");
        assert_eq!(payload["input"][3]["type"], "function_call_output");
        assert_eq!(payload["input"][3]["output"], "file contents");

        request.model = ModelId::new("fixture/model");
        request.options.temperature = Some(0.25);
        request.options.max_output_tokens = Some(64);
        request.options.stop_sequences = vec!["STOP".to_owned()];
        let payload = openai_request_payload(&request);
        assert_eq!(payload["temperature"], 0.25);
        assert_eq!(payload["max_tokens"], 64);
        assert_eq!(payload["stop"][0], "STOP");
        assert_eq!(
            payload["messages"][2]["tool_calls"][0]["function"]["name"],
            "read_file"
        );

        request.tools.push(loom_model::ToolDefinition {
            name: "search".to_owned(),
            description: "Search files".to_owned(),
            input_schema: serde_json::json!({"type":"object"}),
        });
        assert_eq!(
            responses_request_payload(&request)["tools"][0]["name"],
            "search"
        );
        assert_eq!(
            openai_request_payload(&request)["tools"][0]["function"]["name"],
            "search"
        );
    }

    #[test]
    fn provider_usage_and_status_helpers_handle_empty_and_extreme_values() {
        assert_eq!(parse_tool_arguments("  ").unwrap(), serde_json::json!({}));
        assert_eq!(
            finish_reason_from_str("function_call"),
            FinishReason::ToolCall
        );
        assert_eq!(finish_reason_from_str("length"), FinishReason::Length);
        assert_eq!(finish_reason_from_str("cancelled"), FinishReason::Cancelled);
        assert_eq!(finish_reason_from_str("unexpected"), FinishReason::Error);

        let usage = openai_usage(&serde_json::json!({
            "input_tokens": 12,
            "output_tokens": 8,
            "prompt_tokens_details": {"cached_tokens": 3}
        }));
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.cached_input_tokens, 3);
        assert_eq!(
            responses_usage(&serde_json::json!({"input_tokens": 4, "output_tokens": 2}))
                .output_tokens,
            2
        );
        assert_eq!(cost_for_usage(&usage, 1_000, 2_000), 28);
        assert_eq!(cost_for_usage(&usage, u64::MAX, u64::MAX), u64::MAX / 1_000);
        assert_eq!(
            health_endpoint("https://example.test/v1/chat/completions"),
            "https://example.test/v1/models"
        );
        let provider = ProviderId::new("fixture");
        let model = ModelId::new("fixture/model");
        let mut ledger = UsageLedger::default();
        ledger.record(provider.clone(), model.clone(), usage.clone(), 28);
        ledger.record(provider.clone(), model.clone(), usage, 28);
        assert_eq!(ledger.aggregates.len(), 1);
        assert_eq!(ledger.summary(Some(&provider), Some(&model)).requests, 2);
        assert_eq!(ledger.summary(None, None).input_tokens, 24);
        assert_eq!(ledger.session_usage().cost_micros, 56);
        assert_eq!(
            serde_json::from_value::<UsageLedger>(serde_json::to_value(&ledger).unwrap()).unwrap(),
            ledger
        );
        assert_eq!(
            health_endpoint("https://example.test/status"),
            "https://example.test/status"
        );
        assert_eq!(
            trim_endpoint("https://example.test/api///"),
            "https://example.test/api"
        );
        assert_eq!(
            ollama_chat_endpoint("http://localhost:11434///".to_owned()),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            ollama_chat_endpoint("http://localhost/v1/chat/completions/".to_owned()),
            "http://localhost/v1/chat/completions"
        );
        assert_eq!(bearer_header("secret"), "Bearer secret");
        let oauth_error = oauth_response_error(OAuthTokenResponse {
            access_token: None,
            error: Some("access_denied".to_owned()),
            error_description: Some("user cancelled".to_owned()),
        });
        assert_eq!(oauth_error.code, ErrorCode::ProviderAuthentication);
        assert!(oauth_error.message.contains("user cancelled"));

        for (status, code, retryable) in [
            (401, ErrorCode::ProviderAuthentication, false),
            (425, ErrorCode::ProviderRateLimited, true),
            (503, ErrorCode::ProviderUnavailable, true),
            (400, ErrorCode::ProviderInvalidResponse, false),
        ] {
            let error = normalize_provider_error("fixture", status);
            assert_eq!(error.code, code);
            assert_eq!(error.retryable, retryable);
        }
    }

    #[test]
    fn responses_stream_decoder_handles_events_and_rejects_malformed_streams() {
        let mut decoder = StreamDecoder::responses("copilot".to_owned());
        let mut call_ids = BTreeMap::new();
        let mut sink = CollectingSink::default();

        assert_eq!(
            decoder
                .accept(
                    &serde_json::json!({"type":"response.output_text.delta", "delta":""}),
                    &mut call_ids,
                    &mut sink,
                )
                .unwrap(),
            StreamFlow::Continue
        );
        decoder
            .accept(
                &serde_json::json!({"type":"response.output_text.delta", "delta":"hello"}),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder
            .accept(
                &serde_json::json!({"type":"response.output_item.done"}),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder
            .accept(
                &serde_json::json!({"type":"response.output_item.done", "item":{"type":"message"}}),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder
            .accept(
                &serde_json::json!({
                    "type":"response.output_item.done",
                    "item":{"type":"function_call", "name":"read_file", "call_id":"call-1", "arguments":"{\"path\":\"README.md\"}"}
                }),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder
            .accept(
                &serde_json::json!({
                    "type":"response.completed",
                    "response":{"status":"completed", "usage":{"input_tokens":3, "output_tokens":2}}
                }),
                &mut call_ids,
                &mut sink,
            )
            .unwrap();
        decoder.finish(&mut sink).unwrap();

        assert!(
            matches!(sink.events.first(), Some(ModelStreamEvent::TextDelta { text }) if text == "hello")
        );
        assert!(sink.events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ToolCallDelta { call }
                if call.name == "read_file" && call.arguments["path"] == "README.md"
        )));
        assert!(sink.events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Usage { usage } if usage.input_tokens == 3 && usage.output_tokens == 2
        )));
        assert!(matches!(
            sink.events.last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::Stop
            })
        ));

        let mut malformed_call = StreamDecoder::responses("copilot".to_owned());
        assert!(
            malformed_call
                .accept(
                    &serde_json::json!({
                        "type":"response.output_item.done",
                        "item":{"type":"function_call", "arguments":"{}"}
                    }),
                    &mut BTreeMap::new(),
                    &mut CollectingSink::default(),
                )
                .is_err()
        );
        let mut stream_error = StreamDecoder::responses("copilot".to_owned());
        let error = stream_error
            .accept(
                &serde_json::json!({"type":"error", "message":"upstream failed"}),
                &mut BTreeMap::new(),
                &mut CollectingSink::default(),
            )
            .unwrap_err();
        assert!(error.message.contains("upstream failed"));
    }

    #[test]
    fn responses_stream_decoder_preserves_failure_and_incomplete_details() {
        for (event, expected) in [
            (
                serde_json::json!({
                    "type":"response.failed",
                    "response":{"status":"failed", "error":{"message":"model overloaded"}}
                }),
                "copilot request failed: model overloaded",
            ),
            (
                serde_json::json!({
                    "type":"response.incomplete",
                    "response":{"status":"incomplete", "incomplete_details":{"reason":"max_output_tokens"}}
                }),
                "copilot response was incomplete: max_output_tokens",
            ),
        ] {
            let mut decoder = StreamDecoder::responses("copilot".to_owned());
            let mut sink = CollectingSink::default();
            decoder
                .accept(&event, &mut BTreeMap::new(), &mut sink)
                .unwrap();
            decoder.finish(&mut sink).unwrap();
            assert!(matches!(
                sink.events.last(),
                Some(ModelStreamEvent::Completed {
                    reason: FinishReason::ErrorWithMessage { message }
                }) if message == expected
            ));
        }

        let mut decoder = StreamDecoder::responses("copilot".to_owned());
        let mut sink = CollectingSink::default();
        decoder
            .accept(
                &serde_json::json!({
                    "type":"response.output_text.delta",
                    "delta":"partial answer"
                }),
                &mut BTreeMap::new(),
                &mut sink,
            )
            .unwrap();
        decoder
            .accept(
                &serde_json::json!({
                    "type":"response.incomplete",
                    "response":{
                        "status":"incomplete",
                        "incomplete_details":{"reason":"max_output_tokens"},
                        "usage":{"input_tokens":17,"output_tokens":3}
                    }
                }),
                &mut BTreeMap::new(),
                &mut sink,
            )
            .unwrap();
        decoder.finish(&mut sink).unwrap();
        assert!(matches!(
            sink.events.as_slice(),
            [
                ModelStreamEvent::TextDelta { text },
                ModelStreamEvent::Usage { usage },
                ModelStreamEvent::Completed {
                    reason: FinishReason::ErrorWithMessage { message }
                }
            ] if text == "partial answer"
                && usage.input_tokens == 17
                && usage.output_tokens == 3
                && message == "copilot response was incomplete: max_output_tokens"
        ));
    }

    #[test]
    fn chat_stream_decoder_emits_tool_usage_and_completion_or_rejects_bad_arguments() {
        let mut decoder = StreamDecoder::chat_completions("fixture".to_owned());
        let mut sink = CollectingSink::default();
        decoder
            .accept(
                &serde_json::json!({
                    "choices":[{"delta":{"tool_calls":[
                        {"index":0,"function":{"name":"read_", "arguments":"{\"path\":"}},
                        {"index":0,"function":{"name":"file", "arguments":"\"README.md\"}"}}
                    ]}}],
                    "usage":{"prompt_tokens":5,"completion_tokens":4}
                }),
                &mut BTreeMap::new(),
                &mut sink,
            )
            .unwrap();
        decoder.finish(&mut sink).unwrap();
        assert!(sink.events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ToolCallDelta { call }
                if call.name == "read_file" && call.arguments["path"] == "README.md"
        )));
        assert!(matches!(
            sink.events.last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::ToolCall
            })
        ));

        let mut malformed = StreamDecoder::chat_completions("fixture".to_owned());
        let mut malformed_sink = CollectingSink::default();
        malformed
            .accept(
                &serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"read_file","arguments":"{"}}]}}]}),
                &mut BTreeMap::new(),
                &mut malformed_sink,
            )
            .unwrap();
        assert!(malformed.finish(&mut malformed_sink).is_err());

        let missing_name = StreamDecoder {
            tool_calls: BTreeMap::from([(0, PartialToolCall::default())]),
            ..StreamDecoder::chat_completions("fixture".to_owned())
        };
        assert!(missing_name.finish(&mut CollectingSink::default()).is_err());
    }

    #[test]
    fn stream_decoders_surface_provider_reasoning_deltas() {
        for field in ["reasoning_content", "reasoning"] {
            let mut decoder = StreamDecoder::chat_completions("fixture".to_owned());
            let mut sink = CollectingSink::default();
            decoder
                .accept(
                    &serde_json::json!({"choices":[{"delta":{field:"thinking..."}}]}),
                    &mut BTreeMap::new(),
                    &mut sink,
                )
                .unwrap();
            assert!(sink.events.iter().any(|event| matches!(
                event,
                ModelStreamEvent::ReasoningDelta { text } if text == "thinking..."
            )));
        }

        let mut decoder = StreamDecoder::responses("fixture".to_owned());
        let mut sink = CollectingSink::default();
        decoder
            .accept(
                &serde_json::json!({
                    "type": "response.reasoning_summary_text.delta",
                    "delta": "weighing options",
                }),
                &mut BTreeMap::new(),
                &mut sink,
            )
            .unwrap();
        assert!(sink.events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ReasoningDelta { text } if text == "weighing options"
        )));
    }

    #[test]
    fn discovery_limits_are_per_model_and_validate_advertised_values() {
        assert_eq!(
            discovered_context_window(&serde_json::json!({"context_length": 8192})),
            Some(8192)
        );
        assert_eq!(
            discovered_context_window(
                &serde_json::json!({"capabilities": {"limits": {"max_context_window_tokens": 64000, "max_prompt_tokens": 32000}}})
            ),
            Some(64000)
        );
        let limits = serde_json::json!({
            "capabilities": {"limits": {
                "max_context_window_tokens": 64000,
                "max_prompt_tokens": 32000,
                "max_output_tokens": 8192
            }}
        });
        assert_eq!(discovered_input_tokens(&limits), Some(32_000));
        assert_eq!(discovered_output_tokens(&limits), Some(8_192));
        for model in [
            serde_json::json!({}),
            serde_json::json!({"context_length": 0}),
            serde_json::json!({"context_length": -1}),
            serde_json::json!({"context_length": u64::MAX}),
            serde_json::json!({"context_length": "8192"}),
        ] {
            assert_eq!(discovered_context_window(&model), None);
        }
    }

    #[test]
    fn github_copilot_discovery_lists_only_models_with_tool_call_support() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut token_stream, _) = listener.accept().unwrap();
            read_request_headers(&mut token_stream).unwrap();
            write_response(
                &mut token_stream,
                "application/json",
                &format!(r#"{{"token":"copilot-token","endpoints":{{"api":"http://{address}"}}}}"#),
            )
            .unwrap();

            let (mut models_stream, _) = listener.accept().unwrap();
            read_request_headers(&mut models_stream).unwrap();
            write_response(
                &mut models_stream,
                "application/json",
                r#"{"data":[
                    {"id":"agentic-model","capabilities":{"supports":{"tool_calls":true},"limits":{"max_context_window_tokens":64000,"max_prompt_tokens":32000,"max_output_tokens":8192}}},
                    {"id":"chat-model","capabilities":{"supports":{"tool_calls":false}}},
                    {"id":"unknown-model","capabilities":{"supports":{}}}
                ]}"#,
            )
            .unwrap();
        });
        let descriptor = github_copilot_descriptor();
        let provider = GitHubCopilotProvider::with_endpoints(
            format!("http://{address}"),
            format!("http://{address}/token"),
            "github-token",
            descriptor,
        );

        let models = provider.discover_models().unwrap();
        assert_eq!(models[0].context_window, Some(64_000));
        assert_eq!(models[0].max_input_tokens, Some(32_000));
        assert_eq!(models[0].max_output_tokens, Some(8_192));

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["agentic-model"]
        );
        server.join().unwrap();
    }

    fn collect(provider: &mut impl ModelProvider, request: &ModelRequest) -> Vec<ModelStreamEvent> {
        let mut sink = CollectingSink::default();
        provider
            .stream(request, &CancellationToken::new(), &mut sink)
            .unwrap();
        sink.events
    }

    fn serve_once(
        body: &'static str,
        content_type: &'static str,
    ) -> (String, thread::JoinHandle<std::result::Result<(), String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            read_request_headers(&mut stream)?;
            write_response(&mut stream, content_type, body)?;
            Ok(())
        });
        (format!("http://{address}/v1/chat/completions"), server)
    }

    fn read_request_headers(stream: &mut TcpStream) -> std::result::Result<String, String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.len() > 64 * 1024 {
                return Err("request headers exceeded fixture limit".to_owned());
            }
        }
        Ok(String::from_utf8_lossy(&request).into_owned())
    }

    fn write_response(
        stream: &mut TcpStream,
        content_type: &str,
        body: &str,
    ) -> std::result::Result<(), String> {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .map_err(|error| error.to_string())?;
        stream.flush().map_err(|error| error.to_string())
    }

    #[test]
    fn openai_compatible_provider_emits_text_before_the_stream_ends() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"tial\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":",
            "{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":",
            "{\"arguments\":\"\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":4}}\n\n",
            "data: [DONE]\n\n"
        );
        let (endpoint, server) = serve_once(body, "text/event-stream");
        let mut provider =
            OpenAiCompatibleProvider::new(endpoint, "", ModelId::new("fixture/model"));
        let request = ModelRequest {
            model: ModelId::new("fixture/model"),
            messages: vec![ModelMessage::new(MessageRole::User, "hello")],
            tools: Vec::new(),
            options: Default::default(),
        };
        let mut seen = Vec::new();
        provider
            .stream(&request, &CancellationToken::new(), &mut |event| {
                seen.push(event);
                Ok(StreamFlow::Continue)
            })
            .unwrap_or_else(|error| panic!("provider stream failed: {error:?}"));
        server
            .join()
            .expect("fixture server thread panicked")
            .expect("fixture server failed");
        assert!(matches!(
            seen.first(),
            Some(ModelStreamEvent::TextDelta { text }) if text == "par"
        ));
        assert!(matches!(
            seen.get(1),
            Some(ModelStreamEvent::TextDelta { text }) if text == "tial"
        ));
        assert!(seen.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ToolCallDelta { call }
                if call.name == "read_file" && call.arguments["path"] == "README.md"
        )));
        assert!(seen.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Usage { usage } if usage.input_tokens == 7
        )));
        assert!(matches!(
            seen.last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::ToolCall
            })
        ));
    }

    #[test]
    fn a_cancelled_token_stops_a_provider_before_it_sends() {
        let mut provider = DeterministicProvider::demo();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = provider
            .stream(
                &ModelRequest {
                    model: ModelId::new("deterministic/demo"),
                    messages: Vec::new(),
                    tools: Vec::new(),
                    options: Default::default(),
                },
                &cancel,
                &mut CollectingSink::default(),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::RequestCancelled);
    }

    #[test]
    fn a_sink_can_stop_a_stream_early() {
        let mut provider = DeterministicProvider::demo();
        let mut seen = 0_usize;
        provider
            .stream(
                &ModelRequest {
                    model: ModelId::new("deterministic/demo"),
                    messages: Vec::new(),
                    tools: Vec::new(),
                    options: Default::default(),
                },
                &CancellationToken::new(),
                &mut |_event| {
                    seen += 1;
                    Ok(StreamFlow::Stop)
                },
            )
            .unwrap();
        assert_eq!(seen, 1);
    }

    #[test]
    fn deterministic_provider_streams_tool_calls_and_completion() {
        let mut provider = DeterministicProvider::demo();
        let request = ModelRequest {
            model: ModelId::new("deterministic/demo"),
            messages: Vec::new(),
            tools: Vec::new(),
            options: Default::default(),
        };

        let first = collect(&mut provider, &request);
        assert!(matches!(
            first.get(1),
            Some(ModelStreamEvent::ToolCallDelta { call })
                if call.name == "list_files"
        ));
        let final_step = (0..3)
            .map(|_| collect(&mut provider, &request))
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
    fn openai_compatible_response_normalization_rejects_malformed_tool_arguments() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {"name": "read_file", "arguments": "not-json"}
                    }]
                }
            }]
        });
        let error = normalize_openai_response(&body).unwrap_err();
        assert_eq!(error.code, ErrorCode::ProviderInvalidResponse);
    }

    #[test]
    fn openai_response_normalization_validates_envelopes_and_argument_shapes() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"choices": []}),
            serde_json::json!({"choices": [{}]}),
        ] {
            assert_eq!(
                normalize_openai_response(&body).unwrap_err().code,
                ErrorCode::ProviderInvalidResponse
            );
        }
        for tool_call in [
            serde_json::json!({}),
            serde_json::json!({"function": {}}),
            serde_json::json!({"function": {"name": "read_file", "arguments": 7}}),
        ] {
            let body = serde_json::json!({
                "choices": [{"message": {"tool_calls": [tool_call]}}]
            });
            assert_eq!(
                normalize_openai_response(&body).unwrap_err().code,
                ErrorCode::ProviderInvalidResponse
            );
        }

        let events = normalize_openai_response(&serde_json::json!({
            "choices": [{"message": {
                "content": "",
                "tool_calls": [{"function": {"name": "read_file"}}]
            }, "finish_reason": null}]
        }))
        .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.first(),
            Some(ModelStreamEvent::ToolCallDelta { call })
                if call.name == "read_file" && call.arguments == serde_json::json!({})
        ));
        assert!(matches!(
            events.last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::Stop
            })
        ));
    }

    #[test]
    fn responses_normalization_covers_text_tool_usage_and_invalid_calls() {
        let mut call_ids = BTreeMap::new();
        let events = normalize_responses_response(
            &serde_json::json!({
                "status": "completed",
                "output": [
                    {"type":"message","content":[{"text":"answer"},{"text": 4}]},
                    {"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{\"path\":\"README.md\"}"},
                    {"type":"unknown"}
                ],
                "usage":{"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":2}}
            }),
            &mut call_ids,
        )
        .unwrap();
        assert!(matches!(events[0], ModelStreamEvent::TextDelta { ref text } if text == "answer"));
        assert!(
            matches!(events[1], ModelStreamEvent::ToolCallDelta { ref call } if call.name == "read_file" && call.arguments["path"] == "README.md")
        );
        assert!(
            matches!(events[2], ModelStreamEvent::Usage { ref usage } if usage.cached_input_tokens == 2)
        );
        assert!(matches!(
            events[3],
            ModelStreamEvent::Completed {
                reason: FinishReason::Stop
            }
        ));

        for output in [
            serde_json::json!([{"type":"function_call"}]),
            serde_json::json!([{"type":"function_call","name":"read_file","arguments":"invalid"}]),
        ] {
            assert_eq!(
                normalize_responses_response(&serde_json::json!({"output": output}), &mut call_ids)
                    .unwrap_err()
                    .code,
                ErrorCode::ProviderInvalidResponse
            );
        }
        assert!(matches!(
            normalize_responses_response(&serde_json::json!({"status":"failed"}), &mut call_ids)
                .unwrap()
                .last(),
            Some(ModelStreamEvent::Completed {
                reason: FinishReason::Error
            })
        ));
    }

    #[test]
    fn registry_lists_deterministic_and_local_models_without_secrets() {
        let registry = ProviderRegistry::demo();
        let models = registry.list_models().unwrap();
        assert!(
            models
                .iter()
                .any(|model| model.provider.as_str() == "deterministic")
        );
        assert!(
            models
                .iter()
                .any(|model| model.provider.as_str() == "ollama")
        );
        let openai_registry =
            ProviderRegistry::configured(Arc::new(InMemoryCredentialStore::default())).unwrap();
        let openai = openai_registry
            .list_providers()
            .unwrap()
            .into_iter()
            .find(|provider| provider.id.as_str() == OPENAI_PROVIDER_ID)
            .expect("OpenAI should be a distinct configured provider");
        assert_eq!(openai.kind, ProviderKind::OpenAi);
        assert_eq!(openai.display_name, "OpenAI");
        assert!(openai.api_key_configurable);
        assert_eq!(
            openai.models[0].id.as_str(),
            std::env::var("LOOM_OPENAI_MODEL").unwrap_or_else(|_| OPENAI_DEFAULT_MODEL.to_owned())
        );
        let deepseek = openai_registry
            .list_providers()
            .unwrap()
            .into_iter()
            .find(|provider| provider.id.as_str() == DEEPSEEK_PROVIDER_ID)
            .expect("DeepSeek should be a distinct configured provider");
        assert_eq!(deepseek.kind, ProviderKind::DeepSeek);
        assert_eq!(deepseek.display_name, "DeepSeek");
        assert!(deepseek.api_key_configurable);
        assert_eq!(
            deepseek.models[0].id.as_str(),
            std::env::var("LOOM_DEEPSEEK_MODEL")
                .unwrap_or_else(|_| DEEPSEEK_DEFAULT_MODEL.to_owned())
        );
        let serialized = serde_json::to_string(&registry.list_providers().unwrap()).unwrap();
        assert!(!serialized.contains("Bearer"));
    }

    #[test]
    fn openai_api_key_is_stored_by_the_worker_and_redacted_from_provider_summaries() {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let registry = ProviderRegistry::with_credentials(credentials.clone());
        registry
            .register(ProviderConfig::openai(OPENAI_DEFAULT_MODEL))
            .unwrap();
        let scoped_credentials = Arc::new(InMemoryCredentialStore::default());
        registry
            .scope_api_key_credentials(scoped_credentials.clone())
            .unwrap();

        registry
            .configure_api_key_provider(
                &ProviderId::new(OPENAI_PROVIDER_ID),
                "openai-test-secret".to_owned(),
            )
            .unwrap();

        let provider = registry.list_providers().unwrap().remove(0);
        let reference = CredentialRef::new(provider.credential_id.as_deref().unwrap());
        assert_eq!(
            scoped_credentials.resolve(&reference).unwrap(),
            "openai-test-secret"
        );
        assert!(!format!("{provider:?}").contains("openai-test-secret"));
        assert!(
            !serde_json::to_string(&registry.export_configs().unwrap())
                .unwrap()
                .contains("openai-test-secret")
        );
    }

    #[test]
    fn openai_model_discovery_uses_official_models_shape_and_filters_unsupported_models() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> std::result::Result<(), String> {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            let request = read_request_headers(&mut stream)?;
            if !request.starts_with("GET /v1/models ")
                || !request.contains("Bearer openai-discovery-key")
            {
                return Err("OpenAI model request had an unexpected path or credential".to_owned());
            }
            write_response(
                &mut stream,
                "application/json",
                r#"{"data":[{"id":"gpt-4.1"},{"id":"text-embedding-3-small"},{"id":"o3-mini"},{"id":"gpt-6-astra"},{"id":"gpt-6-sol"},{"id":"gpt-6-luna"},{"id":"gpt-6-audio"}]}"#,
            )
        });
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let registry = ProviderRegistry::with_credentials(credentials);
        let mut config = ProviderConfig::openai(OPENAI_DEFAULT_MODEL);
        config.endpoint = Some(format!("http://{address}/v1/chat/completions"));
        registry.register(config).unwrap();
        registry
            .configure_api_key_provider(
                &ProviderId::new(OPENAI_PROVIDER_ID),
                "openai-discovery-key".to_owned(),
            )
            .unwrap();

        let models = registry
            .discover_models(&ProviderId::new(OPENAI_PROVIDER_ID))
            .unwrap();

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            [
                "gpt-4.1",
                "o3-mini",
                "gpt-6-astra",
                "gpt-6-sol",
                "gpt-6-luna"
            ]
        );
        server
            .join()
            .expect("fixture server panicked")
            .expect("fixture server failed");
    }

    #[test]
    fn configured_openai_provider_serves_a_model_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> std::result::Result<(), String> {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            let request = read_request_headers(&mut stream)?;
            if !request.starts_with("POST /v1/chat/completions ")
                || !request.contains("Bearer openai-request-key")
            {
                return Err(
                    "OpenAI completion request had an unexpected path or credential".to_owned(),
                );
            }
            write_response(
                &mut stream,
                "application/json",
                r#"{"choices":[{"message":{"content":"OpenAI fixture"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
            )
        });
        let registry =
            ProviderRegistry::with_credentials(Arc::new(InMemoryCredentialStore::default()));
        let mut config = ProviderConfig::openai("fixture-model");
        config.endpoint = Some(format!("http://{address}/v1/chat/completions"));
        config.models[0].id = ModelId::new("fixture-model");
        registry.register(config).unwrap();
        registry
            .configure_api_key_provider(
                &ProviderId::new(OPENAI_PROVIDER_ID),
                "openai-request-key".to_owned(),
            )
            .unwrap();
        let request = ModelRequest {
            model: ModelId::new("fixture-model"),
            messages: vec![ModelMessage::new(MessageRole::User, "hello")],
            tools: Vec::new(),
            options: Default::default(),
        };
        let mut provider = registry.create_provider(&request.model).unwrap();
        let events = provider
            .stream_collected(&request, &CancellationToken::new())
            .unwrap();
        assert!(events.iter().any(|event| matches!(event, ModelStreamEvent::TextDelta { text } if text == "OpenAI fixture")));
        server
            .join()
            .expect("fixture server panicked")
            .expect("fixture server failed");
    }

    #[test]
    fn deepseek_provider_defaults_use_the_official_endpoint_and_model() {
        let config = ProviderConfig::deepseek(DEEPSEEK_DEFAULT_MODEL);
        assert_eq!(config.kind, ProviderKind::DeepSeek);
        assert_eq!(config.endpoint.as_deref(), Some(DEEPSEEK_API_ENDPOINT));
        assert_eq!(config.display_name, "DeepSeek");
        assert!(config.credential.is_none());
        assert_eq!(config.models[0].id.as_str(), DEEPSEEK_DEFAULT_MODEL);
        assert!(config.models[0].capabilities.tool_calling);
    }

    #[test]
    fn configured_deepseek_provider_serves_a_model_request_with_its_api_key() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> std::result::Result<(), String> {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            let request = read_request_headers(&mut stream)?;
            if !request.starts_with("POST /chat/completions ")
                || !request.contains("Bearer deepseek-request-key")
            {
                return Err(
                    "DeepSeek completion request had an unexpected path or credential".to_owned(),
                );
            }
            write_response(
                &mut stream,
                "application/json",
                r#"{"choices":[{"message":{"content":"DeepSeek fixture"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
            )
        });
        let registry =
            ProviderRegistry::with_credentials(Arc::new(InMemoryCredentialStore::default()));
        let mut config = ProviderConfig::deepseek("fixture-model");
        config.endpoint = Some(format!("http://{address}/chat/completions"));
        config.models[0].id = ModelId::new("fixture-model");
        registry.register(config).unwrap();
        registry
            .configure_api_key_provider(
                &ProviderId::new(DEEPSEEK_PROVIDER_ID),
                "deepseek-request-key".to_owned(),
            )
            .unwrap();

        let provider = registry
            .list_providers()
            .unwrap()
            .into_iter()
            .find(|provider| provider.id.as_str() == DEEPSEEK_PROVIDER_ID)
            .expect("DeepSeek provider should be listed");
        assert_eq!(provider.kind, ProviderKind::DeepSeek);
        assert_eq!(provider.display_name, "DeepSeek");
        assert!(provider.api_key_configurable);
        assert!(provider.credential_id.is_some());

        let request = ModelRequest {
            model: ModelId::new("fixture-model"),
            messages: vec![ModelMessage::new(MessageRole::User, "hello")],
            tools: Vec::new(),
            options: Default::default(),
        };
        let mut provider = registry.create_provider(&request.model).unwrap();
        let events = provider
            .stream_collected(&request, &CancellationToken::new())
            .unwrap();
        assert!(events.iter().any(
            |event| matches!(event, ModelStreamEvent::TextDelta { text } if text == "DeepSeek fixture")
        ));
        server
            .join()
            .expect("fixture server panicked")
            .expect("fixture server failed");
    }

    #[test]
    fn deepseek_model_discovery_lists_all_advertised_models() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> std::result::Result<(), String> {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            let request = read_request_headers(&mut stream)?;
            if !request.starts_with("GET /models ")
                || !request.contains("Bearer deepseek-discovery-key")
            {
                return Err(
                    "DeepSeek model request had an unexpected path or credential".to_owned(),
                );
            }
            write_response(
                &mut stream,
                "application/json",
                r#"{"data":[{"id":"deepseek-flash"},{"id":"deepseek-v4-pro"}]}"#,
            )
        });
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let registry = ProviderRegistry::with_credentials(credentials);
        let mut config = ProviderConfig::deepseek(DEEPSEEK_DEFAULT_MODEL);
        config.endpoint = Some(format!("http://{address}/chat/completions"));
        registry.register(config).unwrap();
        registry
            .configure_api_key_provider(
                &ProviderId::new(DEEPSEEK_PROVIDER_ID),
                "deepseek-discovery-key".to_owned(),
            )
            .unwrap();

        let models = registry
            .discover_models(&ProviderId::new(DEEPSEEK_PROVIDER_ID))
            .unwrap();

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["deepseek-flash", "deepseek-v4-pro"]
        );
        assert_eq!(
            models[0].context_window,
            Some(1_000_000),
            "configured limits should carry over to discovered models"
        );
        server
            .join()
            .expect("fixture server panicked")
            .expect("fixture server failed");
    }

    #[test]
    fn registry_validates_provider_and_model_configuration_and_supports_catalog_updates() {
        let registry = ProviderRegistry::new();
        let credential = CredentialRef::new("fixture-token");
        let model = ModelDescriptor {
            id: ModelId::new("fixture/model"),
            provider: ProviderId::new("fixture"),
            display_name: "Fixture model".to_owned(),
            context_window: Some(8_000),
            max_input_tokens: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        };

        assert!(
            registry
                .register(ProviderConfig::openai_compatible(
                    "",
                    "empty",
                    "http://localhost/v1",
                    model.clone(),
                    None
                ))
                .is_err()
        );
        let mut no_models = ProviderConfig::deterministic();
        no_models.models.clear();
        assert!(registry.register(no_models).is_err());
        assert!(
            registry
                .register(ProviderConfig::openai_compatible(
                    "fixture",
                    "fixture",
                    "http://localhost/v1",
                    model.clone(),
                    Some(CredentialRef::new("  ")),
                ))
                .is_err()
        );
        assert!(registry.register(ProviderConfig::deterministic()).is_ok());
        let mut wrong_deterministic = ProviderConfig::deterministic();
        wrong_deterministic.models[0].id = ModelId::new("deterministic/other");
        assert!(registry.register(wrong_deterministic).is_err());

        registry
            .register(ProviderConfig::openai_compatible(
                "fixture",
                "Fixture provider",
                "http://localhost/v1/chat/completions",
                model.clone(),
                Some(credential),
            ))
            .unwrap();
        assert!(
            registry
                .add_model(&ProviderId::new("missing"), model.clone())
                .is_err()
        );
        let mut empty_id = model.clone();
        empty_id.id = ModelId::new(" ");
        assert!(
            registry
                .add_model(&ProviderId::new("fixture"), empty_id)
                .is_err()
        );
        assert!(
            registry
                .add_model(&ProviderId::new("fixture"), model.clone())
                .is_err()
        );
        let second = ModelDescriptor {
            id: ModelId::new("fixture/second"),
            provider: ProviderId::new("fixture"),
            display_name: "Second model".to_owned(),
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        };
        registry
            .add_model(&ProviderId::new("fixture"), second.clone())
            .unwrap();
        assert!(registry.describe_model(&second.id).is_ok());
        assert!(
            registry
                .describe_model(&ModelId::new("missing/model"))
                .is_err()
        );
        assert!(registry.pricing(&ModelId::new("fixture/model")).is_ok());
        assert!(
            registry
                .create_provider(&ModelId::new("missing/model"))
                .is_err()
        );
        assert!(
            registry
                .health(&ProviderId::new("missing"))
                .unwrap()
                .checked_at
                .is_none()
        );
    }

    #[test]
    fn registry_restores_copilot_configs_without_trusting_saved_model_lists() {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let registry = ProviderRegistry::with_credentials(credentials.clone());
        registry
            .configure_github_copilot("github-secret".to_owned())
            .unwrap();
        let copilot_model = ModelId::new(GITHUB_COPILOT_DEFAULT_MODEL);
        assert_eq!(
            registry
                .create_provider(&copilot_model)
                .unwrap()
                .descriptor()
                .id,
            copilot_model
        );
        let mut configs = registry.export_configs().unwrap();
        let copilot_provider = configs[0].id.clone();
        configs[0].models.push(ModelDescriptor {
            id: ModelId::new("untrusted/saved-model"),
            provider: copilot_provider,
            display_name: "Untrusted saved model".to_owned(),
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        });
        configs.push(ProviderConfig::github_copilot(CredentialRef::new(
            "missing",
        )));
        configs.push(ProviderConfig::deterministic());
        registry.restore_configs(configs).unwrap();
        let restored = registry.export_configs().unwrap();
        let copilot = restored
            .iter()
            .find(|config| config.kind == ProviderKind::GitHubCopilot)
            .unwrap();
        assert_eq!(copilot.models.len(), 1);
        assert!(
            !copilot
                .models
                .iter()
                .any(|model| model.id.as_str() == "untrusted/saved-model")
        );
        assert_eq!(registry.list_providers().unwrap().len(), 1);
    }

    #[test]
    fn legacy_api_key_configs_migrate_to_separate_backend_credential_files() {
        let root = std::env::temp_dir().join(format!(
            "loom-credential-migration-{}",
            loom_core::RequestId::new()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let legacy_path = root.join("legacy.json");
        let shared = Arc::new(FileCredentialStore::open(&legacy_path).unwrap());
        let old_reference = CredentialRef::new("legacy-openai-key");
        shared
            .store(&old_reference, "legacy-secret-value".to_owned())
            .unwrap();

        let make_registry = || {
            let registry = ProviderRegistry::with_credentials(shared.clone());
            let provider_id = ProviderId::new("openai-compatible");
            registry
                .register(ProviderConfig::openai_compatible(
                    provider_id.clone(),
                    "OpenAI-compatible",
                    "http://127.0.0.1:8000/v1/chat/completions",
                    ModelDescriptor {
                        id: ModelId::new("openai-compatible/model"),
                        provider: provider_id,
                        display_name: "Model".to_owned(),
                        context_window: None,
                        max_input_tokens: None,
                        max_output_tokens: None,
                        capabilities: ModelCapabilities::default(),
                    },
                    Some(old_reference.clone()),
                ))
                .unwrap();
            registry
        };
        let first = make_registry();
        let second = make_registry();
        let first_scoped =
            Arc::new(FileCredentialStore::open(root.join("first.credentials.json")).unwrap());
        let second_scoped =
            Arc::new(FileCredentialStore::open(root.join("second.credentials.json")).unwrap());
        first
            .scope_api_key_credentials(first_scoped.clone())
            .unwrap();
        second
            .scope_api_key_credentials(second_scoped.clone())
            .unwrap();

        let mut first_configs = first.export_configs().unwrap();
        let mut second_configs = second.export_configs().unwrap();
        assert!(
            first
                .migrate_api_key_credentials(&mut first_configs)
                .unwrap()
        );
        assert!(
            second
                .migrate_api_key_credentials(&mut second_configs)
                .unwrap()
        );
        first.restore_configs(first_configs).unwrap();
        second.restore_configs(second_configs).unwrap();

        let first_reference = first.list_providers().unwrap()[0]
            .credential_id
            .clone()
            .unwrap();
        let second_reference = second.list_providers().unwrap()[0]
            .credential_id
            .clone()
            .unwrap();
        assert_ne!(first_reference, second_reference);
        assert_eq!(
            first_scoped
                .resolve(&CredentialRef::new(first_reference))
                .unwrap(),
            "legacy-secret-value"
        );
        assert_eq!(
            second_scoped
                .resolve(&CredentialRef::new(second_reference))
                .unwrap(),
            "legacy-secret-value"
        );
        assert_eq!(
            shared.resolve(&old_reference).unwrap(),
            "legacy-secret-value"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_health_check_records_healthy_and_degraded_results() {
        let registry = ProviderRegistry::new();
        let provider_id = ProviderId::new("fixture");
        let model = ModelDescriptor {
            id: ModelId::new("fixture/model"),
            provider: provider_id.clone(),
            display_name: "Fixture model".to_owned(),
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        };

        let (endpoint, server) =
            serve_once(r#"{"data":[{"id":"fixture/model"}]}"#, "application/json");
        registry
            .register_openai_compatible(
                provider_id.clone(),
                "Fixture",
                format!("{endpoint}/chat/completions"),
                model.clone(),
                None,
            )
            .unwrap();
        let healthy = registry.check_health(&provider_id).unwrap();
        assert_eq!(healthy.state, ProviderHealthState::Healthy);
        assert_eq!(healthy.consecutive_failures, 0);
        server.join().unwrap().unwrap();

        let failed_registry = ProviderRegistry::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable_address = listener.local_addr().unwrap();
        drop(listener);
        failed_registry
            .register_openai_compatible(
                provider_id.clone(),
                "Fixture",
                format!("http://{unavailable_address}/v1/chat/completions"),
                model,
                None,
            )
            .unwrap();
        let degraded = failed_registry.check_health(&provider_id).unwrap();
        assert_eq!(degraded.state, ProviderHealthState::Degraded);
        assert_eq!(degraded.consecutive_failures, 1);
        assert!(degraded.last_error.is_some());

        assert!(
            failed_registry
                .check_health(&ProviderId::new("missing"))
                .is_err()
        );
        let empty_registry = ProviderRegistry::new();
        let mut empty_provider = ProviderConfig::ollama("http://127.0.0.1:1", "fixture/model");
        empty_provider.models.clear();
        empty_registry.register(empty_provider).unwrap_err();
    }

    #[test]
    fn provider_status_codes_are_normalized_without_response_body_or_keys() {
        let error = normalize_provider_error("openai", 429);
        assert_eq!(error.code, ErrorCode::ProviderRateLimited);
        assert!(error.retryable);
        assert!(!error.message.contains("secret"));
    }

    #[test]
    fn openai_compatible_provider_works_against_a_local_fixture() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> std::result::Result<(), String> {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            let request_text = read_request_headers(&mut stream)?;
            if !request_text.contains("raw-key-never-in-events") {
                return Err("fixture request did not contain the expected API key".to_owned());
            }
            let body = r#"{"choices":[{"message":{"content":"fixture response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
            write_response(&mut stream, "application/json", body)
        });
        let mut provider = OpenAiCompatibleProvider::new(
            format!("http://{address}/v1/chat/completions"),
            "raw-key-never-in-events",
            ModelId::new("fixture/model"),
        );
        let events = collect(
            &mut provider,
            &ModelRequest {
                model: ModelId::new("fixture/model"),
                messages: vec![ModelMessage::new(MessageRole::User, "hello")],
                tools: Vec::new(),
                options: Default::default(),
            },
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::TextDelta { text } if text == "fixture response"
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::Usage { usage }
                    if usage.input_tokens == 3 && usage.output_tokens == 2
            )
        }));
        server
            .join()
            .expect("fixture server thread panicked")
            .expect("fixture server failed");
        let redacted_error = normalize_transport_error("fixture", "Bearer raw-key-never-in-events");
        assert!(!redacted_error.message.contains("raw-key-never-in-events"));
    }

    #[test]
    fn registry_discovers_models_with_a_credential_reference() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> std::result::Result<(), String> {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            let request_text = read_request_headers(&mut stream)?;
            if !request_text.contains("discovery-key") {
                return Err("fixture request did not contain the expected discovery key".to_owned());
            }
            let body = r#"{"data":[{"id":"discovered/model"}]}"#;
            write_response(&mut stream, "application/json", body)
        });
        let credentials = Arc::new(InMemoryCredentialStore::default());
        credentials.insert(CredentialRef::new("gateway-key"), "discovery-key");
        let registry = ProviderRegistry::with_credentials(credentials);
        let descriptor = ModelDescriptor {
            id: ModelId::new("configured/model"),
            provider: ProviderId::new("gateway"),
            display_name: "Configured model".to_owned(),
            context_window: Some(4_096),
            max_input_tokens: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities {
                tool_calling: true,
                ..Default::default()
            },
        };
        registry
            .register(
                ProviderConfig::openai_compatible(
                    "gateway",
                    "Fixture gateway",
                    format!("http://{address}/v1/chat/completions"),
                    descriptor,
                    Some(CredentialRef::new("gateway-key")),
                )
                .with_pricing(100, 200),
            )
            .unwrap();
        let models = registry
            .discover_models(&ProviderId::new("gateway"))
            .unwrap();
        assert_eq!(models[0].id.as_str(), "discovered/model");
        assert_eq!(models[0].context_window, None);
        assert_eq!(models[0].provider.as_str(), "gateway");
        assert_eq!(
            registry.pricing(&ModelId::new("discovered/model")).unwrap(),
            (100, 200)
        );
        server
            .join()
            .expect("fixture server thread panicked")
            .expect("fixture server failed");
    }

    #[test]
    fn ollama_uses_the_openai_compatible_local_chat_endpoint() {
        let provider = OllamaProvider::new("http://127.0.0.1:11434/", ModelId::new("llama3.2"));
        assert_eq!(
            provider.endpoint(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
        assert_eq!(provider.descriptor().provider.as_str(), "ollama");
    }

    #[test]
    fn file_credential_store_round_trips_secrets_without_exposing_them_in_debug() {
        let path =
            std::env::temp_dir().join(format!("loom-credentials-test-{}.json", std::process::id()));
        let store = FileCredentialStore::open(&path).unwrap();
        store
            .store(&CredentialRef::new("test"), "secret-value".to_owned())
            .unwrap();
        assert_eq!(
            store.resolve(&CredentialRef::new("test")).unwrap(),
            "secret-value"
        );
        assert!(!format!("{store:?}").contains("secret-value"));
        let reopened = FileCredentialStore::open(&path).unwrap();
        assert_eq!(
            reopened.resolve(&CredentialRef::new("test")).unwrap(),
            "secret-value"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn credential_store_handles_missing_entries_removal_and_malformed_files() {
        let suffix = loom_core::ToolCallId::new().to_string();
        let directory = std::env::temp_dir().join(format!("loom-credential-errors-{suffix}"));
        let path = directory.join("credentials.json");
        let store = FileCredentialStore::open(&path).unwrap();
        let reference = CredentialRef::new("provider");
        assert_eq!(
            store.resolve(&reference).unwrap_err().code,
            ErrorCode::ProviderAuthentication
        );
        assert!(!store.remove(&reference).unwrap());
        store
            .insert(reference.clone(), "secret".to_owned())
            .unwrap();
        assert!(store.remove(&reference).unwrap());
        assert_eq!(
            FileCredentialStore::open(&path)
                .unwrap()
                .resolve(&reference)
                .unwrap_err()
                .code,
            ErrorCode::ProviderAuthentication
        );
        fs::write(&path, b"not-json").unwrap();
        assert_eq!(
            FileCredentialStore::open(&path).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn github_copilot_exchanges_github_token_before_chat_completion() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut token_stream, _) = listener.accept().unwrap();
            let token_request = read_request_headers(&mut token_stream).unwrap();
            assert!(token_request.lines().any(|line| {
                line.to_ascii_lowercase()
                    .starts_with("authorization: token ")
            }));
            let token_body =
                format!(r#"{{"token":"copilot-token","endpoints":{{"api":"http://{address}"}}}}"#);
            write_response(&mut token_stream, "application/json", &token_body).unwrap();

            let (mut chat_stream, _) = listener.accept().unwrap();
            let chat_request = read_request_headers(&mut chat_stream)
                .unwrap()
                .to_ascii_lowercase();
            assert!(
                chat_request
                    .lines()
                    .any(|line| line.starts_with("authorization: bearer "))
            );
            assert!(chat_request.contains("editor-version: vscode/1.96.2"));
            let chat_body = r#"{"choices":[{"message":{"content":"copilot response"},"finish_reason":"stop"}]}"#;
            write_response(&mut chat_stream, "application/json", chat_body).unwrap();
        });
        let descriptor = ModelDescriptor {
            id: ModelId::new("gpt-4o"),
            provider: ProviderId::new(GITHUB_COPILOT_PROVIDER_ID),
            display_name: "GitHub Copilot".to_owned(),
            context_window: Some(128_000),
            max_input_tokens: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
        };
        let mut provider = GitHubCopilotProvider::with_endpoints(
            format!("http://{address}"),
            format!("http://{address}/token"),
            "github-token",
            descriptor,
        );
        let events = collect(
            &mut provider,
            &ModelRequest {
                model: ModelId::new("gpt-4o"),
                messages: vec![ModelMessage::new(MessageRole::User, "hello")],
                tools: Vec::new(),
                options: Default::default(),
            },
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::TextDelta { text } if text == "copilot response"
            )
        }));
        server.join().unwrap();
    }
}
