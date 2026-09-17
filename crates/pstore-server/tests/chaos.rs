//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The API under injected faults — M7f.
//!
//! ⚠️ **The property, and it is the only one worth asserting here**: under faults the API
//! never answers *wrongly*. It answers correctly, or it refuses with a code. A wrong document,
//! a short result set, or a `404` for an index that exists are all the same failure wearing
//! different clothes — the one failure mode that looks like success.
//!
//! `pstore-sim`, the fault-injecting store and M2's linearizability proof all exist at the
//! library layer and had never been pointed at the server.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, Faults, Faulty, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

type Store = Faulty<MemoryStore>;

/// The corpus: small, exact, and every vector its own nearest neighbour.
const CORPUS: usize = 12;

fn vector(i: usize) -> String {
    format!("[{}.0,0.5,-0.25,1.0]", i)
}

async fn send<S: pstore_blob::BlobStore + 'static>(
    api: &Arc<Api<S>>,
    req: Request<Body>,
) -> (StatusCode, serde_json::Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn write(i: usize) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(format!(
            r#"{{"durability":"durable","documents":[{{"id":"d{i}","vector":{}}}]}}"#,
            vector(i)
        )))
        .unwrap()
}

/// ⚠️ An **exact-match** query with `top_k` ≥ the corpus, so "correct" is a set and never an
/// order: RRF ranking over a fault-perturbed segment set is not a stable oracle.
fn query(i: usize) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/indexes/docs/query")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(format!(
            r#"{{"vector":{},"top_k":{}}}"#,
            vector(i),
            CORPUS * 2
        )))
        .unwrap()
}

fn fold() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/admin/fold")
        .header("x-pstore-tenant", "7")
        .body(Body::empty())
        .unwrap()
}

/// Every code the error table can answer with. A status outside this set, or a 2xx that is not
/// a correct answer, is the failure this suite exists to find.
fn is_a_refusal(status: StatusCode, body: &serde_json::Value) -> bool {
    !status.is_success() && body["error"]["code"].is_string()
}

fn ids(body: &serde_json::Value) -> BTreeSet<String> {
    body["results"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn under_injected_faults_every_answer_is_correct_or_a_refusal() {
    // Ten seeds: `Faulty` is deterministic per seed, so a failure replays exactly, and one
    // lucky interleaving cannot pass the suite.
    for seed in 0..10u64 {
        let backend = Faulty::new(
            MemoryStore::new(),
            seed,
            Faults {
                read_error: 0.3,
                slow_down: 0.3,
                ..Faults::none()
            },
        );
        let api: Arc<Api<Store>> = Api::new(Accounted::new(backend), LaneId(1)).unwrap();

        // ⚠️ **Attempted, not acknowledged**, and the difference is a finding this suite made
        // rather than a weakening of it. A `durable` write buffers its rows and then flushes;
        // when the flush is refused the rows are **restored to the memtable**, where they are
        // visible to this process exactly as a `batched` write's are. So a refused write may
        // still show up in a later query from the same instance -- the refusal was about
        // durability, not about visibility, and `batched` documents that state. What must
        // never appear is an id **nobody ever sent**.
        let mut attempted: BTreeSet<String> = BTreeSet::new();
        for i in 0..CORPUS {
            attempted.insert(format!("d{i}"));
            let (status, body) = send(&api, write(i)).await;
            if !status.is_success() {
                assert!(
                    is_a_refusal(status, &body),
                    "seed {seed}: a write failed without a code: {status} {body}"
                );
            }
            if i % 4 == 3 {
                let (status, body) = send(&api, fold()).await;
                assert!(
                    status.is_success() || is_a_refusal(status, &body),
                    "seed {seed}: a fold failed without a code: {status} {body}"
                );
            }
            let (status, body) = send(&api, query(i)).await;
            if status.is_success() {
                // ⚠️ **Never a superset, and never a wrong id.** A query may legitimately miss
                // rows a failed write never stored -- it may not invent one.
                let returned = ids(&body);
                assert!(
                    returned.is_subset(&attempted),
                    "seed {seed}: query returned ids nobody sent: {:?} against {:?}",
                    returned,
                    attempted
                );
            } else {
                assert!(
                    is_a_refusal(status, &body),
                    "seed {seed}: a query failed without a code: {status} {body}"
                );
                // ⚠️ A `404` for an index this test has written to is a **wrong answer**, not a
                // refusal: it is the shape a swallowed read error takes on this route.
                assert_ne!(
                    body["error"]["code"], "index_not_found",
                    "seed {seed}: the index vanished under a read fault: {body}"
                );
            }
        }
    }
}

#[tokio::test]
async fn an_acknowledged_document_is_never_missing_under_faults() {
    // ⚠️ **Read back through a SECOND `Api` over the same backend**, whose memtable is empty.
    // Asserting through the writer would prove only that its own RAM still holds the rows --
    // which is the case the code already handles. The interesting one is a `200` whose rows
    // exist nowhere else.
    for seed in 0..10u64 {
        let backend = Faulty::new(
            MemoryStore::new(),
            seed,
            Faults {
                read_error: 0.3,
                slow_down: 0.3,
                ..Faults::none()
            },
        );
        let writer: Arc<Api<Store>> = Api::new(Accounted::new(backend.clone()), LaneId(1)).unwrap();

        let mut durable: BTreeSet<String> = BTreeSet::new();
        for i in 0..CORPUS {
            let (status, _) = send(&writer, write(i)).await;
            if status.is_success() {
                durable.insert(format!("d{i}"));
            }
        }
        // Fold until it takes, or give up: a fold that never lands is a refusal, not a lie.
        let mut folded = false;
        for _ in 0..8 {
            let (status, _) = send(&writer, fold()).await;
            if status.is_success() {
                folded = true;
                break;
            }
        }
        if !folded || durable.is_empty() {
            continue;
        }

        // The faults stay ON for the reader: a refusal is acceptable, a short answer is not.
        let reader: Arc<Api<Store>> = Api::new(Accounted::new(backend), LaneId(2)).unwrap();
        let (status, body) = send(&reader, query(0)).await;
        if !status.is_success() {
            assert!(
                is_a_refusal(status, &body),
                "seed {seed}: the reader failed without a code: {status} {body}"
            );
            continue;
        }
        let seen = ids(&body);
        let missing: Vec<&String> = durable.iter().filter(|d| !seen.contains(*d)).collect();
        assert!(
            missing.is_empty(),
            "seed {seed}: documents acknowledged durable and folded are missing from a \
             successful query: {missing:?}"
        );
    }
}
