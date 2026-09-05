//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The adapter's job in M0a is to answer one question -- does the trait fit a real
//! backend? -- so these tests check the shape, not the network.
//!
//! ⚠️ Integration against S3/GCS/Azure is NOT verified here. That is M0b.
#![cfg(feature = "object_store")]

use pstore_blob::{BlobStore, ObjectStoreBackend, Support};
use std::sync::Arc;

#[tokio::test]
async fn the_trait_fits_a_real_object_store_implementation() {
    // `object_store`'s in-memory backend stands in for S3 here. It proves the adapter
    // compiles, dispatches and round-trips; it proves NOTHING about S3's semantics,
    // which is why the profile below is not Supported.
    let inner = Arc::new(object_store::memory::InMemory::new());
    let s = ObjectStoreBackend::new(
        inner,
        ObjectStoreBackend::unprobed("object_store::InMemory"),
    );

    let k = pstore_blob::Key::new("a/b");
    s.put(&k, bytes::Bytes::from_static(b"0123456789"))
        .await
        .unwrap();
    assert_eq!(&s.get(&k).await.unwrap()[..], b"0123456789");
    assert_eq!(&s.get_range(&k, 2..5).await.unwrap()[..], b"234");
    assert_eq!(s.head(&k).await.unwrap(), 10);
    assert!(s.get_tag(&k).await.is_some());
    assert_eq!(
        s.list_unrestricted(&pstore_blob::Key::new("a"))
            .await
            .unwrap()
            .len(),
        1
    );
    s.delete_batch(std::slice::from_ref(&k)).await.unwrap();
    assert!(s.get(&k).await.is_err());
}

#[tokio::test]
async fn an_unprobed_backend_is_divergent_not_supported() {
    // A backend is divergent until the conformance suite says otherwise. Defaulting to
    // Supported is how a system ends up trusting MinIO's ignored wildcard.
    let caps = ObjectStoreBackend::unprobed("whatever");
    assert!(matches!(caps.cas, Support::Divergent(_)));
    assert!(matches!(caps.create_if_absent, Support::Divergent(_)));
    assert!(
        !caps.delete_is_free,
        "billed deletes are the safe assumption"
    );
}

#[tokio::test]
async fn coalescing_works_through_the_adapter() {
    // `get_ranges` is a provided trait method, so the adapter inherits it -- the point of
    // making it provided rather than required.
    let inner = Arc::new(object_store::memory::InMemory::new());
    let s = ObjectStoreBackend::new(inner, ObjectStoreBackend::unprobed("mem"));
    let k = pstore_blob::Key::new("o");
    s.put(&k, bytes::Bytes::from((0..=255u8).collect::<Vec<_>>()))
        .await
        .unwrap();
    let got = s.get_ranges(&k, &[0..4, 8..12, 250..256]).await.unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(&got[0][..], &[0, 1, 2, 3]);
    assert_eq!(got[2].len(), 6);
}

#[tokio::test]
async fn the_conformance_suite_runs_through_the_adapter() {
    // The same contract the in-process store satisfies. Against object_store's InMemory
    // it should pass -- and in M0b the same call against real S3 is the comparison that
    // makes the whole capability matrix worth having.
    let inner = Arc::new(object_store::memory::InMemory::new());
    let s = ObjectStoreBackend::new(inner, ObjectStoreBackend::unprobed("InMemory"));
    let r = pstore_testkit::conformance::run(&s, 1).await;
    assert!(r.conforms(), "divergences: {:?}", r.divergences());
    // ⚠️ And the DECLARED profile still says Divergent, because nothing has probed a real
    // cloud. Observed beats declared in both directions.
    assert!(matches!(s.capabilities().cas, Support::Divergent(_)));
    assert_eq!(r.observed.cas, Support::Supported);
}

#[tokio::test]
async fn a_short_read_is_an_error_not_a_truncation() {
    // The divergence the conformance suite found. HTTP Range may answer an overrunning
    // range with 206 and fewer bytes; the adapter turns that into an error, because a
    // silent truncation surfaces later as a recall bug rather than as a failure here.
    let inner = Arc::new(object_store::memory::InMemory::new());
    let s = ObjectStoreBackend::new(inner, ObjectStoreBackend::unprobed("mem"));
    let k = pstore_blob::Key::new("short");
    s.put(&k, bytes::Bytes::from_static(b"0123")).await.unwrap();
    assert_eq!(&s.get_range(&k, 1..3).await.unwrap()[..], b"12");
    assert!(
        s.get_range(&k, 2..99).await.is_err(),
        "an overrunning range must fail rather than return what happens to exist"
    );
}
