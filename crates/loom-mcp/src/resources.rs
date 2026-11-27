//! Session logs as MCP resources, equivalent to `events` and `peek`.

use serde_json::{Value, json};

use crate::config::AppState;
use crate::jsonrpc::RpcError;
use crate::runner;

/// The resource templates advertised under `resources/templates/list`.
pub fn resource_templates() -> Vec<Value> {
    vec![
        json!({
            "uriTemplate": "loom://sessions/{session_id}/events",
            "name": "Session events",
            "description": "The session event transcript (loom-cli events), with an optional \
                            ?after=<sequence> cursor and a gap report when a snapshot could not \
                            carry the whole window.",
            "mimeType": "application/json",
        }),
        json!({
            "uriTemplate": "loom://sessions/{session_id}/peek",
            "name": "Session tail",
            "description": "The tail of the session's current output (loom-cli peek), with an \
                            optional ?lines=<n> bound.",
            "mimeType": "application/json",
        }),
    ]
}

/// Concrete resources; this server has none, because every session log is a
/// template instantiated by session id.
pub fn resources_list() -> Vec<Value> {
    Vec::new()
}

#[derive(Clone, Copy)]
enum Stream {
    Events,
    Peek,
}

struct ResourceRef {
    session: String,
    stream: Stream,
    after: Option<u64>,
    lines: Option<u32>,
}

fn parse_uri(uri: &str) -> Result<ResourceRef, RpcError> {
    let rest = uri
        .strip_prefix("loom://sessions/")
        .ok_or_else(|| RpcError::invalid_params(format!("unsupported resource uri '{uri}'")))?;
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, query),
        None => (rest, ""),
    };
    let (session, stream) = path
        .split_once('/')
        .ok_or_else(|| RpcError::invalid_params(format!("unsupported resource uri '{uri}'")))?;
    if session.is_empty() {
        return Err(RpcError::invalid_params(format!(
            "resource uri '{uri}' has no session id"
        )));
    }
    let stream = match stream {
        "events" => Stream::Events,
        "peek" => Stream::Peek,
        other => {
            return Err(RpcError::invalid_params(format!(
                "unsupported resource stream '{other}'"
            )));
        }
    };
    let mut after = None;
    let mut lines = None;
    for (key, value) in query.split('&').filter_map(|part| part.split_once('=')) {
        match key {
            "after" => {
                after = Some(value.parse::<u64>().map_err(|_| {
                    RpcError::invalid_params("resource 'after' must be a sequence number")
                })?);
            }
            "lines" => {
                lines = Some(value.parse::<u32>().map_err(|_| {
                    RpcError::invalid_params("resource 'lines' must be a line count")
                })?);
            }
            _ => {
                return Err(RpcError::invalid_params(format!(
                    "unsupported resource query '{key}'"
                )));
            }
        }
    }
    Ok(ResourceRef {
        session: session.to_owned(),
        stream,
        after,
        lines,
    })
}

/// Handles one `resources/read` request against the worker.
pub async fn read_resource(state: &AppState, params: &Value) -> Result<Value, RpcError> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("resources/read requires a 'uri'"))?;
    let resource = parse_uri(uri)?;
    let session = resource.session.clone();
    let mut argv = match resource.stream {
        Stream::Events => vec![
            "events".to_owned(),
            "--session".to_owned(),
            session.clone(),
            "--json".to_owned(),
        ],
        Stream::Peek => vec![
            "peek".to_owned(),
            "--session".to_owned(),
            session.clone(),
            "--json".to_owned(),
        ],
    };
    if let Some(after) = resource.after {
        argv.extend(["--after".to_owned(), after.to_string()]);
    }
    if let Some(lines) = resource.lines {
        argv.extend(["--lines".to_owned(), lines.to_string()]);
    }

    let execution = runner::run_verb(&state.config, &argv).await;
    let data = match &execution.result {
        Ok(outcome) => outcome.data.clone(),
        Err(failure) => {
            return Err(
                RpcError::server(format!("{}: {}", failure.code(), failure.message()))
                    .with_data(json!({ "code": failure.code(), "exit_code": execution.exit_code })),
            );
        }
    };
    let stream = match resource.stream {
        Stream::Events => "events",
        Stream::Peek => "peek",
    };
    let content = json!({
        "session": session,
        "stream": stream,
        "events": data.get("events").cloned().unwrap_or_else(|| json!([])),
        "gap": data.get("gap").cloned().unwrap_or(Value::Null),
    });
    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": content.to_string(),
        }]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_uris_parse_the_stream_and_cursor() {
        let events = parse_uri("loom://sessions/s1/events").unwrap();
        assert_eq!(events.session, "s1");
        assert!(matches!(events.stream, Stream::Events));
        assert_eq!(events.after, None);
        assert_eq!(events.lines, None);

        let peek = parse_uri("loom://sessions/s1/peek?lines=5").unwrap();
        assert!(matches!(peek.stream, Stream::Peek));
        assert_eq!(peek.lines, Some(5));

        let after = parse_uri("loom://sessions/s1/events?after=7").unwrap();
        assert_eq!(after.after, Some(7));

        assert!(parse_uri("http://sessions/s1/events").is_err());
        assert!(parse_uri("loom://sessions/s1/bogus").is_err());
        assert!(parse_uri("loom://sessions/s1/events?bad=1").is_err());
        assert!(parse_uri("loom://sessions//events").is_err());
    }
}
