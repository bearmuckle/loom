//! The worker runner: one verb, one fresh WebSocket connection.

use loom_cli::cli::{self, Execution, Failure};

use crate::config::Config;

/// Runs one verb against the worker described by `config`, opening a fresh
/// connection so every tool call negotiates exactly like one `loom-cli`
/// invocation.
///
/// The `loom-cli` `Client` seam boxes its request futures without a `Send`
/// bound (its in-process test doubles capture `Rc`), so the verb cannot run
/// directly on the HTTP server's worker threads. It is run on a dedicated
/// blocking thread with its own current-thread runtime, and only the already
/// rendered [`Execution`] crosses back, which is `Send`.
pub async fn run_verb(config: &Config, arguments: &[String]) -> Execution {
    let config = config.clone();
    let arguments = arguments.to_vec();
    match tokio::task::spawn_blocking(move || run_blocking(&config, &arguments)).await {
        Ok(execution) => execution,
        Err(error) => Execution {
            verb: String::new(),
            exit_code: 1,
            result: Err(Failure::Internal(format!(
                "the worker task panicked: {error}"
            ))),
            stdout: String::new(),
            stderr: String::new(),
        },
    }
}

fn run_blocking(config: &Config, arguments: &[String]) -> Execution {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return Execution {
                verb: String::new(),
                exit_code: 1,
                result: Err(Failure::Internal(format!(
                    "could not start the worker runtime: {error}"
                ))),
                stdout: String::new(),
                stderr: String::new(),
            };
        }
    };
    runtime.block_on(cli::run_remote(
        arguments,
        &config.connect,
        config.token.expose(),
        config.allow_insecure_remote,
        config.max_frame_bytes,
        config.ca_file.as_deref(),
    ))
}
