//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::single_range_in_vec_init,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The semantics a real backend can get wrong, and the hazard our manifest design exists
//! to close.
use bytes::Bytes;
use pstore_blob::{BlobError, BlobStore, CasError, Key, MemoryStore, Precondition, TagStyle};
use pstore_types::CasTag;

fn k(s: &str) -> Key {
    Key::new(s)
}

#[tokio::test]
async fn content_hash_tags_exhibit_the_aba_hazard() {
    // OQ-6, made executable. An S3 ETag on a single-part unencrypted PUT is the MD5 of
    // the content, so writing v1 -> v2 -> v1 returns a tag EQUAL to the first. A writer
    // that read v1, paused, and woke up after the round trip will have its CAS ACCEPTED
    // against a world that changed and changed back.
    let s = MemoryStore::with_tag_style(TagStyle::ContentHash);
    let v1 = s.put(&k("h"), Bytes::from_static(b"v1")).await.unwrap();
    s.put(&k("h"), Bytes::from_static(b"v2")).await.unwrap();
    let back = s.put(&k("h"), Bytes::from_static(b"v1")).await.unwrap();
    assert_eq!(
        v1.tag, back.tag,
        "content-derived tags repeat -- that is the hazard"
    );

    let stale_writer_wins = s
        .put_conditional(
            &k("h"),
            Bytes::from_static(b"v3"),
            Precondition::Match(v1.tag),
        )
        .await;
    assert!(
        stale_writer_wins.is_ok(),
        "a content-hash backend lets the stale writer land; this is why HEAD carries a \
         monotonic epoch and a nonce so no two states are ever byte-identical"
    );
}

#[tokio::test]
async fn monotonic_tags_are_immune_to_aba() {
    // GCS generations. The same sequence, and the stale writer is fenced out.
    let s = MemoryStore::with_tag_style(TagStyle::Monotonic);
    let v1 = s.put(&k("h"), Bytes::from_static(b"v1")).await.unwrap();
    s.put(&k("h"), Bytes::from_static(b"v2")).await.unwrap();
    let back = s.put(&k("h"), Bytes::from_static(b"v1")).await.unwrap();
    assert_ne!(v1.tag, back.tag, "a generation never repeats");
    let err = s
        .put_conditional(
            &k("h"),
            Bytes::from_static(b"v3"),
            Precondition::Match(v1.tag),
        )
        .await
        .unwrap_err();
    assert_eq!(err, CasError::Lost);
}

#[tokio::test]
async fn cas_against_a_deleted_object_is_refused() {
    // The object the writer conditioned on is gone. Treating "absent" as "matches" would
    // resurrect data a GC pass deliberately reaped.
    let s = MemoryStore::new();
    let out = s.put(&k("d"), Bytes::from_static(b"x")).await.unwrap();
    s.delete_batch(&[k("d")]).await.unwrap();
    let err = s
        .put_conditional(
            &k("d"),
            Bytes::from_static(b"y"),
            Precondition::Match(out.tag),
        )
        .await
        .unwrap_err();
    assert_eq!(err, CasError::Lost);
}

#[tokio::test]
async fn cas_with_a_fabricated_tag_is_refused() {
    let s = MemoryStore::new();
    s.put(&k("f"), Bytes::from_static(b"x")).await.unwrap();
    let err = s
        .put_conditional(
            &k("f"),
            Bytes::from_static(b"y"),
            Precondition::Match(CasTag::new("not-a-real-tag")),
        )
        .await
        .unwrap_err();
    assert_eq!(err, CasError::Lost);
}

#[tokio::test]
async fn a_range_past_the_end_is_an_error_not_a_short_read() {
    // A short read would silently truncate a posting list, which scores as a recall bug
    // several layers away from its cause.
    let s = MemoryStore::new();
    s.put(&k("r"), Bytes::from_static(b"0123456789"))
        .await
        .unwrap();
    assert!(matches!(
        s.get_range(&k("r"), 5..11).await.unwrap_err(),
        BlobError::RangeOutOfBounds(_, 10)
    ));
    let reversed = std::ops::Range {
        start: 8u64,
        end: 2u64,
    };
    assert!(matches!(
        s.get_range(&k("r"), reversed).await.unwrap_err(),
        BlobError::RangeOutOfBounds(_, 10)
    ));
}

#[tokio::test]
async fn a_batch_delete_over_the_backend_limit_is_refused() {
    // S3 caps DeleteObjects at 1000. Silently truncating would leave orphans that the
    // manifest no longer references and nothing would ever reap.
    let s = MemoryStore::new();
    let too_many: Vec<Key> = (0..1001).map(|i| k(&format!("k{i}"))).collect();
    assert!(s.delete_batch(&too_many).await.is_err());
    assert!(s.delete_batch(&too_many[..1000]).await.is_ok());
}

#[tokio::test]
async fn head_reports_size_without_the_body() {
    let s = MemoryStore::new();
    s.put(&k("s"), Bytes::from(vec![0u8; 4096])).await.unwrap();
    assert_eq!(s.head(&k("s")).await.unwrap(), 4096);
    assert!(matches!(
        s.head(&k("missing")).await.unwrap_err(),
        BlobError::NotFound(_)
    ));
}

#[tokio::test]
async fn list_is_prefix_filtered() {
    let s = MemoryStore::new();
    for n in ["a/1", "a/2", "b/1"] {
        s.put(&k(n), Bytes::from_static(b"x")).await.unwrap();
    }
    let mut got = s.list_unrestricted(&k("a/")).await.unwrap();
    got.sort();
    assert_eq!(got, vec![k("a/1"), k("a/2")]);
}

#[tokio::test]
async fn get_ranges_propagates_a_read_error() {
    let s = MemoryStore::new();
    s.put(&k("g"), Bytes::from_static(b"0123")).await.unwrap();
    assert!(s.get_ranges(&k("g"), &[0..2, 2..99]).await.is_err());
    assert!(s.get_ranges(&k("missing"), &[0..2]).await.is_err());
}

#[tokio::test]
async fn get_ranges_of_nothing_is_not_a_request() {
    let s = MemoryStore::new();
    s.put(&k("g"), Bytes::from_static(b"0123")).await.unwrap();
    assert!(s.get_ranges(&k("g"), &[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn capabilities_name_the_backend_and_its_tag_style() {
    assert_eq!(
        MemoryStore::new().capabilities().backend,
        "memory(Monotonic)"
    );
    assert_eq!(
        MemoryStore::with_tag_style(TagStyle::ContentHash)
            .capabilities()
            .backend,
        "memory(ContentHash)"
    );
    assert!(MemoryStore::default().capabilities().delete_is_free);
}

#[tokio::test]
async fn an_empty_range_is_legal_and_returns_nothing() {
    // Found by mutation testing: `start > end` mutated to `>=` rejects a zero-length
    // range, and nothing noticed because `coalesce` drops empty ranges before they reach
    // the store. A direct read must still accept one.
    let s = MemoryStore::new();
    s.put(&k("e"), Bytes::from_static(b"0123")).await.unwrap();
    assert!(s.get_range(&k("e"), 2..2).await.unwrap().is_empty());
    assert!(s.get_range(&k("e"), 0..0).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_full_length_range_is_legal() {
    // The other side of the same mutation: `end > len` as `>=` would reject reading the
    // whole object by range.
    let s = MemoryStore::new();
    s.put(&k("f"), Bytes::from_static(b"0123")).await.unwrap();
    assert_eq!(&s.get_range(&k("f"), 0..4).await.unwrap()[..], b"0123");
}

#[tokio::test]
async fn get_tag_reports_the_current_tag_and_none_when_absent() {
    // The rebase step of the commit protocol. Returning None always would make every
    // commit loop give up, and returning a stale tag would break fencing.
    let s = MemoryStore::new();
    assert!(s.get_tag(&k("t")).await.is_none());
    let first = s.put(&k("t"), Bytes::from_static(b"1")).await.unwrap();
    assert_eq!(s.get_tag(&k("t")).await.as_ref(), Some(&first.tag));
    let second = s.put(&k("t"), Bytes::from_static(b"2")).await.unwrap();
    assert_eq!(s.get_tag(&k("t")).await.as_ref(), Some(&second.tag));
    assert_ne!(first.tag, second.tag);
}

#[tokio::test]
async fn a_key_displays_as_the_path_it_will_be_fetched_from() {
    // Keys appear in error messages and logs; a Display that dropped the path would make
    // every "no such key" report unactionable.
    assert_eq!(k("h/idx/1/HEAD").to_string(), "h/idx/1/HEAD");
    assert_eq!(k("h/idx/1/HEAD").as_str(), "h/idx/1/HEAD");
}
