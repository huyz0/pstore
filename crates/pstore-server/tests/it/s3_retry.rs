//! M282: the S3 client's retry policy, against a fake S3 that answers `503` after a write lands.
//!
//! ⚠️ **A conditional write that got a status is never repeated.** A `503` does not prove the
//! write did not commit, and a repeat of one that did meets its own object as a rival: `412`,
//! read as `Lost`, so a bundle's own write reads as another writer on its lane and a HEAD
//! commit's as a lost race. Reads and unconditional writes are idempotent and keep their retries.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{BlobStore, CasError, Key, Precondition};
use pstore_server::{Profile, s3_store};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the fake does, and what it saw.
#[derive(Default)]
struct Fake {
    objects: HashMap<String, Vec<u8>>,
    /// Requests seen, as `METHOD path`.
    seen: Vec<String>,
    /// Answer `503` to this many more GETs, before serving them.
    failing_gets: u32,
    /// Answer `503` to this many more PUTs, **after** storing them.
    landing_puts: u32,
}

const ETAG: &str = "\"e1\"";
const DATE: &str = "Tue, 17 Nov 2026 00:00:00 GMT";

fn reply(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nconnection: close\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

fn answer(fake: &mut Fake, method: &str, path: &str, headers: &str, body: Vec<u8>) -> Vec<u8> {
    fake.seen.push(format!("{method} {path}"));
    let unavailable = || {
        reply(
            "503 Service Unavailable",
            &[],
            b"<Error><Code>ServiceUnavailable</Code></Error>",
        )
    };
    match method {
        "PUT" => {
            let create = headers
                .lines()
                .any(|l| l.to_ascii_lowercase().starts_with("if-none-match:") && l.contains('*'));
            if create && fake.objects.contains_key(path) {
                return reply(
                    "412 Precondition Failed",
                    &[],
                    b"<Error><Code>PreconditionFailed</Code></Error>",
                );
            }
            fake.objects.insert(path.to_owned(), body);
            if fake.landing_puts > 0 {
                fake.landing_puts -= 1;
                return unavailable();
            }
            reply("200 OK", &[("etag", ETAG.to_owned())], b"")
        }
        "GET" => {
            if fake.failing_gets > 0 {
                fake.failing_gets -= 1;
                return unavailable();
            }
            match fake.objects.get(path) {
                Some(b) => reply(
                    "200 OK",
                    &[
                        ("etag", ETAG.to_owned()),
                        ("last-modified", DATE.to_owned()),
                    ],
                    b,
                ),
                None => reply(
                    "404 Not Found",
                    &[],
                    b"<Error><Code>NoSuchKey</Code></Error>",
                ),
            }
        }
        _ => reply("501 Not Implemented", &[], b""),
    }
}

/// Serves the fake on a loopback port, one request a connection.
async fn serve() -> (String, Arc<Mutex<Fake>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let fake = Arc::new(Mutex::new(Fake::default()));
    let state = Arc::clone(&fake);
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let split = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..split]).into_owned();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                let mut body = buf[split + 4..].to_vec();
                while body.len() < len {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                let mut words = head.split_whitespace();
                let method = words.next().unwrap_or_default().to_owned();
                let path = words.next().unwrap_or_default().to_owned();
                let out = answer(&mut state.lock().unwrap(), &method, &path, &head, body);
                let _ = sock.write_all(&out).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (endpoint, fake)
}

async fn store(endpoint: &str) -> pstore_blob::ObjectStoreBackend {
    let creds = ("key".to_owned(), "secret".to_owned());
    s3_store(
        endpoint,
        "b",
        Some(&creds),
        "us-east-1",
        Profile::Conforming,
    )
    .unwrap()
}

fn seen(fake: &Mutex<Fake>, method: &str) -> usize {
    let fake = fake.lock().unwrap();
    fake.seen.iter().filter(|r| r.starts_with(method)).count()
}

#[tokio::test]
async fn a_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated() {
    let (endpoint, fake) = serve().await;
    let s = store(&endpoint).await;
    fake.lock().unwrap().landing_puts = 1;
    let got = s
        .put_conditional(
            &Key::new("t/wal/1"),
            Bytes::from_static(b"mine"),
            Precondition::NotExists,
        )
        .await;
    // Uncertain, which the engine resolves by reading (M17): never `Lost`, a second writer.
    assert!(matches!(got, Err(CasError::Io(_))), "{got:?}");
    assert_eq!(seen(&fake, "PUT"), 1, "a conditional write was repeated");
    assert_eq!(
        s.get(&Key::new("t/wal/1")).await.unwrap(),
        Bytes::from_static(b"mine")
    );
}

#[tokio::test]
async fn a_read_that_gets_a_503_is_retried() {
    let (endpoint, fake) = serve().await;
    let s = store(&endpoint).await;
    s.put(&Key::new("t/seg"), Bytes::from_static(b"seg"))
        .await
        .unwrap();
    fake.lock().unwrap().failing_gets = 1;
    assert_eq!(
        s.get(&Key::new("t/seg")).await.unwrap(),
        Bytes::from_static(b"seg")
    );
    assert_eq!(seen(&fake, "GET"), 2);
}

#[tokio::test]
async fn an_unconditional_write_that_gets_a_503_is_retried() {
    let (endpoint, fake) = serve().await;
    let s = store(&endpoint).await;
    fake.lock().unwrap().landing_puts = 1;
    s.put(&Key::new("t/seg"), Bytes::from_static(b"seg"))
        .await
        .unwrap();
    assert_eq!(seen(&fake, "PUT"), 2);
}

/// Azure goes through the same pair of clients (M46's `azure_store`).
#[tokio::test]
async fn an_azure_conditional_write_that_lands_and_then_gets_a_503_is_not_repeated() {
    let (endpoint, fake) = serve().await;
    let config = pstore_server::AzureConfig {
        account: "devstoreaccount1".to_owned(),
        key: Some("a2V5".to_owned()),
        container: "c".to_owned(),
        endpoint: Some(format!("{endpoint}/devstoreaccount1")),
    };
    let s = pstore_server::azure_store(&config, Profile::Conforming).unwrap();
    fake.lock().unwrap().landing_puts = 1;
    let got = s
        .put_conditional(
            &Key::new("t/wal/1"),
            Bytes::from_static(b"mine"),
            Precondition::NotExists,
        )
        .await;
    assert!(matches!(got, Err(CasError::Io(_))), "{got:?}");
    assert_eq!(seen(&fake, "PUT"), 1, "a conditional write was repeated");
    // And a read is retried, as on S3.
    fake.lock().unwrap().failing_gets = 1;
    assert_eq!(
        s.get(&Key::new("t/wal/1")).await.unwrap(),
        Bytes::from_static(b"mine")
    );
    assert_eq!(seen(&fake, "GET"), 2);
}

/// The policies, by number: a dependency bump cannot move them unseen.
#[test]
fn the_retry_policies_are_pinned() {
    let r = pstore_server::retries();
    assert_eq!(r.max_retries, 10);
    assert_eq!(r.retry_timeout, std::time::Duration::from_secs(180));
    assert_eq!(
        r.backoff.init_backoff,
        std::time::Duration::from_millis(100)
    );
    assert_eq!(r.backoff.max_backoff, std::time::Duration::from_secs(15));
    assert!((r.backoff.base - 2.0).abs() < f64::EPSILON);
    let c = pstore_server::conditional_retries();
    assert_eq!(c.max_retries, 0);
    assert_eq!(c.retry_timeout, r.retry_timeout);
}
