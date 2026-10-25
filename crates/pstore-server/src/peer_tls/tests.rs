//! Both TLS directions must authenticate before exchanging application data.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]
use super::PeerTls;
use crate::tls_for_tests as fixtures;
use fixtures::{Authority, Material};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn policy(m: &Material) -> PeerTls {
    PeerTls::from_pem(m.ca.as_bytes(), m.cert.as_bytes(), m.key.as_bytes()).unwrap()
}

async fn exchange(
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
    name: &'static str,
) -> bool {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = async move {
        let (stream, _) = listener.accept().await.map_err(|_| ())?;
        let mut stream = tokio_rustls::TlsAcceptor::from(server)
            .accept(stream)
            .await
            .map_err(|_| ())?;
        let mut ping = [0];
        stream.read_exact(&mut ping).await.map_err(|_| ())?;
        if ping != [7] {
            return Err(());
        }
        stream.write_all(&[8]).await.map_err(|_| ())?;
        Ok::<_, ()>(())
    };
    let client = async move {
        let stream = tokio::net::TcpStream::connect(address)
            .await
            .map_err(|_| ())?;
        let mut stream = tokio_rustls::TlsConnector::from(client)
            .connect(
                rustls::pki_types::ServerName::try_from(name).map_err(|_| ())?,
                stream,
            )
            .await
            .map_err(|_| ())?;
        stream.write_all(&[7]).await.map_err(|_| ())?;
        let mut pong = [0];
        stream.read_exact(&mut pong).await.map_err(|_| ())?;
        if pong != [8] {
            return Err(());
        }
        Ok::<_, ()>(())
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(server, client)
    })
    .await;
    matches!(result, Ok((Ok(()), Ok(()))))
}

#[tokio::test]
async fn both_directions_require_trusted_current_certificates_and_server_san() {
    let ca = Authority::new();
    let server = policy(&ca.issue(&["localhost"], false));
    let client = policy(&ca.issue(&["client"], false));
    let (server_config, _) = server.configs().unwrap();
    let (_, client_config) = client.configs().unwrap();
    assert!(exchange(server_config.clone(), client_config.clone(), "localhost").await);
    let (expired_server, _) = policy(&ca.issue(&["localhost"], true)).configs().unwrap();
    assert!(!exchange(expired_server, client_config.clone(), "localhost").await);
    assert!(!exchange(server_config.clone(), client_config, "wrong.example").await);
    let stranger = policy(&Authority::new().issue(&["stranger"], false));
    let (_, untrusted_server_ca) = stranger.configs().unwrap();
    assert!(!exchange(server_config.clone(), untrusted_server_ca, "localhost").await);
    let alien = Authority::new().issue(&["alien"], false);
    let forged = PeerTls::from_pem(
        ca.pem.as_bytes(),
        alien.cert.as_bytes(),
        alien.key.as_bytes(),
    )
    .unwrap();
    let (_, untrusted_client) = forged.configs().unwrap();
    assert!(!exchange(server_config.clone(), untrusted_client, "localhost").await);
    let expired = policy(&ca.issue(&["client"], true));
    let (_, expired_client) = expired.configs().unwrap();
    assert!(!exchange(server_config.clone(), expired_client, "localhost").await);
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_reader_iter(ca.pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut anonymous = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    anonymous.alpn_protocols = vec![b"h2".to_vec()];
    assert!(!exchange(server_config, Arc::new(anonymous), "localhost").await);
}

#[test]
fn tls_material_is_bounded_validated_and_redacted() {
    let material = Authority::new().issue(&["localhost"], false);
    let good = policy(&material);
    assert!(!format!("{good:?}").contains("PRIVATE KEY"));
    let oversized = vec![b'a'; 1_048_577];
    for (ca, cert, key) in [
        (
            &b"invalid"[..],
            material.cert.as_bytes(),
            material.key.as_bytes(),
        ),
        (
            material.ca.as_bytes(),
            &b"invalid"[..],
            material.key.as_bytes(),
        ),
        (
            material.ca.as_bytes(),
            material.cert.as_bytes(),
            &b"invalid"[..],
        ),
        (
            material.ca.as_bytes(),
            material.cert.as_bytes(),
            oversized.as_slice(),
        ),
    ] {
        let err = PeerTls::from_pem(ca, cert, key).unwrap_err();
        assert!(!format!("{err:?}").contains("PRIVATE KEY"));
    }
}

struct PemFiles(std::collections::BTreeMap<&'static str, std::path::PathBuf>);
impl PemFiles {
    fn new(material: &Material) -> Self {
        use std::io::Write;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut paths = std::collections::BTreeMap::new();
        for (name, bytes) in [
            ("PSTORE_PEER_CA_FILE", material.ca.as_bytes()),
            ("PSTORE_PEER_CERT_FILE", material.cert.as_bytes()),
            ("PSTORE_PEER_KEY_FILE", material.key.as_bytes()),
        ] {
            let (path, mut file) = (0..100)
                .find_map(|_| {
                    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let path = std::env::temp_dir().join(format!(
                        "pstore-peer-PRIVATE-KEY-{}-{seq}.pem",
                        std::process::id()
                    ));
                    match std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                    {
                        Ok(file) => Some((path, file)),
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                        Err(error) => panic!("cannot create PEM fixture: {error}"),
                    }
                })
                .expect("a unique PEM fixture path");
            file.write_all(bytes).unwrap();
            paths.insert(name, path);
        }
        Self(paths)
    }
    fn vars(&self) -> std::collections::BTreeMap<&'static str, String> {
        self.0
            .iter()
            .map(|(name, path)| (*name, path.to_str().unwrap().to_owned()))
            .collect()
    }
    fn path(&self, name: &str) -> &std::path::Path {
        self.0.get(name).unwrap()
    }
}
impl Drop for PemFiles {
    fn drop(&mut self) {
        for path in self.0.values() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[test]
fn listener_configuration_requires_complete_explicit_material() {
    use super::PeerListenerConfig;
    use crate::PeerTransport;
    assert!(
        PeerListenerConfig::from_vars(false, |_| None)
            .unwrap()
            .is_none()
    );
    assert!(PeerListenerConfig::from_vars(true, |_| None).is_err());
    assert!(
        PeerListenerConfig::from_vars(false, |key| (key == "PSTORE_PEER_TRANSPORT")
            .then(|| "h2".to_owned()))
        .is_err()
    );
    let material = Authority::new().issue(&["localhost"], false);
    let files = PemFiles::new(&material);
    let mut vars = files.vars();
    vars.insert("PSTORE_PEER_BIND", "127.0.0.1:9443".to_owned());
    let config = PeerListenerConfig::from_vars(false, |key| vars.get(key).cloned())
        .unwrap()
        .unwrap();
    assert_eq!(config.bind.to_string(), "127.0.0.1:9443");
    assert_eq!(config.transport, PeerTransport::Auto);
    for (value, expected) in [
        ("auto", PeerTransport::Auto),
        ("h2", PeerTransport::H2),
        ("h3", PeerTransport::H3),
    ] {
        vars.insert("PSTORE_PEER_TRANSPORT", value.to_owned());
        let config = PeerListenerConfig::from_vars(true, |key| vars.get(key).cloned())
            .unwrap()
            .unwrap();
        assert_eq!(config.transport, expected);
    }
    for (key, value) in [
        ("PSTORE_PEER_BIND", "not-an-address"),
        ("PSTORE_PEER_TRANSPORT", "h4"),
    ] {
        let mut invalid = vars.clone();
        invalid.insert(key, value.to_owned());
        let error =
            PeerListenerConfig::from_vars(true, |name| invalid.get(name).cloned()).unwrap_err();
        assert!(!format!("{error:?}").contains("PRIVATE-KEY"));
    }
    for missing in [
        "PSTORE_PEER_CA_FILE",
        "PSTORE_PEER_CERT_FILE",
        "PSTORE_PEER_KEY_FILE",
    ] {
        let mut invalid = vars.clone();
        invalid.remove(missing);
        assert!(PeerListenerConfig::from_vars(true, |key| invalid.get(key).cloned()).is_err());
    }
}

#[test]
fn file_loading_enforces_exact_byte_bounds_and_redacts_io_failures() {
    let material = Authority::new().issue(&["localhost"], false);
    let files = PemFiles::new(&material);
    let vars = files.vars();
    assert!(PeerTls::from_vars(|key| vars.get(key).cloned()).is_ok());
    for (name, original) in [
        ("PSTORE_PEER_CA_FILE", material.ca.as_bytes()),
        ("PSTORE_PEER_CERT_FILE", material.cert.as_bytes()),
        ("PSTORE_PEER_KEY_FILE", material.key.as_bytes()),
    ] {
        let mut padded = original.to_vec();
        padded.resize(1_048_576, b' ');
        std::fs::write(files.path(name), &padded).unwrap();
        assert!(
            PeerTls::from_vars(|key| vars.get(key).cloned()).is_ok(),
            "last valid byte boundary: {name}"
        );
        padded.push(b' ');
        std::fs::write(files.path(name), &padded).unwrap();
        let error = PeerTls::from_vars(|key| vars.get(key).cloned()).unwrap_err();
        assert!(!format!("{error:?}").contains("PRIVATE-KEY"));
        std::fs::write(files.path(name), original).unwrap();
        for path in [
            files.path(name).with_extension("missing"),
            std::env::temp_dir(),
        ] {
            let mut invalid = vars.clone();
            invalid.insert(name, path.to_str().unwrap().to_owned());
            let error = PeerTls::from_vars(|key| invalid.get(key).cloned()).unwrap_err();
            assert!(!format!("{error:?}").contains("PRIVATE-KEY"));
        }
    }
}

#[test]
fn pem_identity_rejects_mismatched_keys_invalid_der_and_long_chains() {
    let ca = Authority::new();
    let material = ca.issue(&["localhost"], false);
    let other = ca.issue(&["localhost"], false);
    assert!(
        PeerTls::from_pem(
            material.ca.as_bytes(),
            material.cert.as_bytes(),
            other.key.as_bytes()
        )
        .is_err()
    );
    let invalid_der = b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n";
    assert!(
        PeerTls::from_pem(
            invalid_der,
            material.cert.as_bytes(),
            material.key.as_bytes()
        )
        .is_err()
    );
    assert!(
        PeerTls::from_pem(material.ca.as_bytes(), invalid_der, material.key.as_bytes()).is_err()
    );
    let chain = material.cert.repeat(16);
    let bounded = PeerTls::from_pem(
        material.ca.as_bytes(),
        chain.as_bytes(),
        material.key.as_bytes(),
    )
    .unwrap();
    assert!(bounded.configs_for(b"h2").is_ok());
    assert!(bounded.configs_for(b"h3").is_ok());
    assert!(bounded.configs_for(b"h1").is_err());
    let too_long = material.cert.repeat(17);
    assert!(
        PeerTls::from_pem(
            material.ca.as_bytes(),
            too_long.as_bytes(),
            material.key.as_bytes()
        )
        .is_err()
    );
}

#[test]
fn malformed_pem_cannot_be_hidden_by_valid_trust_or_identity_material() {
    let ca = Authority::new();
    let material = ca.issue(&["localhost"], false);
    assert!(policy(&material).configs_for(b"h2").is_ok());
    assert!(policy(&material).configs_for(b"h3").is_ok());
    let malformed = "-----BEGIN CERTIFICATE-----\n@@@@\n-----END CERTIFICATE-----\n";
    for (is_root, valid) in [(true, &material.ca), (false, &material.cert)] {
        for mixed in [format!("{malformed}{valid}"), format!("{valid}{malformed}")] {
            let (roots, chain) = if is_root {
                (mixed.as_bytes(), material.cert.as_bytes())
            } else {
                (material.ca.as_bytes(), mixed.as_bytes())
            };
            let error = PeerTls::from_pem(roots, chain, material.key.as_bytes())
                .expect_err("a valid neighbor must not hide a certificate parse failure");
            let redacted = format!("{error:?}");
            assert!(!redacted.contains("@@@@"));
            assert!(!redacted.contains(&material.key));
        }
    }
}
