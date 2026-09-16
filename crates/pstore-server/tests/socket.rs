//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M7c.4 — over a real socket.
//!
//! ⚠️ **Not `oneshot`.** Every other test in this crate calls the router directly, which is
//! fast and exercises no listener, no connection handling and no shutdown path. A router that
//! only ever runs that way can be broken in a real stack with the whole suite green.

use pstore_blob::{Accounted, MemoryStore};
use pstore_server::{Api, Config, ConfigError, serve};
use pstore_types::LaneId;
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One HTTP/1.1 request over TCP, returning `(status line, body)`.
///
/// ⚠️ Hand-rolled rather than pulling in a client: the point is to exercise *our* listener,
/// and a dependency whose own connection pooling sits in between measures something else.
async fn request(addr: std::net::SocketAddr, head: &str, body: &str) -> (String, String) {
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{head}\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    sock.read_to_string(&mut raw).await.unwrap();
    let (headers, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = headers.lines().next().unwrap_or_default().to_owned();
    (status, body.to_owned())
}

#[tokio::test]
async fn the_server_answers_over_a_real_socket() {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(serve(api, listener, async {
        let _ = stopped.await;
    }));

    let (status, body) = request(
        addr,
        "PUT /v1/indexes/docs/documents HTTP/1.1\r\nX-Pstore-Tenant: 7",
        r#"{"durability":"durable","documents":[{"id":"over-tcp","vector":[1.0,0.5,-0.25,1.0]}]}"#,
    )
    .await;
    assert!(status.contains("200"), "{status} {body}");
    assert!(body.contains("\"durable\":true"), "{body}");

    let (status, body) = request(
        addr,
        "POST /v1/indexes/docs/query HTTP/1.1\r\nX-Pstore-Tenant: 7",
        r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":3}"#,
    )
    .await;
    assert!(status.contains("200"), "{status} {body}");
    assert!(body.contains("over-tcp"), "{body}");

    // ⚠️ A refusal has to survive the real stack too: an extractor that panics rather than
    // refusing is a 500 here and a clean `400` under `oneshot`.
    let (status, body) = request(
        addr,
        "POST /v1/indexes/docs/query HTTP/1.1",
        r#"{"vector":[1.0],"top_k":3}"#,
    )
    .await;
    assert!(status.contains("400"), "{status} {body}");
    assert!(body.contains("tenant_required"), "{body}");

    // ⚠️ And it stops on the signal rather than being killed: `serve` returns `Ok`.
    drop(stop);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), served)
        .await
        .expect("the server did not shut down within five seconds")
        .expect("the serving task panicked");
    assert!(outcome.is_ok(), "serve returned {outcome:?}");
}

#[tokio::test]
async fn the_server_refuses_to_start_without_a_lane() {
    // ⚠️ Lanes are single-writer and dense. A defaulted `LaneId(0)` is silent, correct-looking,
    // and makes two servers overwrite each other's bundles -- so the refusal is the feature.
    let vars: HashMap<&str, &str> = HashMap::new();
    let err = Config::from_vars(|k| vars.get(k).map(|v| (*v).to_owned())).unwrap_err();
    assert_eq!(err, ConfigError::Lane);
    assert!(
        err.to_string().contains("overwrite"),
        "the refusal must say what goes wrong, not just that it did: {err}"
    );

    for bad in ["", "one", "-1"] {
        assert_eq!(
            Config::from_vars(|k| (k == "PSTORE_LANE").then(|| bad.to_owned())).unwrap_err(),
            ConfigError::Lane,
            "PSTORE_LANE={bad:?} was accepted"
        );
    }

    let ok = Config::from_vars(|k| match k {
        "PSTORE_LANE" => Some("3".to_owned()),
        "PSTORE_BIND" => Some("0.0.0.0:9".to_owned()),
        _ => None,
    })
    .unwrap();
    assert_eq!(ok.lane, LaneId(3));
    assert_eq!(ok.bind, "0.0.0.0:9");

    // The bind address has a default; the lane does not, and that asymmetry is the point.
    let defaulted = Config::from_vars(|k| (k == "PSTORE_LANE").then(|| "1".to_owned())).unwrap();
    assert_eq!(defaulted.bind, "127.0.0.1:8080");
}
