//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The segment is the unit of immutable storage, and its footer contract is what keeps a
//! cold open at two round trips rather than however many a reader needs to feel its way in.
use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::{Document, Segment, SegmentWriter, Value};
use pstore_types::TenantId;

fn doc(id: &str, v: &[f32], n: i64) -> Document {
    let mut d = Document::new(id, v.to_vec());
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d.attrs
        .insert("tag".to_owned(), Value::Str(format!("t{}", n % 3)));
    d
}

fn write_docs(n: usize, rows_per_block: usize) -> bytes::Bytes {
    let mut w = SegmentWriter::new(rows_per_block);
    for i in 0..n {
        w.push(doc(
            &format!("d{i}"),
            &[i as f32, (i * 2) as f32, 1.0],
            i as i64,
        ));
    }
    w.finish()
}

#[tokio::test]
async fn a_segment_round_trips_through_the_blob_store() {
    let s = MemoryStore::new();
    let key = Key::new("seg/1");
    s.put(&key, write_docs(50, 8)).await.unwrap();

    let seg = Segment::open(&s, &key).await.unwrap();
    assert_eq!(seg.row_count(), 50);
    let all = seg.scan(&s, &key, None).await.unwrap();
    assert_eq!(all.len(), 50);
    // Identity, not just count: dropping the last block or losing a field would still
    // give a plausible-looking answer.
    assert_eq!(all[0].id, "d0");
    assert_eq!(all[49].id, "d49");
    assert_eq!(all[7].vector(), vec![7.0, 14.0, 1.0]);
    assert_eq!(all[7].attrs.get("n"), Some(&Value::Int(7)));
    assert_eq!(all[7].attrs.get("tag"), Some(&Value::Str("t1".to_owned())));
}

#[tokio::test]
async fn a_segment_is_readable_from_its_key_alone() {
    // No side table, no listing, no length passed in. A reader that needed any of those
    // would put a lookup on a path this design promises has none.
    let s = MemoryStore::new();
    let key = Key::new("h/idx/t/s0/seg/L0/000-abc.seg");
    s.put(&key, write_docs(9, 4)).await.unwrap();
    assert_eq!(Segment::open(&s, &key).await.unwrap().row_count(), 9);
}

#[tokio::test]
async fn opening_a_cold_segment_costs_at_most_two_reads() {
    // Pattern 6: one suffix range for the footer, one for the index section. A reader that
    // pulled the whole object would pass every functional test above and cost a segment's
    // worth of bandwidth on every open.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(1);
    let v = s.as_tenant(t);
    let key = Key::new("seg/big");
    v.put(&key, write_docs(2000, 64)).await.unwrap();

    let before = s.count(t, OpClass::Read);
    Segment::open(&v, &key).await.unwrap();
    let reads = s.count(t, OpClass::Read) - before;
    assert!(reads <= 2, "cold open cost {reads} reads");
}

#[tokio::test]
async fn a_small_segment_opens_in_one_read() {
    // The suffix fetch is bigger than the footer on purpose, so a small segment's whole
    // index section arrives with it. Never using those spare bytes wastes a round trip on
    // the common case -- most indexes are small.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(2);
    let v = s.as_tenant(t);
    let key = Key::new("seg/small");
    v.put(&key, write_docs(3, 8)).await.unwrap();

    let before = s.count(t, OpClass::Read);
    let seg = Segment::open(&v, &key).await.unwrap();
    assert_eq!(s.count(t, OpClass::Read) - before, 1);
    assert_eq!(seg.row_count(), 3);
}

#[tokio::test]
async fn a_corrupt_footer_is_an_error_not_a_wrong_answer() {
    // A segment that decodes to garbage is worse than one that fails: the garbage is
    // served as data.
    let s = MemoryStore::new();
    let key = Key::new("seg/bad");
    s.put(&key, bytes::Bytes::from_static(b"not a segment at all"))
        .await
        .unwrap();
    assert!(Segment::open(&s, &key).await.is_err());

    let mut good = write_docs(4, 2).to_vec();
    let n = good.len();
    good[n - 3] ^= 0xFF;
    s.put(&Key::new("seg/tampered"), bytes::Bytes::from(good))
        .await
        .unwrap();
    assert!(Segment::open(&s, &Key::new("seg/tampered")).await.is_err());
}

#[tokio::test]
async fn an_empty_segment_is_legal() {
    let s = MemoryStore::new();
    let key = Key::new("seg/empty");
    s.put(&key, SegmentWriter::new(8).finish()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    assert_eq!(seg.row_count(), 0);
    assert!(seg.scan(&s, &key, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_segment_whose_index_section_overflows_the_suffix_takes_two_reads() {
    // The other branch of the footer contract, and until this test existed it was never
    // taken: earlier "big segment" tests had few enough blocks that the whole index
    // section still arrived with the footer. Many small blocks is what forces it.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(9);
    let v = s.as_tenant(t);
    let key = Key::new("seg/wide");
    // ⚠️ Built with the index budget lifted. The writer normally grows its block size
    // until the index fits the suffix read, which is what keeps a cold open to one round
    // trip -- so this branch of the READER cannot be reached by writing a normal segment
    // any more. It still has to work: M3 adds a centroid section that will not fit, and a
    // segment already on the store was written under a different budget. Opting out is
    // therefore what a test of the two-read path must do, and saying so here is cheaper
    // than someone later concluding the branch is dead.
    let mut w = SegmentWriter::new(1).with_index_budget(usize::MAX);
    for i in 0..4000 {
        w.push(doc(&format!("d{i}"), &[i as f32, 1.0, 2.0], i as i64));
    }
    v.put(&key, w.finish()).await.unwrap();

    let before = s.count(t, OpClass::Read);
    let seg = Segment::open(&v, &key).await.unwrap();
    let reads = s.count(t, OpClass::Read) - before;
    assert_eq!(
        reads, 2,
        "a large index section costs the second read, and only that"
    );
    assert_eq!(seg.row_count(), 4000);
    assert!(
        seg.block_count() > 1,
        "the tiny budget did not produce many blocks"
    );

    // And it must still be correct, not merely cheap.
    let all = seg.scan(&v, &key, None).await.unwrap();
    assert_eq!(all.len(), 4000);
    assert_eq!(all[3999].id, "d3999");
}
