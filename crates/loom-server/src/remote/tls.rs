use super::*;

/// Server-side TLS material for the remote WebSocket listener.
///
/// Both PEM documents are needed to serve `wss://`: the certificate chain
/// presented to clients and the private key that matches its leaf.
#[derive(Clone)]
pub struct ServerTlsConfig {
    pub(super) certificate_pem: Vec<u8>,
    pub(super) private_key_pem: Vec<u8>,
}

impl fmt::Debug for ServerTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerTlsConfig")
            .field("certificate_pem_bytes", &self.certificate_pem.len())
            .field("private_key_pem", &"<redacted>")
            .finish()
    }
}

impl ServerTlsConfig {
    /// Builds TLS material from in-memory PEM documents.
    pub fn from_pem(
        certificate_pem: impl Into<Vec<u8>>,
        private_key_pem: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            certificate_pem: certificate_pem.into(),
            private_key_pem: private_key_pem.into(),
        }
    }

    /// Reads the certificate chain and private key from PEM files.
    pub fn from_pem_files(
        certificate: impl AsRef<Path>,
        private_key: impl AsRef<Path>,
    ) -> Result<Self> {
        let certificate = certificate.as_ref();
        let private_key = private_key.as_ref();
        let certificate_pem = fs::read(certificate).map_err(|error| {
            LoomError::invalid_request(format!(
                "could not read the TLS certificate file '{}': {error}",
                certificate.display()
            ))
        })?;
        let private_key_pem = fs::read(private_key).map_err(|error| {
            LoomError::invalid_request(format!(
                "could not read the TLS private key file '{}': {error}",
                private_key.display()
            ))
        })?;
        Ok(Self::from_pem(certificate_pem, private_key_pem))
    }

    /// Builds the rustls server configuration this listener serves with.
    pub(super) fn server_config(&self) -> Result<Arc<rustls::ServerConfig>> {
        let mut certificate_reader = io::BufReader::new(self.certificate_pem.as_slice());
        let certificates = rustls_pemfile::certs(&mut certificate_reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                LoomError::invalid_request(format!(
                    "could not parse the TLS certificate PEM: {error}"
                ))
            })?;
        if certificates.is_empty() {
            return Err(LoomError::invalid_request(
                "the TLS certificate PEM contains no certificates",
            ));
        }
        let mut key_reader = io::BufReader::new(self.private_key_pem.as_slice());
        let private_key = rustls_pemfile::private_key(&mut key_reader)
            .map_err(|error| {
                LoomError::invalid_request(format!(
                    "could not parse the TLS private key PEM: {error}"
                ))
            })?
            .ok_or_else(|| {
                LoomError::invalid_request("the TLS private key PEM contains no private key")
            })?;
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)
            .map_err(|error| {
                LoomError::invalid_request(format!(
                    "the TLS certificate and private key cannot be used together: {error}"
                ))
            })?;
        Ok(Arc::new(config))
    }
}

/// A listening socket that completes a TLS handshake with every accepted
/// connection, so `axum::serve` handles `wss://` and `/health` over TLS with the
/// same router as the plaintext listener.
pub(super) struct TlsListener {
    pub(super) listener: TcpListener,
    pub(super) acceptor: TlsAcceptor,
}

impl TlsListener {
    pub(super) fn new(listener: TcpListener, acceptor: TlsAcceptor) -> Self {
        Self { listener, acceptor }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    log::warn!("TLS listener could not accept a connection: {error}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            // A client that never finishes the handshake must not hold the
            // accept loop, so cap how long the handshake may take.
            match timeout(TLS_HANDSHAKE_TIMEOUT, self.acceptor.accept(stream)).await {
                Ok(Ok(stream)) => return (stream, address),
                Ok(Err(error)) => {
                    log::warn!("TLS handshake with {address} failed: {error}");
                }
                Err(_) => {
                    log::warn!("TLS handshake with {address} timed out");
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// Builds a rustls client configuration that trusts the OS roots and, in
/// addition, every certificate in `ca_pem`.
///
/// The supplied CA is added to the OS roots rather than replacing them, so
/// trusting a private CA never widens trust to unrelated certificates that the
/// OS already rejects.
pub(super) fn client_tls_config(ca_pem: &[u8]) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        log::warn!(
            "could not load every OS root certificate: {:?}",
            native.errors
        );
    }
    let (native_added, native_ignored) = roots.add_parsable_certificates(native.certs);
    log::debug!("trusting {native_added} OS root certificate(s); ignored {native_ignored}");
    let mut reader = io::BufReader::new(ca_pem);
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| {
            LoomError::invalid_request(format!("could not parse the TLS CA PEM: {error}"))
        })?;
    if certificates.is_empty() {
        return Err(LoomError::invalid_request(
            "the TLS CA PEM contains no certificates",
        ));
    }
    let (ca_added, _) = roots.add_parsable_certificates(certificates);
    if ca_added == 0 {
        return Err(LoomError::invalid_request(
            "the TLS CA PEM contains no usable certificates",
        ));
    }
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}
