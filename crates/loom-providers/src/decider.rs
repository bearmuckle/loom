//! The Jev decision-model client.
//!
//! One request carries a factual state and 1-6 choice questions; the answer is
//! one chosen label per question with the full distribution. The call is
//! bounded, retried once, and every failure falls back to the caller: no Jev
//! failure may fail the work that asked for a decision.
//!
//! The API key is resolved from the credential store when the client is built
//! and never appears in a log line, an error, or a `Debug` rendering.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use loom_core::{ErrorCode, LoomError, Result};
use loom_model::{
    CancellationToken, DECIDER_PROVIDER_ID, DecisionAnswer, DecisionDescriptor, DecisionProvider,
    DecisionRequest, DecisionResponse, ModelId, ProviderId, TokenUsage,
};
use serde::Deserialize;

use super::*;

/// A decision is a small, latency-sensitive call: a probe measured a median of
/// 0.16 s. Two seconds is generous for the model and still short enough that a
/// stalled endpoint cannot hold a tool call for long.
const DECIDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const DECIDER_ATTEMPTS: usize = 2;
/// Bounds the answer the client will decode, so a broken endpoint cannot stream
/// an unbounded body into memory.
const MAX_DECISION_RESPONSE_BYTES: usize = 256 * 1024;

/// The Jev decision model as a [`DecisionProvider`].
///
/// It is built per use from the registry entry, so a key replaced in Settings is
/// picked up by the next call. The key stays in this struct for the lifetime of
/// the call and is never rendered.
pub struct Decider {
    endpoint: String,
    model: ModelId,
    api_key: String,
}

impl Decider {
    pub fn new(endpoint: impl Into<String>, model: impl Into<ModelId>, api_key: String) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            api_key,
        }
    }

    fn request_body(&self, request: &DecisionRequest) -> serde_json::Value {
        let mut questions = serde_json::Map::new();
        for question in &request.questions {
            let mut criteria = serde_json::Map::new();
            for option in &question.options {
                criteria.insert(
                    option.label.clone(),
                    serde_json::Value::String(option.when.clone().unwrap_or_default()),
                );
            }
            let mut wire = serde_json::Map::new();
            wire.insert("type".to_owned(), "choice".into());
            if let Some(instructions) = &question.instructions {
                wire.insert("instructions".to_owned(), instructions.clone().into());
            }
            wire.insert("criteria".to_owned(), criteria.into());
            questions.insert(question.id.clone(), wire.into());
        }
        serde_json::json!({
            "model": self.model.as_str(),
            "state": request.state,
            "questions": questions,
        })
    }

    fn decide_once(&self, request: &DecisionRequest) -> Result<DecisionResponse> {
        let payload = self.request_body(request);
        let (status, body) = run_async(async {
            let client = shared_http_client()?;
            let response = client
                .post(trim_endpoint(&self.endpoint))
                .timeout(DECIDER_REQUEST_TIMEOUT)
                .header("Accept", "application/json")
                .header("Authorization", bearer_header(&self.api_key))
                .json(&payload)
                .send()
                .await
                .map_err(|error| transport_error("Jev", &error))?;
            let status = response.status().as_u16();
            let bytes = response
                .bytes()
                .await
                .map_err(|error| transport_error("Jev", &error))?;
            Ok((status, bytes))
        })?;
        if status != 200 {
            return Err(normalize_provider_error("Jev", status));
        }
        if body.len() > MAX_DECISION_RESPONSE_BYTES {
            return Err(invalid_response("the answer was too large to decode"));
        }
        let wire = serde_json::from_slice::<WireDecisionResponse>(&body)
            .map_err(|_| invalid_response("the answer was not a decision document"))?;
        let response = wire.into_response(&self.model)?;
        response
            .validate_against(request)
            .map_err(|reason| invalid_response(&reason))?;
        Ok(response)
    }
}

impl DecisionProvider for Decider {
    fn descriptor(&self) -> DecisionDescriptor {
        DecisionDescriptor {
            provider: ProviderId::new(DECIDER_PROVIDER_ID),
            model: self.model.clone(),
        }
    }

    fn decide(
        &self,
        request: &DecisionRequest,
        cancel: &CancellationToken,
    ) -> Result<DecisionResponse> {
        request
            .validate()
            .map_err(|reason| LoomError::invalid_request(format!("invalid decision: {reason}")))?;
        let mut attempt = 1;
        loop {
            cancel.check()?;
            let started = Instant::now();
            match self.decide_once(request) {
                Ok(response) => {
                    log::debug!(
                        "[loom-providers] Jev answered {} question(s) as {} in {} ms",
                        request.questions.len(),
                        response.model,
                        started.elapsed().as_millis()
                    );
                    return Ok(response);
                }
                Err(error) if error.retryable && attempt < DECIDER_ATTEMPTS => {
                    log::debug!(
                        "[loom-providers] Jev call failed ({}); retrying once",
                        error.message
                    );
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn invalid_response(reason: &str) -> LoomError {
    LoomError::new(
        ErrorCode::ProviderInvalidResponse,
        format!("Jev returned an invalid decision: {reason}"),
        false,
    )
}

/// The part of a decision answer this client reads. Both supported hosts answer
/// the same fields; only their usage and error envelopes differ, and only the
/// answer is needed here.
#[derive(Deserialize)]
struct WireDecisionResponse {
    #[serde(default)]
    #[serde(alias = "model_name")]
    model: Option<String>,
    #[serde(default)]
    answers: BTreeMap<String, WireDecisionAnswer>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireDecisionAnswer {
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    probabilities: Option<BTreeMap<String, f64>>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
}

impl WireDecisionResponse {
    fn into_response(self, requested: &ModelId) -> Result<DecisionResponse> {
        if self.answers.is_empty() {
            return Err(invalid_response("it contains no answers"));
        }
        let mut answers = BTreeMap::new();
        for (id, answer) in self.answers {
            let Some(choice) = answer.choice else {
                return Err(invalid_response(&format!("question '{id}' has no choice")));
            };
            let Some(confidence) = answer.confidence else {
                return Err(invalid_response(&format!(
                    "question '{id}' has no confidence"
                )));
            };
            let probabilities = answer.probabilities.unwrap_or_default();
            answers.insert(
                id,
                DecisionAnswer {
                    choice,
                    confidence,
                    probabilities,
                },
            );
        }
        Ok(DecisionResponse {
            answers,
            model: self
                .model
                .filter(|model| !model.trim().is_empty())
                .unwrap_or_else(|| requested.as_str().to_owned()),
            usage: self
                .usage
                .map(|usage| TokenUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cached_input_tokens: usage.cached_input_tokens,
                })
                .unwrap_or_default(),
        })
    }
}

/// Builds the decision client for the registry entry, or `None` when no usable
/// decider is configured.
pub fn decider_from_config(
    config: &ProviderConfig,
    api_key: String,
) -> Result<Arc<dyn DecisionProvider>> {
    let endpoint = config
        .endpoint
        .as_deref()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .ok_or_else(|| LoomError::invalid_request("the decider has no endpoint"))?;
    let model = config
        .models
        .first()
        .map(|model| model.id.clone())
        .ok_or_else(|| LoomError::invalid_request("the decider has no model"))?;
    Ok(Arc::new(Decider::new(endpoint, model, api_key)))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;
    use loom_model::{DecisionOption, DecisionQuestion};

    fn request() -> DecisionRequest {
        DecisionRequest {
            state: "the user names blue".to_owned(),
            questions: vec![DecisionQuestion {
                id: "colour".to_owned(),
                instructions: Some("Pick the colour the state names.".to_owned()),
                options: vec![
                    DecisionOption {
                        label: "red".to_owned(),
                        when: Some("the state names red".to_owned()),
                    },
                    DecisionOption::new("blue"),
                ],
            }],
        }
    }

    fn answer_body() -> &'static str {
        r#"{"model":"jev-1.13","answers":{"colour":{"type":"choice","choice":"blue","confidence":0.91,"probabilities":{"red":0.09,"blue":0.91}}},"usage":{"input_tokens":12,"output_tokens":3}}"#
    }

    /// Serves one HTTP response and returns the request it read.
    fn serve_once(status: u16, body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let text = String::from_utf8(request.clone()).unwrap();
            let length = text
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or_default();
            let body_start = text
                .find("\r\n\r\n")
                .map(|index| index + 4)
                .unwrap_or(request.len());
            while request.len() - body_start < length {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let sent = String::from_utf8_lossy(&request).into_owned();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
            sent
        });
        (format!("http://{address}/v1/systemone"), server)
    }

    #[test]
    fn a_choice_request_carries_a_criteria_dict_and_reads_the_answer() {
        let (endpoint, server) = serve_once(200, answer_body());
        let decider = Decider::new(endpoint, "jev-1.13", "test-key".to_owned());

        let response = decider
            .decide(&request(), &CancellationToken::new())
            .unwrap();

        let sent = server.join().unwrap();
        assert!(sent.starts_with("POST /v1/systemone "));
        assert!(
            sent.to_ascii_lowercase()
                .contains("authorization: bearer test-key")
        );
        assert!(sent.contains("\"type\":\"choice\""));
        assert!(sent.contains("\"red\":\"the state names red\""));
        assert!(sent.contains("\"blue\":\"\""));
        let answer = response.answers.get("colour").unwrap();
        assert_eq!(answer.choice, "blue");
        assert_eq!(answer.confidence, 0.91);
        assert_eq!(answer.probabilities.get("blue"), Some(&0.91));
        assert_eq!(response.model, "jev-1.13");
        assert_eq!(response.usage.input_tokens, 12);
    }

    #[test]
    fn an_unoffered_choice_is_discarded_instead_of_clamped() {
        let body = r#"{"answers":{"colour":{"choice":"green","confidence":1.0,"probabilities":{"green":1.0}}}}"#;
        let (endpoint, server) = serve_once(200, body);
        let decider = Decider::new(endpoint, "jev-1.13", "test-key".to_owned());

        let error = decider
            .decide(&request(), &CancellationToken::new())
            .unwrap_err();

        server.join().unwrap();
        assert_eq!(error.code, ErrorCode::ProviderInvalidResponse);
        assert!(error.message.contains("not offered"));
    }

    #[test]
    fn a_malformed_request_is_refused_before_it_is_sent() {
        let decider = Decider::new("http://127.0.0.1:9", "jev-1.13", "test-key".to_owned());
        let mut request = request();
        request.questions[0].options.clear();

        let error = decider
            .decide(&request, &CancellationToken::new())
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("at least 2 options"));
    }

    #[test]
    fn a_rejected_credential_maps_to_a_provider_error() {
        let (endpoint, server) = serve_once(401, "{}");
        let decider = Decider::new(endpoint, "jev-1.13", "wrong-key".to_owned());

        let error = decider
            .decide(&request(), &CancellationToken::new())
            .unwrap_err();

        server.join().unwrap();
        assert_eq!(error.code, ErrorCode::ProviderAuthentication);
        assert!(!error.message.contains("wrong-key"));
    }

    #[test]
    fn a_cancelled_call_is_refused_before_it_is_sent() {
        let decider = Decider::new("http://127.0.0.1:9", "jev-1.13", "test-key".to_owned());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = decider.decide(&request(), &cancel).unwrap_err();

        assert_eq!(error.code, ErrorCode::RequestCancelled);
    }
}
