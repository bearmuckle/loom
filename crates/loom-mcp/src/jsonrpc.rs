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

/// Handles one parsed JSON-RPC message. `None` means the message was a
/// notification and produced no response.
pub async fn handle_message(state: &AppState, message: &Value) -> Option<Value> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return message
            .get("id")
            .cloned()
            .map(|id| rpc_error(id, &RpcError::invalid_request("message has no method")));
    };
    let id = message.get("id").cloned();
    match id {
        None => {
            // Notifications: acknowledged and ignored.
            match method {
                "notifications/initialized" | "notifications/cancelled" => {}
                _ => {}
            }
            None
        }
        Some(id) => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let response = match dispatch(state, method, &params).await {
                Ok(result) => rpc_result(id, result),
                Err(error) => rpc_error(id, &error),
            };
            Some(response)
        }
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
        .unwrap_or_default();
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
        .filter(|spec| tools::tool_allowed(spec.name, state.config.read_only))
        .map(|spec| spec.to_json())
        .collect::<Vec<_>>();
    json!({ "tools": tools })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Secret};

    fn state(read_only: bool) -> AppState {
        AppState {
            config: Config {
                connect: "ws://127.0.0.1:1/ws".to_owned(),
                token: Secret::new("token".to_owned()),
                ca_file: None,
                allow_insecure_remote: false,
                max_frame_bytes: None,
                bind: "127.0.0.1:8766".parse().unwrap(),
                tls: None,
                read_only,
            },
        }
    }

    #[tokio::test]
    async fn initialize_echoes_a_supported_version() {
        let state = state(false);
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
    }

    #[test]
    fn tools_list_hides_mutating_tools_in_read_only_mode() {
        let names = |state: &AppState| {
            tools_list(state)["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&state(false)),
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
            names(&state(true)),
            vec!["ls", "status", "models_list", "providers_list"]
        );
    }

    #[tokio::test]
    async fn notifications_are_acknowledged_and_unknown_methods_error() {
        let state = state(false);
        let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_message(&state, &notification).await.is_none());

        let unknown = json!({ "jsonrpc": "2.0", "id": 3, "method": "bogus" });
        let response = handle_message(&state, &unknown).await.unwrap();
        assert_eq!(response["error"]["code"], -32601);
    }
}
