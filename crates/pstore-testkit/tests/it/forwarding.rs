//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Every test double here forwards what it does not deliberately change -- asserted on the
//! ANSWER, not on the call.
//!
//! `class_forwarding.rs` proves the class reaches the store beneath; it unwraps the result
//! and looks no further, so a decorator answering `Ok(empty)` passed it. The conformance
//! suite never calls `list_unrestricted`, `get_immutable` or the `_as` reads. Between the
//! two, seventeen forwarding methods across six doubles could return a constant and nothing
//! noticed (M8i) -- and a double that quietly stops forwarding turns every test built on it
//! into a test of the double.

use bytes::Bytes;
use pstore_blob::{BlobError, BlobStore, Class, Key, MemoryStore, Precondition};
use pstore_testkit::{
    audit::Auditing,
    broken::{Broken, Defect},
    claims::Claims,
    depth::DepthCounting,
    flaky::Flaky,
    gated::Gated,
};

/// Writes one object and checks that every read, the listing and the delete answer with
/// what the store beneath holds.
async fn answers_like_the_store_beneath<S: BlobStore>(s: &S, name: &str) {
    let key = Key::new("fw/a");
    let put = s
        .put_conditional(
            &key,
            Bytes::from_static(b"0123456789"),
            Precondition::NotExists,
        )
        .await
        .unwrap();

    assert_eq!(s.head(&key).await.unwrap(), 10, "{name}: head");
    assert_eq!(
        s.get_tag(&key).await.unwrap(),
        Some(put.tag),
        "{name}: get_tag"
    );
    assert_eq!(
        &s.get_range_as(&key, 2..5, Class::Meta).await.unwrap()[..],
        b"234",
        "{name}: get_range_as"
    );
    assert_eq!(
        &s.get_suffix_as(&key, 3, Class::Meta).await.unwrap()[..],
        b"789",
        "{name}: get_suffix_as"
    );
    assert_eq!(
        &s.get_immutable(&key, Class::Pinned).await.unwrap()[..],
        b"0123456789",
        "{name}: get_immutable"
    );
    assert_eq!(
        s.list_unrestricted(&Key::new("fw")).await.unwrap(),
        vec![key.clone()],
        "{name}: list_unrestricted"
    );

    s.delete_batch(std::slice::from_ref(&key)).await.unwrap();
    assert!(
        matches!(s.get(&key).await, Err(BlobError::NotFound(_))),
        "{name}: delete_batch left the object behind"
    );
}

#[tokio::test]
async fn every_double_forwards_what_it_does_not_change() {
    answers_like_the_store_beneath(&Auditing::new(MemoryStore::new()), "Auditing").await;
    // A defect on the create path only, so every read, the listing and the delete forward.
    answers_like_the_store_beneath(&Broken::new(Defect::IgnoresCreateIfAbsent), "Broken").await;
    answers_like_the_store_beneath(&Claims::conforming(), "Claims").await;
    answers_like_the_store_beneath(&DepthCounting::new(MemoryStore::new()), "DepthCounting").await;
    answers_like_the_store_beneath(&Flaky::new(0, 0.0), "Flaky").await;
    // Unarmed: the gate holds conditional writes only once armed.
    answers_like_the_store_beneath(&Gated::new(2), "Gated").await;
}
