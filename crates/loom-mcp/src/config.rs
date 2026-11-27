//! Resolved server configuration and the bind/auth policy.

use std::{
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use subtle::ConstantTimeEq;

/// A bearer secret that never appears in a `Debug` representation.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// TLS material `--tls-cert` and `--tls-key` amount to.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// The resolved configuration of one `loom-mcp` process.
#[derive(Clone)]
pub struct Config {
    /// Worker WebSocket URL, already resolved.
    pub connect: String,
    /// The operator bearer token read from the token file, used both to
    /// authenticate the MCP caller and to authenticate to the worker.
    pub token: Secret,
    /// PEM file whose certificates are trusted in addition to the OS roots for
    /// a `wss://` worker.
    pub ca_file: Option<PathBuf>,
    /// Plaintext non-loopback opt-in for the worker WebSocket connection.
    pub allow_insecure_remote: bool,
    /// Worker WebSocket frame limit; `None` is the transport default.
    pub max_frame_bytes: Option<usize>,
    /// The MCP listener bind address.
    pub bind: SocketAddr,
    /// TLS material for the MCP listener; both present or both absent.
    pub tls: Option<TlsConfig>,
    /// Expose only read tools; mutating tools are rejected.
    pub read_only: bool,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("connect", &self.connect)
            .field("token", &self.token)
            .field("ca_file", &self.ca_file)
            .field("allow_insecure_remote", &self.allow_insecure_remote)
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("bind", &self.bind)
            .field("tls", &self.tls)
            .field("read_only", &self.read_only)
            .finish()
    }
}

impl Config {
    /// Whether a presented bearer token matches the operator token, compared in
    /// constant time so the secret is not leaked through timing.
    pub fn authenticates(&self, presented: &str) -> bool {
        let expected = self.token.expose().as_bytes();
        let presented = presented.as_bytes();
        if expected.len() != presented.len() {
            return false;
        }
        bool::from(expected.ct_eq(presented))
    }
}

/// The state shared by every handler in one server.
pub struct AppState {
    pub config: Config,
}

/// Validates that a non-loopback bind is only served with TLS or an explicit
/// insecure-remote opt-in.
pub fn validate_bind(config: &Config) -> Result<(), String> {
    if config.bind.ip().is_loopback() {
        return Ok(());
    }
    if config.tls.is_some() {
        return Ok(());
    }
    if config.allow_insecure_remote {
        log::warn!(
            "binding the MCP listener to non-loopback address {} over plaintext HTTP because \
             --allow-insecure-remote was set; the operator token and every tool result are \
             unprotected in transit",
            config.bind
        );
        return Ok(());
    }
    Err(format!(
        "refusing to bind the MCP listener to non-loopback address {} without TLS; pass \
         --tls-cert and --tls-key, or --allow-insecure-remote to accept plaintext on a trusted \
         overlay",
        config.bind
    ))
}

/// Reads the TLS material and builds the rustls server configuration the MCP
/// listener serves with.
pub fn server_tls_config(tls: &TlsConfig) -> Result<std::sync::Arc<rustls::ServerConfig>, String> {
    use std::io::BufReader;

    let certificate_pem = std::fs::read(&tls.cert).map_err(|error| {
        format!(
            "could not read the TLS certificate file '{}': {error}",
            tls.cert.display()
        )
    })?;
    let private_key_pem = std::fs::read(&tls.key).map_err(|error| {
        format!(
            "could not read the TLS private key file '{}': {error}",
            tls.key.display()
        )
    })?;
    let mut certificate_reader = BufReader::new(certificate_pem.as_slice());
    let certificates = rustls_pemfile::certs(&mut certificate_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| format!("could not parse the TLS certificate PEM: {error}"))?;
    if certificates.is_empty() {
        return Err("the TLS certificate PEM contains no certificates".to_owned());
    }
    let mut key_reader = BufReader::new(private_key_pem.as_slice());
    let private_key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|error| format!("could not parse the TLS private key PEM: {error}"))?
        .ok_or_else(|| "the TLS private key PEM contains no private key".to_owned())?;
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|error| {
            format!("the TLS certificate and private key cannot be used together: {error}")
        })?;
    Ok(std::sync::Arc::new(server_config))
}

/// The token file path, read by the binary, is the only credential path; a
/// derived default is resolved with the same precedence `loom-cli` uses.
pub fn read_token(path: &Path, derived_for: Option<&str>) -> Result<String, String> {
    let token = std::fs::read_to_string(path).map_err(|error| match derived_for {
        Some(worker) => format!(
            "could not read the token file '{}' (default for {worker}): {error}",
            path.display()
        ),
        None => format!(
            "could not read the token file '{}': {error}",
            path.display()
        ),
    })?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(format!("the token file '{}' is empty", path.display()));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(bind: SocketAddr, tls: Option<TlsConfig>, allow_insecure_remote: bool) -> Config {
        Config {
            connect: "ws://127.0.0.1:1/ws".to_owned(),
            token: Secret::new("operator-secret".to_owned()),
            ca_file: None,
            allow_insecure_remote,
            max_frame_bytes: None,
            bind,
            tls,
            read_only: false,
        }
    }

    #[test]
    fn non_loopback_binds_require_tls_or_the_insecure_opt_in() {
        let loopback: SocketAddr = "127.0.0.1:8766".parse().unwrap();
        assert!(validate_bind(&config(loopback, None, false)).is_ok());
        assert!(validate_bind(&config(loopback, None, true)).is_ok());

        // A CGNAT address is the shape a Tailscale or WireGuard interface uses.
        let tailnet: SocketAddr = "100.64.0.1:8766".parse().unwrap();
        assert!(validate_bind(&config(tailnet, None, false)).is_err());
        assert!(
            validate_bind(&config(
                tailnet,
                Some(TlsConfig {
                    cert: PathBuf::from("cert.pem"),
                    key: PathBuf::from("key.pem"),
                }),
                false
            ))
            .is_ok()
        );
        assert!(validate_bind(&config(tailnet, None, true)).is_ok());

        // The unspecified address is never a silent loopback.
        let wildcard: SocketAddr = "0.0.0.0:8766".parse().unwrap();
        assert!(validate_bind(&config(wildcard, None, false)).is_err());
        assert!(validate_bind(&config(wildcard, None, true)).is_ok());
    }

    #[test]
    fn the_token_is_compared_and_never_debug_printed() {
        let config = config("127.0.0.1:8766".parse().unwrap(), None, false);
        assert!(config.authenticates("operator-secret"));
        assert!(!config.authenticates("operator-secre"));
        assert!(!config.authenticates("operator-secret-longer"));
        assert!(!config.authenticates(""));
        let debug = format!("{:?}", config);
        assert!(!debug.contains("operator-secret"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }
}
