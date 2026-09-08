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

fn v_bytes(s: &Accounted<MemoryStore>, t: TenantId, class: OpClass) -> u64 {
    s.bytes(t, class)
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

#[tokio::test]
async fn bytes_are_counted_not_just_requests() {
    // ⚠️ A request count cannot express the budget M3 is held to. "Probe 64 posting lists
    // instead of 8" is ONE request either way and eight times the bytes -- that is the
    // whole point of the round-trip architecture, and it means a recall number bought with
    // unbounded bandwidth looks identical to an efficient one through a request counter.
    // Recall and bytes are one number, not two.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(1);
    let v = s.as_tenant(t);
    v.put(&k("a"), Bytes::from(vec![7u8; 1000])).await.unwrap();
    assert_eq!(v_bytes(&s, t, OpClass::Write), 1000);

    v.get(&k("a")).await.unwrap();
    assert_eq!(v_bytes(&s, t, OpClass::Read), 1000);

    // A ranged read is billed for what it moved, not for the object it came from.
    v.get_range(&k("a"), 0..64).await.unwrap();
    assert_eq!(v_bytes(&s, t, OpClass::Read), 1064);

    // A HEAD moves no body. Counting it as the object's size would make a metadata probe
    // look like a full fetch and hide the difference the design turns on.
    v.head(&k("a")).await.unwrap();
    assert_eq!(v_bytes(&s, t, OpClass::Read), 1064);
}

#[tokio::test]
async fn ranges_are_recorded_so_bytes_can_be_attributed_to_a_section() {
    // The store cannot know what a byte range MEANS -- sections are the format's idea. So
    // it records what was asked for, and the caller attributes it using the same footer
    // the reader used. That keeps the accounting honest without teaching the blob layer
    // about segments.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(2);
    let v = s.as_tenant(t);
    v.put(&k("seg"), Bytes::from(vec![0u8; 4096]))
        .await
        .unwrap();

    s.record_ranges();
    v.get_range(&k("seg"), 100..200).await.unwrap();
    v.get_range(&k("seg"), 3000..3100).await.unwrap();
    v.get_suffix(&k("seg"), 42).await.unwrap();

    let log = s.ranges();
    assert_eq!(log.len(), 3, "got {log:?}");
    assert_eq!(log[0], (k("seg"), 100..200));
    assert_eq!(log[1], (k("seg"), 3000..3100));
    // A suffix read is resolved to the range it actually moved, or it cannot be attributed
    // to a section at all -- and the footer is always read as a suffix.
    assert_eq!(log[2], (k("seg"), 4054..4096));

    // Bytes in a named span, which is what a section assertion needs.
    assert_eq!(s.bytes_in(&k("seg"), 0..1000), 100);
    assert_eq!(s.bytes_in(&k("seg"), 1000..4096), 142);
}

#[tokio::test]
async fn a_hinted_suffix_read_is_attributed_like_an_unhinted_one() {
    // ⚠️ It was not. `get_suffix` resolved its span to absolute bytes so the footer could be
    // attributed to a section; `get_suffix_as` — added when reads gained a cache class — did
    // not, and `Segment::open` uses the hinted one. The result was that the single read every
    // query on every segment performs was invisible to `ranges()` and `bytes_in()`, which are
    // the tools every byte and section assertion in this project is built on. Found by a test
    // trying to count how many times a hybrid query opened its segment, which measured zero.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(4242);
    let v = s.as_tenant(t);
    let key = Key::new("obj");
    v.put(&key, bytes::Bytes::from(vec![7u8; 5_000]))
        .await
        .unwrap();

    s.record_ranges();
    v.get_suffix(&key, 100).await.unwrap();
    v.get_suffix_as(&key, 100, pstore_blob::Class::Meta)
        .await
        .unwrap();
    let seen: Vec<_> = s.ranges().into_iter().filter(|(k, _)| *k == key).collect();
    assert_eq!(
        seen.len(),
        2,
        "a hinted suffix read was not recorded: {seen:?}"
    );
    for (_, r) in &seen {
        assert_eq!(*r, 4_900..5_000, "a suffix resolved to the wrong span");
    }
    assert_eq!(s.bytes_in(&key, 4_900..5_000), 200);
}
