//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M46: the Azure backend end to end, against Azurite. **Ignored** here, because it needs a
//! running emulator: `scripts/azurite.sh` starts one and runs it. ⚠️ Emulator evidence --
//! provisional, and silent on Azure's latency, cost and CAS under contention (M0b).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, ObjectStoreBackend};
use pstore_engine::Engine;
use pstore_format::{Document, Impact, Value, VectorField};
use pstore_query::{Fusion, Prefetch};
use pstore_server::{Api, Config, Profile, azure_store};
use pstore_types::{Epoch, LaneId, TenantId};
use serde_json::{Value as Json, json};
use std::sync::Arc;
use tower::ServiceExt;

const T: TenantId = TenantId(4600);

fn doc(i: u32) -> Document {
    let x = i as f32;
    let mut d = Document::new(format!("d{i:04}"), vec![x.sin(), x.cos(), 1.0]);
    d.attrs.insert("n".to_owned(), Value::Int(i64::from(i)));
    d.attrs.insert(
        "text".to_owned(),
        Value::Str(format!("word{} word{}", i % 5, i % 3)),
    );
    d.vectors.insert(
        "s".to_owned(),
        VectorField::Sparse(vec![(3, Impact::new(0.5)), (i % 7, Impact::new(0.25))]),
    );
    d
}

fn legs() -> [Vec<Prefetch>; 3] {
    [
        vec![Prefetch::Dense {
            field: pstore_format::DEFAULT_FIELD.to_owned(),
            query: vec![0.3, 0.9, 1.0],
            limit: 5,
            tune: pstore_index::vec_index::Query::default(),
        }],
        vec![Prefetch::Text {
            field: "text".to_owned(),
            query: "word2".to_owned(),
            limit: 5,
        }],
        vec![Prefetch::Sparse {
            field: "s".to_owned(),
            query: vec![(3, 1.0), (2, 0.5)],
            limit: 5,
        }],
    ]
}

/// Every leg's ranking, now or `as_of` an epoch.
async fn ranked<S: BlobStore + 'static>(e: &Engine<S>, at: Option<Epoch>) -> String {
    let mut out = String::new();
    for p in legs() {
        let a = match at {
            Some(at) => e.query_as_of("idx", at, &p, Fusion::default(), 5).await,
            None => e.query("idx", &p, Fusion::default(), 5).await,
        }
        .unwrap();
        out.push_str(&format!("{:?}\n", e.resolve(&a)));
    }
    out
}

/// One request through the server's own router, as a client sends it.
async fn send(
    api: &Arc<Api<ObjectStoreBackend>>,
    tenant: TenantId,
    method: &str,
    uri: &str,
    body: Option<&Json>,
) -> Json {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.0.to_string())
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let out: Json = serde_json::from_slice(&bytes).unwrap_or(Json::Null);
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {out}");
    out
}

fn ids(b: &Json) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
#[ignore = "needs Azurite: run scripts/azurite.sh"]
async fn the_server_writes_folds_and_answers_as_of_against_azurite() {
    // The server's own stack -- `Accounted`, its tenant views, the routes -- over Azure.
    let config = Config::from_vars(|k| std::env::var(k).ok()).unwrap();
    let azure = config.azure.expect("PSTORE_BACKEND=azure");
    let store = azure_store(&azure, Profile::Conforming).unwrap();
    let api = Api::new(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let tenant = TenantId(T.0 + 1000 + u128::from(std::process::id() % 1000));
    let mut at = 0;
    for k in 0..3u32 {
        let docs: Vec<Json> = (k * 8..k * 8 + 8)
            .map(|i| {
                let x = i as f32;
                json!({"id": format!("d{i:04}"), "vector": [x.sin(), x.cos(), 1.0],
                       "attributes": {"n": i}})
            })
            .collect();
        let body = json!({"durability": "durable", "documents": docs});
        send(
            &api,
            tenant,
            "PUT",
            "/v1/indexes/idx/documents",
            Some(&body),
        )
        .await;
        at = send(&api, tenant, "POST", "/v1/admin/fold", None).await["epoch"]
            .as_u64()
            .unwrap();
    }
    let q = json!({"vector": [0.3, 0.9, 1.0], "top_k": 5});
    let before = ids(&send(&api, tenant, "POST", "/v1/indexes/idx/query", Some(&q)).await);
    assert_eq!(before.len(), 5);

    // No route compacts, so an engine over the same container does.
    Engine::new(Arc::new(store), tenant, LaneId(2))
        .compact("idx")
        .await
        .unwrap()
        .expect("a merge");
    let past = json!({"vector": [0.3, 0.9, 1.0], "top_k": 5, "as_of": at});
    let got = send(&api, tenant, "POST", "/v1/indexes/idx/query", Some(&past)).await;
    assert_eq!(got["meta"]["epoch"], at, "{got}");
    assert_eq!(ids(&got), before, "as_of through the server differs");
}

#[tokio::test]
#[ignore = "needs Azurite: run scripts/azurite.sh"]
async fn writes_folds_queries_and_reads_as_of_against_azurite() {
    let config = Config::from_vars(|k| std::env::var(k).ok()).unwrap();
    let azure = config.azure.expect("PSTORE_BACKEND=azure");
    let store = azure_store(&azure, Profile::Conforming).unwrap();
    assert!(!store.capabilities().suffix_read);
    // The profile admits durable writes, so a server would serve this container.
    assert!(Api::new(Accounted::new(store.clone()), LaneId(9)).is_ok());

    // A tenant of its own each run, so a reused emulator never answers from an old one.
    let tenant = TenantId(T.0 + u128::from(std::process::id() % 1000));
    let store = Arc::new(store);
    let w = Engine::new(Arc::clone(&store), tenant, LaneId(1));
    let mut at = Epoch(0);
    for k in 0..3u32 {
        w.write("idx", (k * 8..k * 8 + 8).map(doc).collect())
            .await
            .unwrap();
        w.flush().await.unwrap();
        at = w.fold().await.unwrap();
    }
    let before = ranked(&Engine::new(Arc::clone(&store), tenant, LaneId(2)), None).await;
    assert!(before.contains("d00"), "{before}");

    w.compact("idx").await.unwrap().expect("a merge");
    let r = Engine::new(Arc::clone(&store), tenant, LaneId(3));
    assert_eq!(
        ranked(&r, None).await,
        before,
        "a compaction changed the answers"
    );
    // The epoch before the compaction: segments only the graveyard names, with no length,
    // so each opens by `head` then range (M46).
    assert_eq!(ranked(&r, Some(at)).await, before, "as_of differs");
}
