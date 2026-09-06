//! What the engine does when the store misbehaves or the bytes are wrong.
//!
//! Every path here ends in an error, which is exactly why they need tests: an error branch
//! that is never taken is indistinguishable from one that panics, corrupts, or silently
//! succeeds. The two shapes are kept apart on purpose —
//!
//! - **The backend fails.** Transient, and the only correct response is to report it
//!   truthfully rather than to treat a failed read as an empty one. Treating a failed
//!   lane-registry read as "no lanes" would silently skip recovery of every writer.
//! - **The bytes are wrong.** Not transient. A truncated or corrupt object must be refused
//!   at the boundary, because everything above it trusts the decode.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_engine::{Engine, Head, lanes};
use pstore_format::Document;
use pstore_testkit::flaky::Flaky;
use pstore_types::{LaneId, Seq, TenantId};
use std::sync::Arc;

fn doc(id: &str) -> Document {
    Document {
        id: id.to_owned(),
        vectors: std::collections::BTreeMap::from([(
            pstore_format::DEFAULT_FIELD.to_owned(),
            pstore_format::VectorField::dense(vec![0.0; 4]),
        )]),
        attrs: Default::default(),
    }
}

#[tokio::test]
async fn a_failed_registry_read_is_an_error_not_an_empty_registry() {
    // ⚠️ The most dangerous confusion in the whole recovery path. `NotFound` means "no
    // lanes yet"; any other failure means "we do not know". Collapsing the two makes a
    // fold on a struggling backend quietly recover nothing and report success, advancing
    // watermarks over bundles it never read.
    let store = Flaky::refusing_reads();
    let t = TenantId(1);
    assert!(lanes::live(&store, t).await.is_err());
    assert!(lanes::register(&store, t, LaneId(0)).await.is_err());
    assert!(lanes::tail(&store, t, LaneId(0), 0).await.is_err());
    assert!(store.failures() > 0);
}

#[tokio::test]
async fn a_failed_head_read_is_an_error_not_an_empty_head() {
    // Same confusion, one layer up: an absent HEAD is a tenant that has never committed,
    // and a *failed* HEAD read treated the same way would let a writer commit epoch 1
    // over a tenant that is already at epoch 900.
    let store = Arc::new(Flaky::refusing_reads());
    let e = Engine::new(Arc::clone(&store), TenantId(2), LaneId(0));
    assert!(e.fold().await.is_err());
    assert!(e.scan("idx", None).await.is_err());
    assert!(e.compact("idx").await.is_err());
    assert!(e.gc(0).await.is_err());
}

#[tokio::test]
async fn a_corrupt_lane_registry_is_refused_not_guessed() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(3);
    // Not a multiple of the entry width: there is no honest way to read this, and a
    // partial parse would report a subset of lanes as if it were all of them.
    store
        .put(&lanes::key(t), Bytes::from_static(b"xxx"))
        .await
        .unwrap();
    assert!(lanes::live(&*store, t).await.is_err());
    assert!(lanes::register(&*store, t, LaneId(1)).await.is_err());
}

#[tokio::test]
async fn a_corrupt_head_is_refused_not_guessed() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(4);
    store
        .put(&Head::key(t), Bytes::from_static(b"not a manifest"))
        .await
        .unwrap();
    let e = Engine::new(Arc::clone(&store), t, LaneId(0));
    assert!(e.fold().await.is_err());
}

#[tokio::test]
async fn a_corrupt_segment_surfaces_as_a_format_error() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(5);
    let e = Engine::new(Arc::clone(&store), t, LaneId(0));
    e.write("idx", vec![doc("a")]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let seg = e.head_for_test().await.indexes["idx"][0].key.clone();
    // Overwritten with a plausible-length body that is not a segment: the footer magic
    // must catch this rather than the reader wandering into arbitrary offsets.
    store
        .put(&Key::new(seg), Bytes::from(vec![0u8; 512]))
        .await
        .unwrap();

    let reader = Engine::new(Arc::clone(&store), t, LaneId(9));
    let err = reader.scan("idx", None).await.unwrap_err();
    assert!(
        format!("{err}").contains("format"),
        "a corrupt segment surfaced as {err}"
    );
}

#[tokio::test]
async fn a_corrupt_bundle_fails_the_fold() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(6);
    let e = Engine::new(Arc::clone(&store), t, LaneId(0));
    e.write("idx", vec![doc("a")]).await.unwrap();
    e.flush().await.unwrap();

    store
        .put(
            &pstore_engine::bundle_key(t, LaneId(0), Seq(0)),
            Bytes::from_static(b"\xff\xff\xff\xff garbage"),
        )
        .await
        .unwrap();
    assert!(
        e.fold().await.is_err(),
        "a corrupt bundle was folded as if it were empty"
    );
}

#[tokio::test]
async fn a_backend_that_cannot_evaluate_the_condition_is_retried_and_then_reported() {
    // `Contended` is not `Lost`: nobody won, the backend simply could not answer. The
    // engine retries without rebasing, and when the budget runs out it says so rather
    // than reporting a commit that never happened.
    let store = Arc::new(Flaky::always_contended());
    let t = TenantId(7);
    let e = Engine::new(Arc::clone(&store), t, LaneId(0));
    e.write("idx", vec![doc("a")]).await.unwrap();
    // The lane registration is itself a CAS, so this fails before any bundle is written.
    assert!(e.flush().await.is_err());
    assert!(store.failures() > 1, "it gave up without retrying");
}

#[tokio::test]
async fn an_unknown_attribute_tag_is_refused() {
    // Forward compatibility has to be explicit. A value tag we do not know is a document
    // written by a future version, and guessing at it would silently drop or mistype an
    // attribute that a filter later depends on.
    let mut buf = Vec::new();
    buf.extend_from_slice(&1u32.to_le_bytes()); // one document
    buf.extend_from_slice(&1u32.to_le_bytes()); // id length
    buf.push(b'a');
    buf.extend_from_slice(&0u32.to_le_bytes()); // no vector
    buf.extend_from_slice(&1u32.to_le_bytes()); // one attribute
    buf.extend_from_slice(&1u32.to_le_bytes()); // key length
    buf.push(b'k');
    buf.push(200); // a value tag from the future
    assert!(pstore_format::decode_docs(&buf).is_err());
}

#[tokio::test]
async fn a_lane_that_never_ends_is_refused_rather_than_probed_forever() {
    // ⚠️ Probing terminates on a 404, which is an answer the STORE has to give. A backend
    // that keeps answering -- hostile, buggy, or serving a corrupted lane -- would spin
    // here forever, on a path a query waits on. Found as a hung test under mutation
    // testing rather than a failing one, which is the expensive way to find out.
    let err = lanes::tail(&Flaky::never_missing(), TenantId(9), LaneId(0), 0)
        .await
        .unwrap_err();
    assert!(
        format!("{err}").contains("probe bound"),
        "an endless lane surfaced as {err}"
    );
}
