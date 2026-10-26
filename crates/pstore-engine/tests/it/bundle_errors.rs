//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Malformed WAL bundles must fail the fold. Folding what happens to decode and dropping
//! the rest reports success while losing acknowledged writes.
use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_engine::Engine;
use pstore_format::{Document, Value, decode_docs, encode_docs};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![n as f32]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d.attrs.insert("s".to_owned(), Value::Str(format!("v{n}")));
    d
}

fn lane_key(t: TenantId, lane: u64, seq: u64) -> Key {
    Key::new(format!(
        "{:04x}/wal/{}/{:016x}/{:016}.bundle",
        t.0 as u16, t.0, lane, seq
    ))
}

#[tokio::test]
async fn documents_round_trip_through_the_shared_codec() {
    // One codec for segments and bundles, so the two cannot disagree about what a
    // document is. Both value types, and an empty run.
    let docs = vec![doc("a", 1), doc("b", -2), Document::new("c", vec![])];
    assert_eq!(decode_docs(&encode_docs(&docs)).unwrap(), docs);
    assert!(decode_docs(&encode_docs(&[])).unwrap().is_empty());
}

#[tokio::test]
async fn a_truncated_document_run_is_refused_at_every_length() {
    let bytes = encode_docs(&[doc("a", 1), doc("bb", 2)]);
    for cut in 0..bytes.len() {
        assert!(
            decode_docs(&bytes[..cut]).is_err(),
            "decoded a run cut to {cut}"
        );
    }
}

#[tokio::test]
async fn an_unknown_value_tag_in_a_document_run_is_refused() {
    let mut bytes = encode_docs(&[doc("a", 1)]);
    // The first attribute's type tag, after count, id and the empty-ish vector header.
    let at = bytes.iter().position(|&b| b == 0).unwrap_or(0);
    bytes[at] = 250;
    let _ = decode_docs(&bytes);
    // Whether it fails on the tag or an earlier field, it must not yield a document
    // claiming an attribute type nothing wrote.
    if let Ok(got) = decode_docs(&bytes) {
        assert!(got.iter().all(|d| {
            d.attrs
                .values()
                .all(|v| matches!(v, Value::Int(_) | Value::Str(_)))
        }));
    }
}

#[tokio::test]
async fn a_bundle_with_a_mangled_footer_fails_the_fold() {
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(40);
    let e = Engine::new(Arc::clone(&s), t, LaneId(1));
    e.write("idx", vec![doc("a", 1)]).await.unwrap();
    let seq = e.flush().await.unwrap().unwrap();
    let key = lane_key(t, 1, seq.0);

    let good = s.get(&key).await.unwrap();
    let mut bad = good.to_vec();
    let n = bad.len();
    bad[n - 2] ^= 0xFF; // trailing magic
    s.put(&key, bytes::Bytes::from(bad)).await.unwrap();
    assert!(e.fold().await.is_err());
}

#[tokio::test]
async fn a_bundle_with_an_impossible_index_offset_fails_the_fold() {
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(41);
    let e = Engine::new(Arc::clone(&s), t, LaneId(1));
    e.write("idx", vec![doc("a", 1)]).await.unwrap();
    let seq = e.flush().await.unwrap().unwrap();
    let key = lane_key(t, 1, seq.0);

    let mut bad = s.get(&key).await.unwrap().to_vec();
    let n = bad.len();
    // index_offset sits right after the leading magic of the footer.
    let at = n - (8 + 8 + 4 + 8) + 8;
    bad[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    s.put(&key, bytes::Bytes::from(bad)).await.unwrap();
    assert!(
        e.fold().await.is_err(),
        "an offset past the object must not be trusted"
    );
}

#[tokio::test]
async fn a_hole_in_a_lane_bounds_the_tail_rather_than_being_skipped() {
    // Lanes are dense by construction, so a missing object is a HOLE, not an end. Counting
    // past it would silently drop every bundle before the next present key while reporting
    // success; stopping at it means the rows before the hole are still folded and the ones
    // after are simply not yet visible.
    use pstore_engine::lanes;
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(42);
    let e = Engine::new(Arc::clone(&s), t, LaneId(1));
    for i in 0..4 {
        e.write("idx", vec![doc(&format!("d{i}"), i)])
            .await
            .unwrap();
        e.flush().await.unwrap();
    }
    // Remove the second bundle, as a torn upload or an over-eager reaper would.
    s.delete_batch(&[lane_key(t, 1, 1)]).await.unwrap();
    assert_eq!(
        lanes::tail(&*s, t, LaneId(1), 0).await.unwrap(),
        1,
        "the tail is the hole"
    );

    e.fold().await.unwrap();
    let got = e.scan("idx", None).await.unwrap();
    assert!(
        got.iter().any(|d| d.id == "d0"),
        "rows before the hole must still be folded"
    );
}

#[tokio::test]
async fn folding_nothing_is_a_no_op_not_an_error() {
    let s = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&s), TenantId(43), LaneId(1));
    assert_eq!(e.fold().await.unwrap(), pstore_types::Epoch::ZERO);
    // And flushing nothing writes nothing.
    assert!(e.flush().await.unwrap().is_none());
}
