//! What `MemoryStore`'s tags must guarantee, beyond "the same content gives the same tag".
//!
//! ⚠️ **M0a.11's other survivors were here.** `next_tag`'s FNV mixing had `^=` mutated to
//! `|=` and `&=` and both survived, because the only property under test was *equal content
//! yields an equal tag* — which a constant function satisfies. The property that actually
//! carries weight is the other one: **different content yields a different tag**, because a
//! collision is a CAS that should have been rejected and was not.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{BlobStore, Key, MemoryStore, Precondition, TagStyle};
use std::collections::HashSet;

#[tokio::test]
async fn distinct_content_gets_distinct_tags() {
    // ⚠️ `|=` saturates toward all-ones and `&=` collapses toward zero, so both make bodies
    // collide within a few dozen writes — and every existing tag test still passes, since
    // they all ask whether *equal* content agrees.
    let s = MemoryStore::with_tag_style(TagStyle::ContentHash);
    let k = Key::new("k");
    let mut tags = HashSet::new();
    for i in 0..256u32 {
        let body = Bytes::from(format!("body-{i:08}"));
        let out = s.put(&k, body).await.unwrap();
        assert!(
            tags.insert(out.tag.as_str().to_owned()),
            "body {i} collided with an earlier tag: {}",
            out.tag.as_str()
        );
    }

    // Single-byte differences too, which is where a weak mixer shows first.
    let mut tags = HashSet::new();
    for i in 0..=255u8 {
        let out = s.put(&k, Bytes::from(vec![i])).await.unwrap();
        assert!(
            tags.insert(out.tag.as_str().to_owned()),
            "byte {i} collided"
        );
    }
}

#[tokio::test]
async fn a_colliding_tag_would_admit_a_cas_that_should_lose() {
    // The consequence, stated as behaviour rather than as a hash property: a writer holding
    // the tag of one body must not be able to overwrite a *different* body.
    let s = MemoryStore::with_tag_style(TagStyle::ContentHash);
    let k = Key::new("k");
    let first = s.put(&k, Bytes::from_static(b"alpha")).await.unwrap();
    s.put(&k, Bytes::from_static(b"beta")).await.unwrap();

    assert!(
        s.put_conditional(
            &k,
            Bytes::from_static(b"gamma"),
            Precondition::Match(first.tag)
        )
        .await
        .is_err(),
        "a stale tag was accepted, so the two bodies share one"
    );
}

#[tokio::test]
async fn the_declared_defaults_are_what_the_fixtures_rely_on() {
    // ⚠️ `MemoryStore::with_coalesce_gap` exists *because* the default is larger than a small
    // fixture's whole object, so every ranged read merges into one. That makes the default a
    // number other tests depend on rather than an arbitrary one, and mutation testing found
    // nothing pinning it.
    let s = MemoryStore::new();
    let c = s.capabilities();
    assert_eq!(c.coalesce_gap, 64 * 1024);
    assert_eq!(c.max_batch_delete, 1000);
    assert!(c.delete_is_free);
    assert!(c.admits_durable_writes());
    assert_eq!(
        MemoryStore::with_tag_style(TagStyle::ContentHash)
            .capabilities()
            .coalesce_gap,
        64 * 1024,
        "the tag style must not change the coalescing gap"
    );
}
