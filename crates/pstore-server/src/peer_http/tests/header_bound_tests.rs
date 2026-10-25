//! Included under qualification after its initial queued run finishes.
use super::*;

#[tokio::test]
async fn http3_rejects_encoded_headers_before_waiting_for_an_oversized_payload() {
    let (address, tls, calls, task) = server().await;
    let raw = connect(address, &tls).await;
    let (mut send, mut receive) = raw.connection.open_bi().await.unwrap();
    let mut declaration = integer(1);
    declaration.extend(integer(64 * 1024 * 1024));
    declaration.extend([0, 0]);
    send.write_all(&declaration).await.unwrap();
    let refused=tokio::time::timeout(Duration::from_secs(1),receive.read_to_end(65536)).await
        .expect("oversized HEADERS must be refused from the length prefix, without waiting for payload or RPC expiry");
    assert!(match refused {
        Err(_) => true,
        Ok(bytes) => bytes.is_empty(),
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn http3_client_refuses_oversized_encoded_response_headers_before_payload() {
    let ca = crate::tls_for_tests::Authority::new();
    let material = ca.issue(&["localhost"], false);
    let tls = PeerTls::from_pem(
        material.ca.as_bytes(),
        material.cert.as_bytes(),
        material.key.as_bytes(),
    )
    .unwrap();
    let (config, _) = tls.configs_for(b"h3").unwrap();
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(config).unwrap();
    let endpoint = quinn::Endpoint::server(
        quinn::ServerConfig::with_crypto(Arc::new(crypto)),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let address = endpoint.local_addr().unwrap();
    let fake = tokio::spawn(async move {
        let connection = endpoint.accept().await.unwrap().await.unwrap();
        let mut control = connection.open_uni().await.unwrap();
        control.write_all(&[0, 4, 0]).await.unwrap();
        let (mut send, _receive) = connection.accept_bi().await.unwrap();
        let mut declaration = integer(1);
        declaration.extend(integer(64 * 1024 * 1024));
        declaration.extend([0, 0]);
        send.write_all(&declaration).await.unwrap();
        std::future::pending::<()>().await;
    });
    let client = super::super::Client::connect(
        address,
        "localhost",
        &tls,
        super::super::Protocol::H3,
        Arc::new(tokio::sync::Semaphore::new(peer_wire::WIRE_BUDGET)),
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        client.call(peer_wire::Request {
            version: peer_wire::VERSION,
            id: 1,
            tenant: "9".to_owned(),
            body: json!({}),
        }),
    )
    .await
    .expect("client must refuse the response length before waiting for payload");
    assert!(result.is_err());
    fake.abort();
}
