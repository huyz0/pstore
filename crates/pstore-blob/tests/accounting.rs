//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! RA -- requests per operation, split by class -- is the currency of this project, so
//! it is asserted by a counter rather than reasoned about.
use bytes::Bytes;
use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass, Precondition};
use pstore_types::TenantId;

fn k(s: &str) -> Key {
    Key::new(s)
}

#[tokio::test]
async fn accounting_counts_one_write_per_put() {
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(1);
    for i in 0..10u8 {
        s.as_tenant(t)
            .put(&k(&format!("k{i}")), Bytes::from_static(b"x"))
            .await
            .unwrap();
    }
    // The write path's whole economic claim: RA(write batch) = 1 W, whatever the batch
    // holds. Counting bytes instead of requests would make this pass vacuously.
    assert_eq!(s.count(t, OpClass::Write), 10);
    assert_eq!(s.count(t, OpClass::Read), 0);
    assert_eq!(s.count(t, OpClass::List), 0);
}

#[tokio::test]
async fn accounting_separates_read_write_and_list_classes() {
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(7);
    let v = s.as_tenant(t);
    v.put(&k("a"), Bytes::from_static(b"hello")).await.unwrap();
    v.get(&k("a")).await.unwrap();
    v.get_range(&k("a"), 0..2).await.unwrap();
    v.head(&k("a")).await.unwrap();
    v.list_unrestricted(&k("")).await.unwrap();
    assert_eq!(s.count(t, OpClass::Write), 1);
    // HEAD is billed as a read, which is why it is never used where the manifest already
    // proves what exists.
    assert_eq!(s.count(t, OpClass::Read), 3);
    // LIST is billed at the WRITE rate on S3 and returns at most 1000 keys. Counting it
    // as a read would hide the one operation this architecture is built to avoid.
    assert_eq!(s.count(t, OpClass::List), 1);
}

#[tokio::test]
async fn accounting_is_per_tenant() {
    let s = Accounted::new(MemoryStore::new());
    let (a, b) = (TenantId(1), TenantId(2));
    s.as_tenant(a)
        .put(&k("a"), Bytes::from_static(b"x"))
        .await
        .unwrap();
    for i in 0..3u8 {
        s.as_tenant(b)
            .put(&k(&format!("b{i}")), Bytes::from_static(b"x"))
            .await
            .unwrap();
    }
    // Summing every tenant into one bucket would make per-tenant cost attribution -- and
    // therefore Design rule 13 -- unenforceable.
    assert_eq!(s.count(a, OpClass::Write), 1);
    assert_eq!(s.count(b, OpClass::Write), 3);
    assert_eq!(s.total(OpClass::Write), 4);
}

#[tokio::test]
async fn a_failed_conditional_write_is_still_billed() {
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(3);
    let v = s.as_tenant(t);
    v.put_conditional(&k("c"), Bytes::from_static(b"1"), Precondition::NotExists)
        .await
        .unwrap();
    v.put_conditional(&k("c"), Bytes::from_static(b"2"), Precondition::NotExists)
        .await
        .unwrap_err();
    // AWS charges for failed conditional requests too, so a CAS retry storm costs real
    // money. A counter that only counted successes would hide exactly that.
    assert_eq!(s.count(t, OpClass::Write), 2);
}
