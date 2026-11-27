//! `loom-mcp`: an MCP server that mirrors `loom-cli` over streamable HTTP or
//! stdio. It is a client of the existing worker WebSocket protocol, not a
//! second backend.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use loom_cli::cli::{resolve_connect_from_env, resolve_token_file_from_env};
use loom_mcp::{AppState, Config, Secret, TlsConfig, config::read_token};

/// The bind address used when `--bind` is not supplied.
const DEFAULT_BIND: &str = "127.0.0.1:8766";
/// `RUST_LOG` overrides this.
const DEFAULT_LOG_FILTER: &str = "warn,loom_mcp=info";

fn main() -> std::process::ExitCode {
    init_logging();
    match run(std::env::args().skip(1)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            log::error!("loom-mcp: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn init_logging() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(DEFAULT_LOG_FILTER))
        .format_timestamp_millis()
        .init();
}

#[derive(Clone, Debug)]
struct Options {
    connect: Option<String>,
    token_file: Option<PathBuf>,
    ca_file: Option<PathBuf>,
    allow_insecure_remote: bool,
    max_frame_bytes: Option<usize>,
    bind: SocketAddr,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    read_only: bool,
    stdio: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            connect: None,
            token_file: None,
            ca_file: None,
            allow_insecure_remote: false,
            max_frame_bytes: None,
            bind: DEFAULT_BIND.parse().expect("valid default bind address"),
            tls_cert: None,
            tls_key: None,
            read_only: false,
            stdio: false,
        }
    }
}

enum Parsed {
    Help,
    Version,
    Serve(Options),
}

fn parse<I>(arguments: I) -> Result<Parsed, String>
where
    I: IntoIterator<Item = String>,
{
    let mut options = Options::default();
    let mut args = arguments.into_iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--connect" => {
                options.connect = Some(required_value(&mut args, "--connect")?);
            }
            "--token-file" => {
                options.token_file =
                    Some(PathBuf::from(required_value(&mut args, "--token-file")?));
            }
            "--ca-file" => {
                options.ca_file = Some(PathBuf::from(required_value(&mut args, "--ca-file")?));
            }
            "--max-frame-bytes" => {
                options.max_frame_bytes = Some(parse_max_frame_bytes(&required_value(
                    &mut args,
                    "--max-frame-bytes",
                )?)?);
            }
            "--bind" => {
                options.bind = parse_bind(required_value(&mut args, "--bind")?)?;
            }
            "--tls-cert" => {
                options.tls_cert = Some(PathBuf::from(required_value(&mut args, "--tls-cert")?));
            }
            "--tls-key" => {
                options.tls_key = Some(PathBuf::from(required_value(&mut args, "--tls-key")?));
            }
            "--allow-insecure-remote" => options.allow_insecure_remote = true,
            "--read-only" => options.read_only = true,
            "--stdio" => options.stdio = true,
            "--help" | "-h" => return Ok(Parsed::Help),
            "--version" | "-V" => return Ok(Parsed::Version),
            value if value.starts_with("--connect=") => {
                options.connect = Some(value["--connect=".len()..].to_owned());
            }
            value if value.starts_with("--token-file=") => {
                options.token_file = Some(PathBuf::from(&value["--token-file=".len()..]));
            }
            value if value.starts_with("--ca-file=") => {
                options.ca_file = Some(PathBuf::from(&value["--ca-file=".len()..]));
            }
            value if value.starts_with("--max-frame-bytes=") => {
                options.max_frame_bytes =
                    Some(parse_max_frame_bytes(&value["--max-frame-bytes=".len()..])?);
            }
            value if value.starts_with("--bind=") => {
                options.bind = parse_bind(value["--bind=".len()..].to_owned())?;
            }
            value if value.starts_with("--tls-cert=") => {
                options.tls_cert = Some(PathBuf::from(&value["--tls-cert=".len()..]));
            }
            value if value.starts_with("--tls-key=") => {
                options.tls_key = Some(PathBuf::from(&value["--tls-key=".len()..]));
            }
            unknown => {
                return Err(format!(
                    "unknown argument '{unknown}'; use --help for usage"
                ));
            }
        }
    }
    Ok(Parsed::Serve(options))
}

fn required_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_bind(value: String) -> Result<SocketAddr, String> {
    value
        .parse()
        .map_err(|error| format!("--bind must be a socket address: {error}"))
}

fn parse_max_frame_bytes(value: &str) -> Result<usize, String> {
    let bytes = value
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("--max-frame-bytes must be a whole number of bytes, got '{value}'"))?;
    if bytes == 0 {
        return Err("--max-frame-bytes must be greater than zero".to_owned());
    }
    Ok(bytes)
}

fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), String> {
    let options = match parse(arguments) {
        Ok(Parsed::Help) => {
            print!("{}", help_text());
            return Ok(());
        }
        Ok(Parsed::Version) => {
            println!("loom-mcp {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Ok(Parsed::Serve(options)) => options,
        Err(message) => return Err(message),
    };

    let connect = resolve_connect_from_env(options.connect);
    let token_file = resolve_token_file_from_env(options.token_file, &connect)?;
    let token = read_token(&token_file.path, token_file.derived_for.as_deref())?;
    let tls = match (options.tls_cert, options.tls_key) {
        (Some(cert), Some(key)) => Some(TlsConfig { cert, key }),
        (None, None) => None,
        (Some(_), None) => {
            return Err(
                "--tls-cert requires the matching --tls-key; supply both or neither".to_owned(),
            );
        }
        (None, Some(_)) => {
            return Err(
                "--tls-key requires the matching --tls-cert; supply both or neither".to_owned(),
            );
        }
    };
    let read_only = options.read_only;
    let tls_configured = tls.is_some();
    let config = Config {
        connect: connect.clone(),
        token: Secret::new(token),
        ca_file: options.ca_file,
        allow_insecure_remote: options.allow_insecure_remote,
        max_frame_bytes: options.max_frame_bytes,
        bind: options.bind,
        tls,
        read_only,
    };

    if options.stdio {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("could not start the async runtime: {error}"))?;
        let state = Arc::new(AppState { config });
        log::info!(
            "loom-mcp serving stdio for worker {}{}",
            connect,
            if read_only { " in read-only mode" } else { "" }
        );
        return runtime.block_on(loom_mcp::stdio::serve(state));
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the async runtime: {error}"))?;
    runtime.block_on(async move {
        let running = loom_mcp::http::serve(config).await?;
        log::info!(
            "loom-mcp listening on {}{} for worker {}{}",
            running.local_addr,
            if tls_configured {
                " (https)"
            } else {
                " (http)"
            },
            connect,
            if read_only { " in read-only mode" } else { "" },
        );
        tokio::signal::ctrl_c().await.ok();
        running.task.abort();
        Ok(())
    })
}

fn help_text() -> String {
    "Usage: loom-mcp [--connect <url>] [--token-file <path>] [--ca-file <pem>] \
     [--max-frame-bytes <bytes>] [--bind <addr>] [--tls-cert <pem> --tls-key <pem>] \
     [--allow-insecure-remote] [--read-only] [--stdio]\n\
     \n\
     An MCP server that mirrors loom-cli over streamable HTTP (the default) or\n\
     stdio. It is a client of the existing worker WebSocket protocol, not a\n\
     second backend.\n\
     \n\
     Options:\n\
     \x20 --connect <url>         worker WebSocket URL (fallback LOOM_URL, then\n\
     \x20                        ws://127.0.0.1:8765/ws)\n\
     \x20 --token-file <path>     file holding the operator bearer token (fallback\n\
     \x20                        LOOM_TOKEN_FILE, then the path derived from the\n\
     \x20                        connected URL); --token is deliberately not offered\n\
     \x20 --ca-file <pem>         also trust this PEM certificate for a wss:// worker\n\
     \x20 --allow-insecure-remote accept plaintext ws:// to a non-loopback worker, and\n\
     \x20                        a plaintext non-loopback MCP bind\n\
     \x20 --max-frame-bytes <n>   worker WebSocket frame size limit\n\
     \x20 --bind <addr>           MCP listen address (default 127.0.0.1:8766)\n\
     \x20 --tls-cert <pem>        serve the MCP listener over HTTPS with this chain\n\
     \x20 --tls-key <pem>         PEM private key matching --tls-cert\n\
     \x20 --read-only             expose only the read tools (ls, status, models,\n\
     \x20                        providers, and the log resources)\n\
     \x20 --stdio                 serve newline-delimited JSON-RPC on stdio instead of\n\
     \x20                        HTTP\n\
     \x20 -h, --help              print this help and exit\n\
     \x20 -V, --version           print the version and exit\n\
     \n\
     A non-loopback --bind requires --tls-cert and --tls-key, or an explicit\n\
     --allow-insecure-remote opt-in (which logs a warning). The operator token\n\
     is never printed, logged, or returned."
        .to_owned()
}
