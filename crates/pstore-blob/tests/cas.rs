//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! CAS is the primitive the whole architecture rests on, so it is the first thing tested.
use bytes::Bytes;
use pstore_blob::{BlobStore, CasError, Key, MemoryStore, Precondition};

fn k(s: &str) -> Key {
    Key::new(s)
}

fn b(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

#[tokio::test]
async fn create_if_absent_rejects_second_write() {
    let s = MemoryStore::new();
    s.put_conditional(&k("a"), b("one"), Precondition::NotExists)
        .await
        .expect("first create must succeed");
    let err = s
        .put_conditional(&k("a"), b("two"), Precondition::NotExists)
        .await
        .expect_err("second create must be refused");
    assert!(matches!(err, CasError::Lost), "got {err:?}");
    assert_eq!(
        &s.get(&k("a")).await.unwrap()[..],
        b"one",
        "loser must not overwrite"
    );
}

#[tokio::test]
async fn cas_rejects_stale_tag() {
    let s = MemoryStore::new();
    let first = s
        .put_conditional(&k("h"), b("v1"), Precondition::NotExists)
        .await
        .unwrap();
    s.put_conditional(&k("h"), b("v2"), Precondition::Match(first.tag.clone()))
        .await
        .expect("CAS on the observed tag must succeed");
    // The fencing property: a writer holding the old tag cannot land, however long it
    // was paused for.
    let err = s
        .put_conditional(&k("h"), b("v3"), Precondition::Match(first.tag))
        .await
        .expect_err("stale tag must be fenced out");
    assert!(matches!(err, CasError::Lost), "got {err:?}");
    assert_eq!(&s.get(&k("h")).await.unwrap()[..], b"v2");
}

#[tokio::test]
async fn cas_lost_and_contended_are_distinct() {
    // 412 means another writer won: rebase and retry. 409 means the backend could not
    // evaluate the condition: retry WITHOUT rebasing. Conflating them causes rebase
    // storms, so they are distinct types rather than one status code.
    assert_ne!(CasError::Lost, CasError::Contended);
    assert!(CasError::Lost.should_rebase());
    assert!(!CasError::Contended.should_rebase());
}

#[tokio::test]
async fn get_range_returns_the_requested_slice() {
    let s = MemoryStore::new();
    s.put(&k("r"), b("0123456789")).await.unwrap();
    assert_eq!(&s.get_range(&k("r"), 2..5).await.unwrap()[..], b"234");
    assert_eq!(
        &s.get_range(&k("r"), 0..10).await.unwrap()[..],
        b"0123456789"
    );
}

#[tokio::test]
async fn delete_batch_removes_only_the_named_keys() {
    let s = MemoryStore::new();
    for n in ["a", "b", "c"] {
        s.put(&k(n), b("x")).await.unwrap();
    }
    s.delete_batch(&[k("a"), k("c")]).await.unwrap();
    assert!(s.get(&k("a")).await.is_err());
    assert!(s.get(&k("b")).await.is_ok());
    assert!(s.get(&k("c")).await.is_err());
}
