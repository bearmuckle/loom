//! The decision-model client this CLI drives itself.
//!
//! Jev is TypeSafe's System One decision model: one factual `state` and one
//! closed question in, one choice with a full probability distribution out. The
//! design sketch places this use in the slice that "could live entirely in
//! `loom-cli`", so nothing here reaches the worker and no core crate knows the
//! model exists.
//!
//! The key is read from a file, sent only in the `Authorization` header, and
//! never printed, echoed in an error message, or stored in a parsed command.
//! Every call carries `User-Agent: loom-cli/<version>`, so the service can tell
//! this client's traffic apart.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use loom_core::{ErrorCode, LoomError};
use serde_json::Value;

/// The header every call sends, which names this client and its version.
pub(crate) const USER_AGENT: &str = concat!("loom-cli/", env!("CARGO_PKG_VERSION"));
/// How long one decision call may take before it is reported as unreachable.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// The id of the one question every call asks, which keys the answer.
pub(crate) const QUESTION_ID: &str = "decision";
/// How much of the service's error body is reported, so one line stays readable.
const MAX_DETAIL_BYTES: usize = 300;

/// Which service a Jev call goes to. The two hosts speak the same request and
/// answer shape, so one client swaps them by base URL, model, and key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Source {
    TypeSafe,
    OpenCode,
}

impl Source {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "typesafe" => Some(Self::TypeSafe),
            "opencode" => Some(Self::OpenCode),
            _ => None,
        }
    }

    /// The word the flags, the text output, and the `--json` payload use.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::TypeSafe => "typesafe",
            Self::OpenCode => "opencode",
        }
    }

    /// The systemone endpoint of the service.
    pub(crate) const fn endpoint(self) -> &'static str {
        match self {
            Self::TypeSafe => "https://api.typesafe.ai/v1/systemone",
            Self::OpenCode => "https://opencode.ai/inference/systemone/v1/systemone",
        }
    }

    /// The model the service serves this client by default. TypeSafe names an
    /// alias the service resolves, OpenCode names a pinned free model.
    pub(crate) const fn model(self) -> &'static str {
        match self {
            Self::TypeSafe => "jev-latest",
            Self::OpenCode => "jev-1.13-free",
        }
    }

    /// The key file this source reads when `--key-file` is absent.
    fn default_key_path(self, home: Option<&Path>) -> Option<PathBuf> {
        let home = home?;
        Some(
            home.join(".config")
                .join("loom")
                .join(format!("{}.key", self.name())),
        )
    }
}

/// Where one command reads its key and what it calls.
///
/// Resolution happens once, before any request, so `jev status` can report
/// exactly what a call would do without making one.
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    pub(crate) source: Source,
    /// Whether `--source` named the source instead of auto-detection.
    pub(crate) source_explicit: bool,
    pub(crate) endpoint: String,
    pub(crate) model: String,
    pub(crate) key_file: PathBuf,
}

impl Settings {
    /// The key, trimmed. A missing or unreadable file is a usage error that
    /// names the path and never its contents.
    pub(crate) fn key(&self) -> Result<String, String> {
        let text = std::fs::read_to_string(&self.key_file).map_err(|error| {
            format!(
                "could not read the Jev key file '{}': {error}",
                self.key_file.display()
            )
        })?;
        let key = text.trim();
        if key.is_empty() {
            return Err(format!(
                "the Jev key file '{}' is empty",
                self.key_file.display()
            ));
        }
        Ok(key.to_owned())
    }

    /// Whether the key file holds a key, which is what `jev status` reports.
    pub(crate) fn key_configured(&self) -> bool {
        self.key().is_ok()
    }
}

/// Resolves the source, endpoint, model, and key path of one command.
///
/// `--source` wins. Without it the first source whose default key file exists is
/// used, typesafe first, so a machine with only the OpenCode key needs no flag;
/// when neither exists the answer is typesafe, which makes `jev status` report
/// the path it would read. `--endpoint` and `--model` override the source's.
pub(crate) fn select(
    flags: &crate::cli::args::JevFlags,
    home: Option<&Path>,
) -> Result<Settings, String> {
    let source_explicit = flags.source.is_some();
    let source = match &flags.source {
        Some(value) => Source::parse(value)
            .ok_or_else(|| format!("--source must be typesafe or opencode, got '{value}'"))?,
        None => detect_source(home),
    };
    let endpoint = match &flags.endpoint {
        Some(endpoint) => endpoint.clone(),
        None => source.endpoint().to_owned(),
    };
    let model = match &flags.model {
        Some(model) => model.clone(),
        None => source.model().to_owned(),
    };
    let key_file = match &flags.key_file {
        Some(path) => expand_home(path, home),
        None => source.default_key_path(home).ok_or_else(|| {
            format!(
                "could not derive the default {} key path: HOME is not set; pass --key-file <path>",
                source.name()
            )
        })?,
    };
    Ok(Settings {
        source,
        source_explicit,
        endpoint,
        model,
        key_file,
    })
}

fn detect_source(home: Option<&Path>) -> Source {
    for source in [Source::TypeSafe, Source::OpenCode] {
        let Some(path) = source.default_key_path(home) else {
            continue;
        };
        if path.is_file() {
            return source;
        }
    }
    Source::TypeSafe
}

/// Expands a leading `~` with `HOME`, as the token file's path is expanded.
fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// One answer to a `choice` question.
///
/// `confidence` is the model's own number and is kept optional: an answer that
/// does not carry one must never clear a threshold. `probabilities` is sorted by
/// label so two runs over the same answer print identical bytes.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Answer {
    pub(crate) choice: String,
    pub(crate) confidence: Option<f64>,
    pub(crate) probabilities: Vec<(String, f64)>,
    /// The model the service reported, which may differ from the alias asked for.
    pub(crate) model: String,
}

impl Answer {
    /// Whether the answer clears `threshold`, which a missing confidence never
    /// does.
    pub(crate) fn clears(&self, threshold: f64) -> bool {
        self.confidence
            .is_some_and(|confidence| confidence >= threshold)
    }
}

/// Asks one `choice` question and returns the parsed answer.
///
/// The request is the documented systemone shape: one `state`, and a `questions`
/// map whose entry is a `choice` question with `criteria` as a dict of label to
/// "when it applies". A 400 or 422 is the caller's own request being wrong, so it
/// is reported as `invalid_request`; a refused key is `authentication_failed`; a
/// rate limit and an unreachable host are retryable provider failures.
pub(crate) async fn ask(
    settings: &Settings,
    key: &str,
    state: &str,
    instructions: &str,
    options: &[(String, String)],
) -> Result<Answer, LoomError> {
    let mut criteria = serde_json::Map::new();
    for (label, when) in options {
        criteria.insert(label.clone(), Value::String(when.clone()));
    }
    let mut question = serde_json::Map::new();
    question.insert("type".to_owned(), Value::String("choice".to_owned()));
    if !instructions.trim().is_empty() {
        question.insert(
            "instructions".to_owned(),
            Value::String(instructions.to_owned()),
        );
    }
    question.insert("criteria".to_owned(), Value::Object(criteria));
    let mut questions = serde_json::Map::new();
    questions.insert(QUESTION_ID.to_owned(), Value::Object(question));
    let body = serde_json::json!({
        "model": settings.model,
        "state": state,
        "questions": Value::Object(questions),
    });

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(CALL_TIMEOUT)
        .build()
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not build the HTTP client: {error}"),
                false,
            )
        })?;
    let response = client
        .post(&settings.endpoint)
        .header("authorization", format!("Bearer {key}"))
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|error| {
            LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("could not reach {}: {error}", settings.endpoint),
                true,
            )
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|error| {
        LoomError::new(
            ErrorCode::ProviderUnavailable,
            format!(
                "could not read the answer from {}: {error}",
                settings.endpoint
            ),
            true,
        )
    })?;
    if !status.is_success() {
        return Err(http_error(status.as_u16(), &text, &settings.endpoint));
    }
    let body: Value = serde_json::from_str(&text).map_err(|error| {
        LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!("the answer from {} is not JSON: {error}", settings.endpoint),
            false,
        )
    })?;
    answer_from_body(&body, &settings.model)
}

/// The failure one non-2xx answer maps to, with the service's own detail.
fn http_error(status: u16, text: &str, endpoint: &str) -> LoomError {
    let detail = error_detail(text);
    match status {
        401 | 403 => LoomError::new(
            ErrorCode::AuthenticationFailed,
            format!("{endpoint} refused the Jev key: {detail}"),
            false,
        ),
        402 => LoomError::new(
            ErrorCode::ProviderAuthentication,
            format!("{endpoint} has no funds for this model: {detail}"),
            false,
        ),
        429 => LoomError::new(
            ErrorCode::ProviderRateLimited,
            format!("{endpoint} rate limited the call: {detail}"),
            true,
        ),
        400 | 422 => LoomError::new(
            ErrorCode::InvalidRequest,
            format!("{endpoint} rejected the request: {detail}"),
            false,
        ),
        other => LoomError::new(
            ErrorCode::ProviderUnavailable,
            format!("{endpoint} answered HTTP {other}: {detail}"),
            status >= 500,
        ),
    }
}

/// The service's own explanation of a failure: TypeSafe nests it under `detail`,
/// OpenCode under `error`, and anything else is the bounded body.
fn error_detail(text: &str) -> String {
    let described = serde_json::from_str::<Value>(text).ok().and_then(|value| {
        let detail = value.get("detail").or_else(|| value.get("error"))?;
        match detail {
            Value::String(message) => Some(message.clone()),
            Value::Object(object) => {
                let kind = ["error_type", "type"]
                    .iter()
                    .find_map(|key| object.get(*key).and_then(Value::as_str))
                    .unwrap_or_default();
                let message = object
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Some(match (kind.is_empty(), message.is_empty()) {
                    (false, false) => format!("{kind}: {message}"),
                    (false, true) => kind.to_owned(),
                    _ => message.to_owned(),
                })
            }
            _ => None,
        }
    });
    match described {
        Some(detail) if !detail.trim().is_empty() => bound_line(&detail),
        _ => bound_line(text),
    }
}

/// One line of at most [`MAX_DETAIL_BYTES`], so a failure message stays a line.
fn bound_line(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.len() <= MAX_DETAIL_BYTES {
        return flattened;
    }
    let mut end = MAX_DETAIL_BYTES;
    while end > 0 && !flattened.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &flattened[..end])
}

/// Parses a `choice` answer out of the response body.
///
/// The question map is keyed by id, so the answer is read from `answers[<id>]`.
/// Peers that answer with the bare answer, or with a list of answers, are
/// accepted as well, because the shape is the one part of the wire this client
/// cannot pin from the published probe.
pub(crate) fn answer_from_body(body: &Value, requested_model: &str) -> Result<Answer, LoomError> {
    let answer = answer_value(body).ok_or_else(|| {
        LoomError::new(
            ErrorCode::ProviderInvalidResponse,
            format!(
                "the answer carries no choice for '{QUESTION_ID}': {}",
                bound_line(&body.to_string())
            ),
            false,
        )
    })?;
    let choice = answer
        .get("choice")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|choice| !choice.is_empty())
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::ProviderInvalidResponse,
                format!(
                    "the answer carries no choice: {}",
                    bound_line(&answer.to_string())
                ),
                false,
            )
        })?;
    let mut probabilities = answer
        .get("probabilities")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(label, value)| Some((label.clone(), value.as_f64()?)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    probabilities.sort_by(|left, right| left.0.cmp(&right.0));
    let model = answer
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| body.get("model").and_then(Value::as_str))
        .unwrap_or(requested_model)
        .to_owned();
    Ok(Answer {
        choice: choice.to_owned(),
        confidence: answer.get("confidence").and_then(Value::as_f64),
        probabilities,
        model,
    })
}

/// The one answer object, wherever the envelope puts it.
fn answer_value(body: &Value) -> Option<&Value> {
    let answers = body.get("answers");
    match answers {
        Some(Value::Object(map)) => map
            .get(QUESTION_ID)
            .filter(|answer| answer.get("choice").is_some())
            .or_else(|| map.values().find(|answer| answer.get("choice").is_some())),
        Some(Value::Array(items)) => items
            .iter()
            .find(|item| {
                let id = ["id", "question_id", "name"]
                    .iter()
                    .find_map(|key| item.get(*key).and_then(Value::as_str));
                id.is_some_and(|id| id == QUESTION_ID) && item.get("choice").is_some()
            })
            .or_else(|| items.iter().find(|item| item.get("choice").is_some())),
        _ => body.get("choice").map(|_| body),
    }
}

/// The tools `jev triage --apply` may approve on its own.
///
/// Deliberately a strict allow-list rather than a copy of the tool catalogue's
/// action classification: a tool this client does not know is never approved
/// automatically, so the guard cannot quietly gain the power to approve a write,
/// a command, or a network action by being kept in step with another crate.
pub(crate) const READ_TOOLS: [&str; 4] = ["glob", "list_files", "read_file", "search_text"];

/// Why `--apply` refused to approve the pending call, as a stable token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardRefusal {
    /// The pending call is not one of [`READ_TOOLS`].
    NotAReadTool,
    /// A path argument resolves outside the session root, or cannot be checked.
    PathOutsideRoot,
}

impl GuardRefusal {
    /// The word the text line and the `--json` payload report.
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::NotAReadTool => "not-a-read-tool",
            Self::PathOutsideRoot => "path-outside-session-root",
        }
    }
}

/// Whether one pending call is safe for this client to approve by itself.
///
/// It is safe only when the call is a read tool and every path argument it names
/// resolves inside the session root the worker reported. The check is lexical,
/// because the paths are the worker's and this client may not be able to open
/// them at all; it therefore refuses anything it cannot resolve rather than
/// guessing on the worker's behalf.
pub(crate) fn guard(tool: &str, arguments: &Value, root: &str) -> Result<(), GuardRefusal> {
    if !READ_TOOLS.contains(&tool) {
        return Err(GuardRefusal::NotAReadTool);
    }
    if root.trim().is_empty() {
        return Err(GuardRefusal::PathOutsideRoot);
    }
    for key in ["path", "paths"] {
        let Some(value) = arguments.get(key) else {
            continue;
        };
        let Some(paths) = path_strings(value) else {
            return Err(GuardRefusal::PathOutsideRoot);
        };
        for path in paths {
            if !resolves_inside(root, &path) {
                return Err(GuardRefusal::PathOutsideRoot);
            }
        }
    }
    Ok(())
}

/// The paths one argument value names, or `None` when it is not a path this
/// client can check.
fn path_strings(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::String(path) => Some(vec![path.clone()]),
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    }
}

/// Whether `path` stays inside `root`, resolved lexically.
///
/// A relative path is joined to `root`; an absolute one has to start at `root`.
/// `..` that would climb above `root`, and an absolute path elsewhere, are both
/// outside.
fn resolves_inside(root: &str, path: &str) -> bool {
    let parts_of = |value: &str| {
        value
            .split('/')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let root_parts = parts_of(root);
    let absolute = path.starts_with('/');
    let mut parts: Vec<String> = if absolute {
        Vec::new()
    } else {
        root_parts.clone()
    };
    let floor = parts.len();
    for part in parts_of(path) {
        match part.as_str() {
            "." => {}
            ".." => {
                if parts.len() <= floor {
                    return false;
                }
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    if absolute {
        parts.starts_with(&root_parts)
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        Answer, GuardRefusal, Settings, Source, USER_AGENT, answer_from_body, error_detail, guard,
        resolves_inside, select,
    };
    use crate::cli::args::JevFlags;

    fn settings(flags: JevFlags) -> Settings {
        select(&flags, Some(std::path::Path::new("/home/tester"))).expect("a resolved source")
    }

    #[test]
    fn each_source_names_its_endpoint_model_and_key_file() {
        assert_eq!(Source::parse("typesafe"), Some(Source::TypeSafe));
        assert_eq!(Source::parse("opencode"), Some(Source::OpenCode));
        assert_eq!(Source::parse("OpenAI"), None);
        assert_eq!(
            Source::TypeSafe.endpoint(),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            Source::OpenCode.endpoint(),
            "https://opencode.ai/inference/systemone/v1/systemone"
        );
        assert_eq!(Source::TypeSafe.model(), "jev-latest");
        assert_eq!(Source::OpenCode.model(), "jev-1.13-free");
    }

    #[test]
    fn the_key_path_is_the_source_default_unless_a_flag_names_one() {
        let default = settings(JevFlags {
            source: Some("opencode".to_owned()),
            ..JevFlags::default()
        });
        assert_eq!(
            default.key_file,
            std::path::PathBuf::from("/home/tester/.config/loom/opencode.key")
        );
        assert_eq!(default.endpoint, Source::OpenCode.endpoint());
        assert_eq!(default.model, Source::OpenCode.model());
        assert!(default.source_explicit);

        let named = settings(JevFlags {
            source: Some("typesafe".to_owned()),
            key_file: Some(std::path::PathBuf::from("~/elsewhere/jev.key")),
            endpoint: Some("http://127.0.0.1:8899/v1/systemone".to_owned()),
            model: Some("jev-test".to_owned()),
        });
        assert_eq!(
            named.key_file,
            std::path::PathBuf::from("/home/tester/elsewhere/jev.key")
        );
        assert_eq!(named.endpoint, "http://127.0.0.1:8899/v1/systemone");
        assert_eq!(named.model, "jev-test");
    }

    #[test]
    fn the_default_source_is_detected_from_the_key_files_that_exist() {
        // A home with no key at all reports typesafe, so `status` names the path
        // it would read instead of failing.
        let empty = std::env::temp_dir().join(format!("loom-jev-home-{}", std::process::id()));
        std::fs::create_dir_all(empty.join(".config/loom")).unwrap();
        let detected = select(&JevFlags::default(), Some(&empty)).unwrap();
        assert_eq!(detected.source, Source::TypeSafe);
        assert!(!detected.source_explicit);

        // The OpenCode key alone selects OpenCode without a flag.
        std::fs::write(empty.join(".config/loom/opencode.key"), "opencode-value").unwrap();
        let detected = select(&JevFlags::default(), Some(&empty)).unwrap();
        assert_eq!(detected.source, Source::OpenCode);
        assert!(detected.key_configured());
        assert_eq!(detected.key().unwrap(), "opencode-value");

        // TypeSafe wins when both exist, because it is the first source.
        std::fs::write(empty.join(".config/loom/typesafe.key"), "typesafe-value\n").unwrap();
        let detected = select(&JevFlags::default(), Some(&empty)).unwrap();
        assert_eq!(detected.source, Source::TypeSafe);
        assert_eq!(detected.key().unwrap(), "typesafe-value");

        // A blank key file is not a configured key, and its message names the
        // path rather than the contents.
        std::fs::write(empty.join(".config/loom/typesafe.key"), "  \n").unwrap();
        let blank = select(&JevFlags::default(), Some(&empty)).unwrap();
        assert!(!blank.key_configured());
        assert!(blank.key().unwrap_err().contains("is empty"));
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn a_missing_home_cannot_derive_a_default_key_path() {
        let error = select(&JevFlags::default(), None).unwrap_err();
        assert!(error.contains("HOME is not set"), "{error}");
        assert!(error.contains("--key-file"), "{error}");
    }

    #[test]
    fn the_user_agent_names_this_client_and_its_version() {
        assert_eq!(USER_AGENT, concat!("loom-cli/", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn an_answer_is_read_from_the_question_map_or_a_bare_answer() {
        let keyed = json!({
            "answers": {"decision": {
                "type": "choice", "choice": "approve", "confidence": 0.86,
                "probabilities": {"ask_user": 0.14, "approve": 0.86}
            }},
            "model": "jev-1.14.0"
        });
        let answer = answer_from_body(&keyed, "jev-latest").unwrap();
        assert_eq!(answer.choice, "approve");
        assert_eq!(answer.confidence, Some(0.86));
        assert_eq!(answer.model, "jev-1.14.0");
        assert_eq!(
            answer.probabilities,
            vec![("approve".to_owned(), 0.86), ("ask_user".to_owned(), 0.14),],
            "the labels are sorted, so the text output is byte-stable"
        );
        assert!(answer.clears(0.86));
        assert!(!answer.clears(0.9));

        // A bare answer, and a list of answers, are accepted as well.
        let bare = json!({"choice": "ask_user", "confidence": 0.2});
        assert_eq!(
            answer_from_body(&bare, "jev-latest").unwrap().choice,
            "ask_user"
        );
        let listed = json!({"answers": [
            {"id": "other", "choice": "wrong"},
            {"id": "decision", "choice": "approve", "confidence": 1.0}
        ]});
        let answer = answer_from_body(&listed, "jev-latest").unwrap();
        assert_eq!(answer.choice, "approve");
        assert_eq!(
            answer.model, "jev-latest",
            "the requested model is the fallback"
        );
    }

    #[test]
    fn an_answer_without_a_choice_is_a_malformed_response() {
        for body in [
            json!({}),
            json!({"answers": {"decision": {"type": "choice"}}}),
            json!({"answers": {"decision": {"choice": "  "}}}),
        ] {
            let error = answer_from_body(&body, "jev-latest").unwrap_err();
            assert_eq!(
                error.code,
                loom_core::ErrorCode::ProviderInvalidResponse,
                "{body}"
            );
        }
    }

    #[test]
    fn a_missing_confidence_never_clears_a_threshold() {
        let answer = Answer {
            choice: "approve".to_owned(),
            confidence: None,
            ..Answer::default()
        };
        assert!(!answer.clears(0.0));
    }

    #[test]
    fn a_failure_detail_is_read_from_either_envelope() {
        assert_eq!(
            error_detail(
                r#"{"detail":{"error_type":"api_usage_error","message":"Unknown model"}}"#
            ),
            "api_usage_error: Unknown model"
        );
        assert_eq!(
            error_detail(r#"{"detail":"Noul question must have criteria"}"#),
            "Noul question must have criteria"
        );
        assert_eq!(
            error_detail(r#"{"error":{"type":"invalid_request_error","message":"no funds"}}"#),
            "invalid_request_error: no funds"
        );
        assert_eq!(error_detail("plain text"), "plain text");
        let long = error_detail(&"x".repeat(1000));
        assert!(long.len() < 400, "{long}");
        assert!(long.ends_with('…'));
    }

    #[test]
    fn only_a_read_tool_inside_the_session_root_may_be_approved() {
        let root = "/work/sessions/abc";
        assert_eq!(
            guard("read_file", &json!({"path": "src/lib.rs"}), root),
            Ok(())
        );
        assert_eq!(guard("list_files", &json!({"path": "."}), root), Ok(()));
        assert_eq!(
            guard("glob", &json!({"path": "src", "pattern": "**/*.rs"}), root),
            Ok(())
        );
        assert_eq!(
            guard(
                "search_text",
                &json!({"paths": [root, "/work/sessions/abc/src"]}),
                root
            ),
            Ok(())
        );
        // A path argument that is absent defaults to the root itself.
        assert_eq!(guard("list_files", &json!({}), root), Ok(()));

        assert_eq!(
            guard("apply_patch", &json!({"path": "a.txt"}), root),
            Err(GuardRefusal::NotAReadTool)
        );
        assert_eq!(
            guard("run_command", &json!({"command": "rm"}), root),
            Err(GuardRefusal::NotAReadTool)
        );
        assert_eq!(
            guard("read_file", &json!({"path": "../other/secret"}), root),
            Err(GuardRefusal::PathOutsideRoot)
        );
        assert_eq!(
            guard("read_file", &json!({"path": "/etc/passwd"}), root),
            Err(GuardRefusal::PathOutsideRoot)
        );
        assert_eq!(
            guard("read_file", &json!({"path": root}), root),
            Ok(()),
            "the root itself is inside the root"
        );
        assert_eq!(
            guard("read_file", &json!({"path": 3}), root),
            Err(GuardRefusal::PathOutsideRoot),
            "a path this client cannot check is refused"
        );
        assert_eq!(
            guard("read_file", &json!({"path": "a"}), ""),
            Err(GuardRefusal::PathOutsideRoot),
            "an unknown root cannot be verified"
        );
    }

    #[test]
    fn a_path_resolves_inside_lexically() {
        assert!(resolves_inside("/a/b", "c"));
        assert!(resolves_inside("/a/b", "./c/../d"));
        assert!(resolves_inside("/a/b", "/a/b/c"));
        assert!(!resolves_inside("/a/b", ".."));
        assert!(!resolves_inside("/a/b", "c/../../outside"));
        assert!(!resolves_inside("/a/b", "/a/bc"));
        assert!(!resolves_inside("/a/b", "/a/c/../b/../c"));
        assert!(resolves_inside("/", "any"));
    }
}
