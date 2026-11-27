//! The minimal JSON-RPC 2.0 / MCP message handling shared by both transports.

use serde_json::{Value, json};

use crate::config::AppState;
use crate::{resources, tools};

/// MCP protocol versions this server speaks; the client's requested version is
/// echoed when it is one of these.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-11-25"];

/// A JSON-RPC error, with the code the MCP transport reports.
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(-32600, message)
    }

    pub fn method_not_found() -> Self {
        Self::new(-32601, "method not found")
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::new(-32000, message)
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, error: &RpcError) -> Value {
    let mut object = json!({ "code": error.code, "message": error.message });
    if let Some(data) = &error.data {
        object["data"] = data.clone();
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": object })
}

/// Handles one parsed JSON-RPC message. `None` means the message was a valid
/// notification and produced no response; a malformed message always produces
/// an error so it is never mistaken for an acknowledged notification.
pub async fn handle_message(state: &AppState, message: &Value) -> Option<Value> {
    if let Err(reason) = validate_message(message) {
        return Some(rpc_error(
            error_id(message),
            &RpcError::invalid_request(reason),
        ));
    }
    let method = message.get("method").and_then(Value::as_str).unwrap();
    let Some(id) = message.get("id").cloned() else {
        // Notifications: acknowledged and ignored.
        return None;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let response = match dispatch(state, method, &params).await {
        Ok(result) => rpc_result(id, result),
        Err(error) => rpc_error(id, &error),
    };
    Some(response)
}

fn validate_message(message: &Value) -> Result<(), &'static str> {
    if !message.is_object() {
        return Err("message must be a JSON object");
    }
    let Some(Value::String(version)) = message.get("jsonrpc") else {
        return Err("message must declare \"jsonrpc\": \"2.0\"");
    };
    if version != "2.0" {
        return Err("message must declare \"jsonrpc\": \"2.0\"");
    }
    match message.get("method") {
        Some(Value::String(method)) if !method.is_empty() => {}
        Some(Value::String(_)) => return Err("method must not be empty"),
        Some(_) => return Err("method must be a string"),
        None => return Err("message has no method"),
    }
    if let Some(params) = message.get("params")
        && !params.is_object()
        && !params.is_array()
    {
        return Err("params must be an object or array");
    }
    if let Some(id) = message.get("id") {
        match id {
            Value::String(_) | Value::Number(_) => {}
            _ => return Err("id must be a string or number"),
        }
    }
    Ok(())
}

fn error_id(message: &Value) -> Value {
    match message.get("id") {
        Some(id @ (Value::String(_) | Value::Number(_))) => id.clone(),
        _ => Value::Null,
    }
}

async fn dispatch(state: &AppState, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => initialize(params),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list(state)),
        "tools/call" => tools::call_tool(state, params).await,
        "resources/list" => Ok(json!({ "resources": resources::resources_list() })),
        "resources/templates/list" => {
            Ok(json!({ "resourceTemplates": resources::resource_templates() }))
        }
        "resources/read" => resources::read_resource(state, params).await,
        _ => Err(RpcError::method_not_found()),
    }
}

fn initialize(params: &Value) -> Result<Value, RpcError> {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("initialize requires a protocolVersion string"))?;
    if !PROTOCOL_VERSIONS.contains(&requested) {
        return Err(RpcError::invalid_params(format!(
            "unsupported protocol version '{requested}'; supported versions: {}",
            PROTOCOL_VERSIONS.join(", ")
        )));
    }
    Ok(json!({
        "protocolVersion": requested,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
        },
        "serverInfo": {
            "name": "loom-mcp",
            "version": env!("CARGO_PKG_VERSION"),
        },
    }))
}

fn tools_list(state: &AppState) -> Value {
    let tools = tools::tool_specs()
        .into_iter()
        .filter(|spec| tools::tool_allowed(spec.name, &state.config))
        .map(|spec| spec.to_json())
        .collect::<Vec<_>>();
    json!({ "tools": tools })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, HttpLimits, Secret};

    fn state(read_only: bool, allow_mutating: bool) -> AppState {
        AppState {
            config: Config {
                connect: "ws://127.0.0.1:1/ws".to_owned(),
                token: Secret::new("token".to_owned()).unwrap(),
                ca_file: None,
                allow_insecure_remote: false,
                max_frame_bytes: None,
                bind: "127.0.0.1:8766".parse().unwrap(),
                tls: None,
                read_only,
                allow_mutating,
                allowed_hosts: Vec::new(),
            },
            limits: HttpLimits::default(),
        }
    }

    #[tokio::test]
    async fn initialize_echoes_a_supported_version() {
        let state = state(false, false);
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" },
        });
        let response = handle_message(&state, &request).await.unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(response["result"]["serverInfo"]["name"], "loom-mcp");

        let unsupported = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": { "protocolVersion": "1999-01-01" },
        });
        let response = handle_message(&state, &unsupported).await.unwrap();
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unsupported protocol version")
        );

        let missing = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "initialize",
            "params": {},
        });
        let response = handle_message(&state, &missing).await.unwrap();
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn tools_list_hides_mutating_tools_until_opt_in() {
        let names = |state: &AppState| {
            tools_list(state)["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&state(false, true)),
            vec![
                "ls",
                "status",
                "run",
                "send",
                "approve",
                "deny",
                "archive",
                "models_list",
                "providers_list",
            ]
        );
        assert_eq!(
            names(&state(false, false)),
            vec!["ls", "status", "models_list", "providers_list"]
        );
        assert_eq!(
            names(&state(true, true)),
            vec!["ls", "status", "models_list", "providers_list"]
        );
    }

    #[tokio::test]
    async fn notifications_are_acknowledged_and_unknown_methods_error() {
        let state = state(false, false);
        let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_message(&state, &notification).await.is_none());

        let unknown = json!({ "jsonrpc": "2.0", "id": 3, "method": "bogus" });
        let response = handle_message(&state, &unknown).await.unwrap();
        assert_eq!(response["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn malformed_messages_return_invalid_request_errors() {
        let state = state(false, false);
        for (message, expected_id) in [
            (json!(123), Value::Null),
            (json!({}), Value::Null),
            (json!([]), Value::Null),
            (json!({ "id": 1, "method": "ping" }), json!(1)),
            (
                json!({ "jsonrpc": "2.0", "method": "ping", "params": 7 }),
                Value::Null,
            ),
            (
                json!({ "jsonrpc": "2.0", "id": {}, "method": "ping" }),
                Value::Null,
            ),
        ] {
            let response = handle_message(&state, &message)
                .await
                .expect("a malformed message still gets a response");
            assert_eq!(response["error"]["code"], -32600, "{message}");
            assert_eq!(response["id"], expected_id, "{message}");
        }
    }
}
