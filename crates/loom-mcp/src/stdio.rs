//! Stdio transport: newline-delimited JSON-RPC over stdin/stdout.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::AppState;
use crate::jsonrpc;

/// Serves MCP over stdin/stdout until the input closes. Stdio trusts the local
/// caller: the bearer-token check belongs to the HTTP listener, because a local
/// process that can launch this binary can already read the token file.
pub async fn serve(state: Arc<AppState>) -> Result<(), String> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|error| format!("could not read stdio: {error}"))?;
        if read == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(message) => message,
            Err(_) => continue,
        };
        if let Some(response) = jsonrpc::handle_message(state.as_ref(), &message).await {
            let mut bytes = response.to_string();
            bytes.push('\n');
            stdout
                .write_all(bytes.as_bytes())
                .await
                .map_err(|error| format!("could not write stdio: {error}"))?;
            stdout
                .flush()
                .await
                .map_err(|error| format!("could not flush stdio: {error}"))?;
        }
    }
}
