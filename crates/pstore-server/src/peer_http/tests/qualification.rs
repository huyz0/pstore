//! Probe the pinned HTTP/3 stack on the wire rather than trusting its defaults.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "protocol fixture and assertion failures report through panic"
)]
use super::{Handler, serve};
#[path = "header_bound_tests.rs"]
mod header_bound_tests;
use crate::{PeerTls, peer_wire};
use bytes::Bytes;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct Raw {
    connection: quinn::Connection,
    _endpoint: quinn::Endpoint,
    _control: quinn::SendStream,
    _incoming: Vec<quinn::RecvStream>,
    settings: std::collections::BTreeMap<u64, u64>,
}

fn integer(value: u64) -> Vec<u8> {
    match value {
        0..=63 => vec![value as u8],
        64..=16383 => ((value as u16) | 0x4000).to_be_bytes().to_vec(),
        16384..=1073741823 => ((value as u32) | 0x8000_0000).to_be_bytes().to_vec(),
        _ => {
            assert!(value < (1_u64 << 62));
            (value | 0xc000_0000_0000_0000).to_be_bytes().to_vec()
        }
    }
}

fn take_integer(bytes: &[u8], offset: &mut usize) -> u64 {
    let first = bytes[*offset];
    let size = 1_usize << (first >> 6);
    let mut value = u64::from(first & 0x3f);
    for byte in &bytes[*offset + 1..*offset + size] {
        value = (value << 8) | u64::from(*byte);
    }
    *offset += size;
    value
}

async fn read_integer(stream: &mut quinn::RecvStream) -> u64 {
    let mut bytes = [0_u8; 8];
    stream.read_exact(&mut bytes[..1]).await.unwrap();
    let size = 1_usize << (bytes[0] >> 6);
    stream.read_exact(&mut bytes[1..size]).await.unwrap();
    take_integer(&bytes[..size], &mut 0)
}

fn frame(kind: u64, payload: &[u8]) -> Vec<u8> {
    let mut bytes = integer(kind);
    bytes.extend(integer(payload.len() as u64));
    bytes.extend_from_slice(payload);
    bytes
}

fn prefix_integer(bytes: &mut Vec<u8>, prefix: u8, width: u8, value: usize) {
    let max = (1_usize << width) - 1;
    bytes.push(prefix | value.min(max) as u8);
    if value < max {
        return;
    }
    let mut remaining = value - max;
    while remaining >= 128 {
        bytes.push((remaining as u8 & 127) | 128);
        remaining >>= 7;
    }
    bytes.push(remaining as u8);
}

fn literal(bytes: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    prefix_integer(bytes, 0x20, 3, name.len());
    bytes.extend_from_slice(name);
    prefix_integer(bytes, 0, 7, value.len());
    bytes.extend_from_slice(value);
}

fn headers(padding: usize) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    for (name, value) in [
        (b":method".as_slice(), b"POST".as_slice()),
        (b":scheme", b"https"),
        (b":authority", b"localhost"),
        (b":path", b"/peer/v1/part"),
        (b"content-type", b"application/msgpack"),
    ] {
        literal(&mut bytes, name, value);
    }
    literal(&mut bytes, b"x-padding", &vec![b'x'; padding]);
    bytes
}

async fn server() -> (
    std::net::SocketAddr,
    PeerTls,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let ca = crate::tls_for_tests::Authority::new();
    let material = ca.issue(&["localhost", "127.0.0.1"], false);
    let tls = PeerTls::from_pem(
        material.ca.as_bytes(),
        material.cert.as_bytes(),
        material.key.as_bytes(),
    )
    .unwrap();
    let (tcp, udp) = super::tests::same_port().await;
    let address = tcp.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let handler: Handler = {
        let calls = calls.clone();
        Arc::new(move |_, req| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                peer_wire::Reply {
                    version: peer_wire::VERSION,
                    id: req.id,
                    status: 200,
                    body: req.body,
                }
            })
        })
    };
    let identity = tls.clone();
    let task = tokio::spawn(async move {
        serve(tcp, udp, identity, handler).await.unwrap();
    });
    (address, tls, calls, task)
}

async fn connect(address: std::net::SocketAddr, tls: &PeerTls) -> Raw {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (_, config) = tls.configs_for(b"h3").unwrap();
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(config).unwrap();
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
        let connection = endpoint
            .connect(address, "localhost")
            .unwrap()
            .await
            .unwrap();
        let mut control = connection.open_uni().await.unwrap();
        control.write_all(&[0, 4, 0]).await.unwrap();
        let mut incoming = Vec::new();
        let settings = loop {
            let mut stream = connection.accept_uni().await.unwrap();
            let kind = read_integer(&mut stream).await;
            if kind != 0 {
                incoming.push(stream);
                continue;
            }
            assert_eq!(read_integer(&mut stream).await, 4);
            let size = read_integer(&mut stream).await as usize;
            assert!(size <= 4096);
            let mut bytes = vec![0; size];
            stream.read_exact(&mut bytes).await.unwrap();
            let mut settings = std::collections::BTreeMap::new();
            let mut offset = 0;
            while offset < bytes.len() {
                let key = take_integer(&bytes, &mut offset);
                let value = take_integer(&bytes, &mut offset);
                assert!(settings.insert(key, value).is_none());
            }
            incoming.push(stream);
            break settings;
        };
        Raw {
            connection,
            _endpoint: endpoint,
            _control: control,
            _incoming: incoming,
            settings,
        }
    })
    .await
    .unwrap()
}

fn data_frames(bytes: &[u8]) -> Vec<Bytes> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let kind = take_integer(bytes, &mut offset);
        let size = take_integer(bytes, &mut offset) as usize;
        if kind == 0 {
            result.push(Bytes::copy_from_slice(&bytes[offset..offset + size]));
        }
        offset += size;
    }
    result
}

#[tokio::test]
async fn http3_wire_settings_disable_dynamic_qpack_and_limit_header_sections() {
    let (address, tls, calls, task) = server().await;
    let raw = connect(address, &tls).await;
    assert_eq!(raw.settings.get(&6), Some(&8192));
    assert_eq!(raw.settings.get(&1).copied().unwrap_or(0), 0);
    assert_eq!(raw.settings.get(&7).copied().unwrap_or(0), 0);
    for padding in [8, 9000] {
        let (mut send, mut receive) = raw.connection.open_bi().await.unwrap();
        send.write_all(&frame(1, &headers(padding))).await.unwrap();
        let payload = peer_wire::encode(&peer_wire::Request {
            version: peer_wire::VERSION,
            id: 1,
            tenant: "9".to_owned(),
            body: json!({"positive":"control"}),
        })
        .unwrap();
        send.write_all(&frame(0, &payload)).await.unwrap();
        send.finish().unwrap();
        let bytes = tokio::time::timeout(Duration::from_secs(3), receive.read_to_end(65536))
            .await
            .unwrap()
            .unwrap();
        if padding == 8 {
            assert!(
                !data_frames(&bytes).is_empty(),
                "valid literal headers must reach dispatch"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        } else {
            assert!(
                data_frames(&bytes).is_empty(),
                "oversized headers must not return an application reply"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
    task.abort();
}

#[tokio::test]
async fn http3_refuses_dynamic_references_and_incomplete_headers_before_dispatch() {
    let (address, tls, calls, task) = server().await;
    for invalid in [vec![0, 0, 0x80], vec![1, 0], vec![0, 0, 0xff]] {
        let raw = connect(address, &tls).await;
        let (mut send, mut receive) = raw.connection.open_bi().await.unwrap();
        send.write_all(&frame(1, &invalid)).await.unwrap();
        send.finish().unwrap();
        let response = tokio::time::timeout(Duration::from_secs(3), receive.read_to_end(65536))
            .await
            .unwrap();
        assert!(
            response.is_err(),
            "invalid QPACK must fail transport decoding"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    let raw = connect(address, &tls).await;
    let (mut send, mut receive) = raw.connection.open_bi().await.unwrap();
    send.write_all(&[1, 20, 0, 0]).await.unwrap();
    let response = tokio::time::timeout(Duration::from_secs(3), receive.read_to_end(65536))
        .await
        .unwrap();
    assert!(
        match response {
            Err(_) => true,
            Ok(bytes) => bytes.is_empty(),
        },
        "an incomplete header block must release its stream without an application response"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    task.abort();
}
