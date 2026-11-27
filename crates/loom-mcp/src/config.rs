//! Resolved server configuration and the bind/auth policy.

use std::{
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

/// The default cap on JSON-RPC requests processed at once.
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 64;
/// The default wall-clock bound for one JSON-RPC request.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The default largest accepted JSON-RPC batch.
pub const DEFAULT_MAX_BATCH_REQUESTS: usize = 32;

/// A bearer secret that never appears in a `Debug` representation.
///
/// The raw value is kept so it can be presented to the worker, while
/// authentication against a caller compares only SHA-256 digests, exactly like
/// the worker's own token store.
#[derive(Clone)]
pub struct Secret {
    value: String,
    digest: [u8; 32],
}

impl Secret {
    pub fn new(value: String) -> Result<Self, String> {
        if value.trim().is_empty() {
            return Err("the operator token must not be empty".to_owned());
        }
        let digest: [u8; 32] = Sha256::digest(value.as_bytes()).into();
        Ok(Self { value, digest })
    }

    pub fn expose(&self) -> &str {
        &self.value
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
    /// Expose only read tools; mutating tools are rejected. This is the
    /// default unless `--allow-mutating-tools` is given.
    pub read_only: bool,
    /// Opt in to exposing the mutating tools.
    pub allow_mutating: bool,
    /// Extra hostnames (without a port) accepted in `Host` and `Origin`
    /// headers, in addition to the bind address and loopback aliases.
    pub allowed_hosts: Vec<String>,
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
            .field("allow_mutating", &self.allow_mutating)
            .field("allowed_hosts", &self.allowed_hosts)
            .finish()
    }
}

impl Config {
    /// Whether a presented bearer token matches the operator token, compared as
    /// SHA-256 digests in constant time so neither the token nor its length is
    /// leaked through timing.
    pub fn authenticates(&self, presented: &str) -> bool {
        let presented: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        bool::from(self.token.digest.ct_eq(&presented))
    }
}

/// Per-transport resource limits shared by every HTTP handler.
#[derive(Clone)]
pub struct HttpLimits {
    /// The semaphore capping concurrent JSON-RPC requests.
    pub concurrent: Arc<Semaphore>,
    /// The wall-clock bound for one JSON-RPC request.
    pub request_timeout: Duration,
    /// The largest accepted JSON-RPC batch.
    pub max_batch_requests: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            concurrent: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_REQUESTS)),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_batch_requests: DEFAULT_MAX_BATCH_REQUESTS,
        }
    }
}

/// The state shared by every handler in one server.
pub struct AppState {
    pub config: Config,
    pub limits: HttpLimits,
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

/// The hostnames the HTTP transport accepts in `Host` and `Origin` headers.
///
/// The bind address and, for a loopback bind, its localhost aliases are always
/// allowed; `--allowed-host` adds the hostnames a TLS reverse proxy or overlay
/// presents. Every value is normalized to lower case without brackets or a
/// trailing dot.
pub fn allowed_hosts(config: &Config) -> Vec<String> {
    let mut hosts = config
        .allowed_hosts
        .iter()
        .map(|host| normalize_host(host))
        .collect::<Vec<_>>();
    let ip = config.bind.ip();
    if ip.is_loopback() {
        hosts.extend([
            "127.0.0.1".to_owned(),
            "localhost".to_owned(),
            "::1".to_owned(),
        ]);
    } else if !ip.is_unspecified() {
        hosts.push(ip.to_string());
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Normalizes a `Host`/`Origin` host for comparison: lower case, no IPv6
/// brackets, and no trailing dot.
pub fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase()
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
            token: Secret::new("operator-secret".to_owned()).unwrap(),
            ca_file: None,
            allow_insecure_remote,
            max_frame_bytes: None,
            bind,
            tls,
            read_only: false,
            allow_mutating: false,
            allowed_hosts: Vec::new(),
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

    #[test]
    fn an_empty_or_blank_token_is_rejected_at_construction() {
        assert!(Secret::new(String::new()).is_err());
        assert!(Secret::new("  \n".to_owned()).is_err());
        assert!(Secret::new("operator-secret".to_owned()).is_ok());
    }

    #[test]
    fn allowed_hosts_derive_from_the_bind_and_explicit_hosts() {
        let loopback: SocketAddr = "127.0.0.1:8766".parse().unwrap();
        let mut loopback_config = config(loopback, None, false);
        loopback_config.allowed_hosts = vec!["Example.COM.".to_owned(), "[::1]".to_owned()];
        let hosts = allowed_hosts(&loopback_config);
        assert!(hosts.contains(&"127.0.0.1".to_owned()));
        assert!(hosts.contains(&"localhost".to_owned()));
        assert!(hosts.contains(&"::1".to_owned()));
        assert!(hosts.contains(&"example.com".to_owned()));

        let tailnet: SocketAddr = "100.64.0.1:8766".parse().unwrap();
        let tailnet = config(tailnet, None, true);
        assert_eq!(allowed_hosts(&tailnet), vec!["100.64.0.1".to_owned()]);
    }
}
