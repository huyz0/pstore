//! Mutual TLS configuration for the MessagePack peer listener and client.

use crate::ConfigError;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::io::Read;
use std::sync::Arc;

const MAX_PEM: usize = 1_048_576;
pub(crate) const ALPN: &[u8] = b"h2";

/// Validated standalone peer listeners and their immutable deployment identity.
#[derive(Debug, Clone)]
pub struct PeerListenerConfig {
    /// Shared TCP/UDP listen address for the dedicated peer authority.
    pub bind: std::net::SocketAddr,
    /// Deployment CA, certificate and key, redacted in Debug output.
    pub tls: PeerTls,
    /// Transport used for outgoing peer connections.
    pub transport: crate::PeerTransport,
}

impl PeerListenerConfig {
    /// Validates configured peering before storage or sockets are opened.
    ///
    /// # Errors
    /// Missing bind address/material, invalid transport or malformed TLS files.
    pub fn from_vars(
        peering: bool,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ConfigError> {
        let configured = [
            "PSTORE_PEER_BIND",
            "PSTORE_PEER_CA_FILE",
            "PSTORE_PEER_CERT_FILE",
            "PSTORE_PEER_KEY_FILE",
            "PSTORE_PEER_TRANSPORT",
        ]
        .iter()
        .any(|key| get(key).is_some());
        if !peering && !configured {
            return Ok(None);
        }
        let bad_bind = || {
            ConfigError::Peers(
                "PSTORE_PEER_BIND",
                String::new(),
                "a dedicated peer TCP/UDP listen address is required",
            )
        };
        let bind = get("PSTORE_PEER_BIND")
            .ok_or_else(bad_bind)?
            .parse()
            .map_err(|_| bad_bind())?;
        let transport = match get("PSTORE_PEER_TRANSPORT").as_deref() {
            None | Some("auto") => crate::PeerTransport::Auto,
            Some("h2") => crate::PeerTransport::H2,
            Some("h3") => crate::PeerTransport::H3,
            _ => {
                return Err(ConfigError::Peers(
                    "PSTORE_PEER_TRANSPORT",
                    String::new(),
                    "auto, h2 or h3 required",
                ));
            }
        };
        let tls = PeerTls::from_vars(get)?;
        Ok(Some(Self {
            bind,
            tls,
            transport,
        }))
    }
}

/// Deployment peer CA and a certificate/key usable for both peer TLS roles.
///
/// This CA grants internal authority across tenants. Keep it separate from API
/// client identity. Debug output never contains PEM material or file contents.
#[derive(Clone, PartialEq, Eq)]
pub struct PeerTls(Arc<Material>);

#[derive(PartialEq, Eq)]
struct Material {
    ca: Vec<u8>,
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl std::fmt::Debug for PeerTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PeerTls([redacted])")
    }
}

fn refused() -> ConfigError {
    ConfigError::Peers(
        "PSTORE_PEER_TLS",
        String::new(),
        "valid bounded peer CA, certificate chain and matching private key are required",
    )
}

impl PeerTls {
    /// Validates local PEM material. Each input is bounded at 1 MiB.
    ///
    /// # Errors
    /// Empty/invalid roots, a malformed chain/key, mismatched identity or bounds.
    /// Trust, time, key usage and server SAN are verified during each handshake.
    pub fn from_pem(ca: &[u8], cert: &[u8], key: &[u8]) -> Result<Self, ConfigError> {
        if [ca, cert, key]
            .iter()
            .any(|p| p.is_empty() || p.len() > MAX_PEM)
        {
            return Err(refused());
        }
        let policy = Self(Arc::new(Material {
            ca: ca.to_vec(),
            cert: cert.to_vec(),
            key: key.to_vec(),
        }));
        policy.configs()?;
        Ok(policy)
    }

    /// Loads the three required peer TLS files through a bounded read.
    ///
    /// # Errors
    /// Missing/unreadable files or invalid material. Errors redact file contents.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let read = |name: &str| -> Result<Vec<u8>, ConfigError> {
            let path = get(name).ok_or_else(refused)?;
            let file = std::fs::File::open(path).map_err(|_| refused())?;
            let mut bytes = Vec::new();
            file.take((MAX_PEM + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| refused())?;
            if bytes.len() > MAX_PEM {
                return Err(refused());
            }
            Ok(bytes)
        };
        Self::from_pem(
            &read("PSTORE_PEER_CA_FILE")?,
            &read("PSTORE_PEER_CERT_FILE")?,
            &read("PSTORE_PEER_KEY_FILE")?,
        )
    }

    pub(crate) fn configs(&self) -> Result<(Arc<ServerConfig>, Arc<ClientConfig>), ConfigError> {
        self.configs_for(ALPN)
    }

    pub(crate) fn configs_for(
        &self,
        alpn: &[u8],
    ) -> Result<(Arc<ServerConfig>, Arc<ClientConfig>), ConfigError> {
        if alpn != b"h2" && alpn != b"h3" {
            return Err(refused());
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_reader_iter(self.0.ca.as_slice()) {
            roots
                .add(cert.map_err(|_| refused())?)
                .map_err(|_| refused())?;
        }
        if roots.is_empty() {
            return Err(refused());
        }
        let certs = CertificateDer::pem_reader_iter(self.0.cert.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| refused())?;
        if certs.is_empty() || certs.len() > 16 {
            return Err(refused());
        }
        let key = PrivateKeyDer::from_pem_reader(self.0.key.as_slice()).map_err(|_| refused())?;
        // WebPkiClientVerifier requires a client certificate. There is deliberately
        // no allow_unauthenticated path, even for an unsecured public HTTP API.
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots.clone()),
            provider.clone(),
        )
        .build()
        .map_err(|_| refused())?;
        let mut server = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| refused())?
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs.clone(), key.clone_key())
            .map_err(|_| refused())?;
        let mut client = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| refused())?
            .with_root_certificates(roots)
            .with_client_auth_cert(certs, key)
            .map_err(|_| refused())?;
        server.alpn_protocols = vec![alpn.to_vec()];
        server.max_early_data_size = 0;
        client.alpn_protocols = vec![alpn.to_vec()];
        client.enable_early_data = false;
        Ok((Arc::new(server), Arc::new(client)))
    }
}

#[cfg(test)]
mod tests;
