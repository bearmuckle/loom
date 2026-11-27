//! Stdio transport: newline-delimited JSON-RPC over stdin/stdout.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::config::AppState;
use crate::jsonrpc;

/// Serves MCP over stdin/stdout until the input closes. Stdio trusts the local
/// caller: the bearer-token check belongs to the HTTP listener, because a local
/// process that can launch this binary can already read the token file.
pub async fn serve(state: Arc<AppState>) -> Result<(), String> {
    let reader = BufReader::new(tokio::io::stdin());
    let writer = tokio::io::stdout();
    serve_io(state, reader, writer).await
}

async fn serve_io<R, W>(state: Arc<AppState>, mut reader: R, mut writer: W) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
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
            writer
                .write_all(bytes.as_bytes())
                .await
                .map_err(|error| format!("could not write stdio: {error}"))?;
            writer
                .flush()
                .await
                .map_err(|error| format!("could not flush stdio: {error}"))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, HttpLimits, Secret};
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::{AsyncWriteExt, BufReader};

    fn state() -> Arc<AppState> {
        Arc::new(AppState {
            config: Config {
                connect: "ws://127.0.0.1:1/ws".to_owned(),
                token: Secret::new("token".to_owned()).unwrap(),
                ca_file: None,
                allow_insecure_remote: false,
                max_frame_bytes: None,
                bind: "127.0.0.1:8766".parse().unwrap(),
                tls: None,
                read_only: false,
                allow_mutating: false,
                allowed_hosts: Vec::new(),
            },
            limits: HttpLimits::default(),
        })
    }

    #[tokio::test]
    async fn stdio_answers_valid_lines_and_skips_garbage() {
        let (client, server) = tokio::io::duplex(4096);
        let (client_read, mut client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);
        let mut client_reader = BufReader::new(client_read);
        let task = tokio::spawn(serve_io(state(), BufReader::new(server_read), server_write));

        let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
        client_write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), client_reader.read_line(&mut line))
            .await
            .expect("stdio response arrived")
            .expect("readable response line");
        let response: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"], json!({}));

        client_write.write_all(b"{not json}\n").await.unwrap();
        client_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        line.clear();
        tokio::time::timeout(Duration::from_secs(5), client_reader.read_line(&mut line))
            .await
            .expect("second response arrived")
            .expect("readable second line");
        let response: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(response["id"], 2);
        assert_eq!(response["result"], json!({}));

        client_write.shutdown().await.unwrap();
        drop(client_write);
        drop(client_reader);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("stdio task exits after EOF")
            .expect("stdio task joined")
            .expect("stdio task exited cleanly");
    }
}
