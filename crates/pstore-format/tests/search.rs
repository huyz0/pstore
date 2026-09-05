//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Zone maps must prune, and must never prune a block that could match. Exact search must
//! agree with an independent brute-force reference, because "approximately right" is a
//! recall bug wearing a disguise.
use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::{Document, Filter, Segment, SegmentWriter, Value};
use pstore_types::TenantId;

fn build(n: usize, rows_per_block: usize) -> bytes::Bytes {
    let mut w = SegmentWriter::new(rows_per_block);
    for i in 0..n {
        let mut d = Document::new(format!("d{i}"), vec![i as f32, 1.0, 0.5]);
        d.attrs.insert("n".to_owned(), Value::Int(i as i64));
        d.attrs
            .insert("kind".to_owned(), Value::Str(format!("k{}", i % 4)));
        w.push(d);
    }
    w.finish()
}

#[tokio::test]
async fn zone_maps_skip_blocks_that_cannot_match() {
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(1);
    let v = s.as_tenant(t);
    let key = Key::new("seg");
    v.put(&key, build(400, 10)).await.unwrap();
    let seg = Segment::open(&v, &key).await.unwrap();
    assert_eq!(seg.block_count(), 40);

    // `n` is dense and ascending, so one block of ten holds n in 50..=59.
    let narrow = seg.blocks_to_read(Some(&Filter::Eq("n".to_owned(), Value::Int(55))));
    assert_eq!(narrow.len(), 1, "one block can contain n == 55");
    assert_eq!(seg.blocks_to_read(None).len(), 40);

    // ...and the saving is real, not just planned.
    let before = s.count(t, OpClass::Read);
    let got = seg
        .scan(&v, &key, Some(&Filter::Eq("n".to_owned(), Value::Int(55))))
        .await
        .unwrap();
    assert_eq!(s.count(t, OpClass::Read) - before, 1, "one coalesced round");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, "d55");
}

#[tokio::test]
async fn pruning_does_not_change_the_answer() {
    // The dangerous mutation is not "prunes too little" -- that is slow. It is "prunes a
    // block that could match", which silently drops rows and reads as a recall bug.
    let s = MemoryStore::new();
    let key = Key::new("seg");
    s.put(&key, build(300, 7)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let all = seg.scan(&s, &key, None).await.unwrap();

    for f in [
        Filter::Gt("n".to_owned(), 250),
        Filter::Lt("n".to_owned(), 40),
        Filter::Eq("n".to_owned(), Value::Int(0)),
        Filter::Eq("n".to_owned(), Value::Int(299)),
        Filter::Gt("n".to_owned(), 298),
        Filter::Lt("n".to_owned(), 1),
    ] {
        let pruned = seg.scan(&s, &key, Some(&f)).await.unwrap();
        let reference: Vec<_> = all.iter().filter(|d| f.matches(d)).cloned().collect();
        assert_eq!(pruned, reference, "pruning changed the answer for {f:?}");
    }
}

#[tokio::test]
async fn a_string_filter_cannot_prune_but_still_filters() {
    // No ordering, so every block must be read -- and the answer must still be exact.
    let s = MemoryStore::new();
    let key = Key::new("seg");
    s.put(&key, build(80, 8)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let f = Filter::Eq("kind".to_owned(), Value::Str("k2".to_owned()));
    assert_eq!(
        seg.blocks_to_read(Some(&f)).len(),
        10,
        "strings cannot prune"
    );
    let got = seg.scan(&s, &key, Some(&f)).await.unwrap();
    assert_eq!(got.len(), 20);
    assert!(
        got.iter()
            .all(|d| d.attrs.get("kind") == Some(&Value::Str("k2".to_owned())))
    );
}

#[tokio::test]
async fn a_filter_on_an_unknown_column_matches_nothing_and_prunes_nothing() {
    // Pruning on a column with no zone map must keep every block: it is absence of
    // information, not evidence of absence.
    let s = MemoryStore::new();
    let key = Key::new("seg");
    s.put(&key, build(40, 8)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let f = Filter::Gt("nope".to_owned(), 0);
    assert_eq!(seg.blocks_to_read(Some(&f)).len(), 5);
    assert!(seg.scan(&s, &key, Some(&f)).await.unwrap().is_empty());
}

#[tokio::test]
async fn exact_search_matches_a_brute_force_reference() {
    let s = MemoryStore::new();
    let key = Key::new("seg");
    let mut w = SegmentWriter::new(16);
    // Deterministic pseudo-random vectors: a fixed seed so a failure replays.
    let mut x = 12_345u64;
    let mut rnd = || {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((x >> 33) as f32) / (u32::MAX as f32)
    };
    let mut expect: Vec<(String, Vec<f32>)> = Vec::new();
    for i in 0..500 {
        let v = vec![rnd(), rnd(), rnd(), rnd()];
        expect.push((format!("d{i}"), v.clone()));
        w.push(Document::new(format!("d{i}"), v));
    }
    s.put(&key, w.finish()).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();

    for q in [
        vec![0.1, 0.2, 0.3, 0.4],
        vec![0.9, 0.9, 0.1, 0.0],
        vec![0.0; 4],
    ] {
        let got = seg.search(&s, &key, &q, 10, None).await.unwrap();
        // Independent reference: computed here, from the inputs, not from the segment.
        let mut reference: Vec<(String, f32)> = expect
            .iter()
            .map(|(id, v)| {
                let d: f32 = v.iter().zip(&q).map(|(a, b)| (a - b) * (a - b)).sum();
                (id.clone(), d)
            })
            .collect();
        reference.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        reference.truncate(10);
        let got_ids: Vec<_> = got.iter().map(|(id, _)| id.clone()).collect();
        let ref_ids: Vec<_> = reference.iter().map(|(id, _)| id.clone()).collect();
        assert_eq!(
            got_ids, ref_ids,
            "top-10 disagrees with brute force for {q:?}"
        );
    }
}

#[tokio::test]
async fn search_returns_k_results_when_k_exceeds_the_segment() {
    let s = MemoryStore::new();
    let key = Key::new("seg");
    s.put(&key, build(3, 8)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    assert_eq!(
        seg.search(&s, &key, &[0.0, 0.0, 0.0], 100, None)
            .await
            .unwrap()
            .len(),
        3
    );
    assert!(
        seg.search(&s, &key, &[0.0, 0.0, 0.0], 0, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn search_respects_a_filter() {
    // Pre-filter composing with the scan: the filtered top-k must be the top-k OF the
    // filtered set, not the filtered top-k of everything.
    let s = MemoryStore::new();
    let key = Key::new("seg");
    s.put(&key, build(200, 10)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    let f = Filter::Gt("n".to_owned(), 150);
    let got = seg
        .search(&s, &key, &[0.0, 0.0, 0.0], 5, Some(&f))
        .await
        .unwrap();
    assert_eq!(got.len(), 5);
    // Nearest to the origin among n > 150 is n = 151.
    assert_eq!(got[0].0, "d151");
    assert!(
        got.iter()
            .all(|(id, _)| id.trim_start_matches('d').parse::<i64>().unwrap() > 150)
    );
}

#[tokio::test]
async fn search_on_a_dimension_mismatch_is_an_error() {
    let s = MemoryStore::new();
    let key = Key::new("seg");
    s.put(&key, build(8, 4)).await.unwrap();
    let seg = Segment::open(&s, &key).await.unwrap();
    assert!(seg.search(&s, &key, &[0.0, 0.0], 3, None).await.is_err());
}
