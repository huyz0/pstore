#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "protocol fixture failures report through assertions"
)]
use super::*;
use futures_util::future::poll_fn;
use h3::quic::{
    BidiStream as _, Connection as _, OpenStreams as _, RecvStream as _, SendStream as _,
};
use std::sync::Arc;

async fn pair() -> (
    quinn::Endpoint,
    quinn::Endpoint,
    quinn::Connection,
    quinn::Connection,
) {
    let ca = crate::tls_for_tests::Authority::new();
    let material = ca.issue(&["localhost"], false);
    let tls = crate::PeerTls::from_pem(
        material.ca.as_bytes(),
        material.cert.as_bytes(),
        material.key.as_bytes(),
    )
    .unwrap();
    let (server, client) = tls.configs_for(b"h3").unwrap();
    let server = quinn::Endpoint::server(
        quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server).unwrap(),
        )),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let mut client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client_endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client).unwrap(),
    )));
    let connecting = client_endpoint
        .connect(server.local_addr().unwrap(), "localhost")
        .unwrap();
    let (remote, local) = tokio::join!(
        async { server.accept().await.unwrap().await.unwrap() },
        async { connecting.await.unwrap() }
    );
    (server, client_endpoint, remote, local)
}

#[tokio::test]
async fn splitting_a_stream_preserves_partial_frame_bounds_and_stream_identity() {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let (_server, _client, remote, local) = pair().await;
        let mut guarded = Connection::new(remote);
        let (mut send, _receive) = local.open_bi().await.unwrap();
        send.write_all(&[1, 0x80]).await.unwrap();
        let mut incoming = poll_fn(|cx| guarded.poll_accept_bidi(cx)).await.unwrap();
        let id = incoming.recv_id();
        assert_eq!(id, incoming.send_id());
        let mut read = 0;
        while read < 2 {
            read += poll_fn(|cx| incoming.poll_data(cx)).await.unwrap().unwrap().len();
        }
        let (mut reply, mut receive) = incoming.split();
        assert_eq!(reply.send_id(), id);
        assert_eq!(receive.recv_id(), id);
        send.write_all(&[0, 0x80, 1]).await.unwrap();
        assert!(matches!(poll_fn(|cx| receive.poll_data(cx)).await, Err(StreamErrorIncoming::StreamTerminated { error_code }) if error_code == h3::error::Code::H3_EXCESSIVE_LOAD.value()));
        reply.reset(h3::error::Code::H3_REQUEST_CANCELLED.value());
        assert!(matches!(send.stopped().await.unwrap(), Some(code) if code.into_inner() == h3::error::Code::H3_EXCESSIVE_LOAD.value()));
    }).await.unwrap();
}

#[tokio::test]
async fn connection_and_cloned_opener_preserve_fin_and_application_close() {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let (_server, _client, remote, local) = pair().await;
        let mut guarded = Connection::new(remote);
        let mut opened = poll_fn(|cx| guarded.poll_open_bidi(cx)).await.unwrap();
        let id = opened.send_id();
        poll_fn(|cx| opened.poll_ready(cx)).await.unwrap();
        poll_fn(|cx| opened.poll_finish(cx)).await.unwrap();
        let (_send, mut receive) = local.accept_bi().await.unwrap();
        assert_eq!(receive.id().index(), id.index());
        assert!(receive.read_chunk(16, true).await.unwrap().is_none());
        let mut opener = guarded.opener().clone();
        let mut stream = poll_fn(|cx| opener.poll_open_send(cx)).await.unwrap();
        poll_fn(|cx| stream.poll_finish(cx)).await.unwrap();
        assert!(local.accept_uni().await.unwrap().read_chunk(16, true).await.unwrap().is_none());
        opener.close(h3::error::Code::H3_REQUEST_CANCELLED, b"cancel-test");
        assert!(matches!(local.closed().await, quinn::ConnectionError::ApplicationClosed(error) if error.error_code.into_inner() == h3::error::Code::H3_REQUEST_CANCELLED.value() && error.reason.as_ref() == b"cancel-test"));
    }).await.unwrap();
}
