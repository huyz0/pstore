//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The write path's economic claim and the commit protocol's correctness claim, both
//! asserted by counters and by concurrency rather than by reading the code.
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::{Document, Filter, Value};
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

fn doc(id: &str, n: i64) -> Document {
    let mut d = Document::new(id, vec![n as f32, 1.0]);
    d.attrs.insert("n".to_owned(), Value::Int(n));
    d
}

#[tokio::test]
async fn a_write_batch_costs_exactly_one_put() {
    // RA(write batch) = 1 W, whatever the batch holds. One PUT per document is a
    // 10^6-times cost blowup and would pass every functional test.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(1);
    let e = Engine::new(Arc::new(s.as_tenant(t)), t, pstore_types::LaneId(1));

    // Register the lane first, so the loop measures the batch rather than the one-off.
    e.write("warm", vec![doc("w", 0)]).await.unwrap();
    e.flush().await.unwrap();

    for size in [1usize, 10, 1000] {
        let before = s.count(t, OpClass::Write);
        let docs: Vec<_> = (0..size).map(|i| doc(&format!("d{i}"), i as i64)).collect();
        e.write("idx", docs).await.unwrap();
        e.flush().await.unwrap();
        assert_eq!(
            s.count(t, OpClass::Write) - before,
            1,
            "a batch of {size} cost more than one PUT"
        );
    }
}

#[tokio::test]
async fn a_lane_registration_costs_one_cas_for_the_lane_not_one_per_batch() {
    // The registration is real, so it is measured rather than waved away: two writes on
    // the first flush, one on every flush after.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(30);
    let e = Engine::new(Arc::new(s.as_tenant(t)), t, pstore_types::LaneId(1));
    e.write("idx", vec![doc("a", 1)]).await.unwrap();
    let before = s.count(t, OpClass::Write);
    e.flush().await.unwrap();
    assert_eq!(
        s.count(t, OpClass::Write) - before,
        2,
        "bundle plus the registration CAS"
    );

    for _ in 0..5 {
        e.write("idx", vec![doc("b", 2)]).await.unwrap();
        let before = s.count(t, OpClass::Write);
        e.flush().await.unwrap();
        assert_eq!(
            s.count(t, OpClass::Write) - before,
            1,
            "later flushes are one PUT"
        );
    }
}

#[tokio::test]
async fn many_indexes_share_one_bundle() {
    // The finding that per-index flushing has a cost floor independent of data volume:
    // one bundle per index restores it. Fifty indexes, one window, one PUT.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(2);
    let e = Engine::new(Arc::new(s.as_tenant(t)), t, pstore_types::LaneId(1));
    for i in 0..50 {
        e.write(&format!("idx{i}"), vec![doc("d0", i)])
            .await
            .unwrap();
    }
    // Flush once first so the lane is registered: registration is one CAS per lane
    // lifetime, and this test is about the per-batch cost of fifty indexes.
    e.flush().await.unwrap();
    for i in 0..50 {
        e.write(&format!("idx{i}"), vec![doc("d1", i)])
            .await
            .unwrap();
    }
    let before = s.count(t, OpClass::Write);
    e.flush().await.unwrap();
    assert_eq!(
        s.count(t, OpClass::Write) - before,
        1,
        "fifty indexes, one bundle"
    );
}

#[tokio::test]
async fn a_bundle_reader_gets_only_its_own_index() {
    // Cross-tenant bundling is only safe if a reader can address its own slice. Returning
    // a neighbour's rows would be a data-leak bug wearing the costume of a count error.
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(3);
    let e = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(1));
    e.write("alpha", vec![doc("a1", 1), doc("a2", 2)])
        .await
        .unwrap();
    e.write("beta", vec![doc("b1", 3)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let alpha = e.scan("alpha", None).await.unwrap();
    let beta = e.scan("beta", None).await.unwrap();
    assert_eq!(
        alpha.iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
        ["a1", "a2"]
    );
    assert_eq!(
        beta.iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
        ["b1"]
    );
}

#[tokio::test]
async fn a_write_is_visible_before_it_is_folded() {
    // The freshness layer's whole claim: visibility does not wait on the fold, so the
    // flush interval never enters the time-to-searchable budget.
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(4);
    let e = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(1));
    e.write("idx", vec![doc("fresh", 1)]).await.unwrap();
    assert_eq!(
        e.scan("idx", None).await.unwrap().len(),
        1,
        "visible before flush"
    );
    e.flush().await.unwrap();
    assert_eq!(
        e.scan("idx", None).await.unwrap().len(),
        1,
        "visible after flush"
    );
    assert_eq!(e.epoch(), Epoch::ZERO, "and nothing has been committed yet");
}

#[tokio::test]
async fn a_folded_write_is_still_visible_exactly_once() {
    // The hazard is a document visible TWICE -- from the memtable and from the segment --
    // which reads as a duplicate rather than an error.
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(5);
    let e = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(1));
    e.write("idx", vec![doc("a", 1), doc("b", 2)])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let got = e.scan("idx", None).await.unwrap();
    assert_eq!(got.len(), 2, "folded rows must appear once, not twice");
    let mut ids: Vec<_> = got.iter().map(|d| d.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, ["a", "b"]);
    assert!(e.epoch() > Epoch::ZERO, "a fold commits");
}

#[tokio::test]
async fn writes_after_a_fold_are_visible_alongside_folded_ones() {
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(6);
    let e = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(1));
    e.write("idx", vec![doc("old", 1)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("idx", vec![doc("new", 2)]).await.unwrap();

    let mut ids: Vec<_> = e
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    ids.sort();
    assert_eq!(ids, ["new", "old"]);
}

#[tokio::test]
async fn a_stale_committer_is_fenced() {
    // The fencing property, which is what makes locks unnecessary. A committer holding a
    // superseded tag must be refused however long it was paused for.
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(7);
    let a = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(1));
    let b = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(2));

    a.write("idx", vec![doc("a", 1)]).await.unwrap();
    a.flush().await.unwrap();
    a.fold().await.unwrap();

    // `b` still holds the pre-commit view. Its commit must be refused, not silently win.
    assert!(
        b.commit_stale_for_test().await.is_err(),
        "a stale tag must be fenced out"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_committers_lose_and_duplicate_nothing() {
    // Linearizability: every commit either applies or rebases, and the epochs it produces
    // are strictly increasing with no gaps and no repeats. A lost update shows up as a
    // gap; a double-apply shows up as a repeat.
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(8);
    let mut tasks = Vec::new();
    for w in 0..8u64 {
        let s = Arc::clone(&s);
        tasks.push(tokio::spawn(async move {
            let e = Engine::new(s, t, pstore_types::LaneId(w));
            let mut got = Vec::new();
            for c in 0..4 {
                e.write("idx", vec![doc(&format!("w{w}c{c}"), c)])
                    .await
                    .unwrap();
                e.flush().await.unwrap();
                got.push(e.fold().await.unwrap());
            }
            got
        }));
    }
    let mut epochs: Vec<Epoch> = Vec::new();
    for t in tasks {
        epochs.extend(t.await.unwrap());
    }
    epochs.sort();
    assert_eq!(epochs.len(), 32);
    // ⚠️ This assertion CHANGED in M2.4 and the reason matters. When `fold` was
    // lane-scoped, each of the 32 folds necessarily produced a distinct epoch, and
    // counting them was a cheap proxy for "no commit was lost". `fold` is now
    // tenant-scoped: a writer folds every live lane's un-folded bundles, so a concurrent
    // fold legitimately finds nothing left to do and returns the current epoch
    // unchanged. That collapse is the FEATURE -- it is what lets a successor fold a dead
    // writer's lane -- so demanding 32 distinct epochs would now be demanding wasted
    // commits.
    //
    // The property the old assertion was protecting is asserted directly below, and more
    // strongly: every acknowledged row is visible exactly once. A lost update loses a
    // row; a double-apply duplicates one. Neither can hide behind an epoch count.
    let uniq: std::collections::BTreeSet<_> = epochs.iter().collect();
    assert!(
        uniq.len() > 1,
        "no commit made progress at all: {} epochs",
        uniq.len()
    );
    // Epochs still advance by exactly one per COMMIT, with no gaps: a gap would mean a
    // number was consumed without a manifest behind it, which is what a torn commit or a
    // double increment looks like. What changed is only that a fold may return an epoch
    // it did not create.
    let mut ordered: Vec<u64> = uniq.iter().map(|e| e.0).collect();
    ordered.sort_unstable();
    for (i, e) in ordered.iter().enumerate() {
        assert_eq!(*e, (i + 1) as u64, "epoch sequence has a gap at {i}");
    }

    // And every document survived: 8 writers x 4 commits.
    let seen = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(99))
        .scan("idx", None)
        .await
        .unwrap();
    let ids: Vec<&str> = seen.iter().map(|d| d.id.as_str()).collect();
    let distinct: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(
        distinct.len(),
        32,
        "a commit was LOST: {} of 32 rows are visible",
        distinct.len()
    );
    assert_eq!(
        ids.len(),
        32,
        "a commit was applied TWICE: {} rows for 32 writes",
        ids.len()
    );
}

#[tokio::test]
async fn search_and_filter_work_across_the_memtable_and_segments() {
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(9);
    let e = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(1));
    e.write("idx", (0..20).map(|i| doc(&format!("d{i}"), i)).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // Fresh, unfolded rows alongside committed ones.
    e.write("idx", (20..30).map(|i| doc(&format!("d{i}"), i)).collect())
        .await
        .unwrap();

    assert_eq!(e.scan("idx", None).await.unwrap().len(), 30);
    let filtered = e
        .scan("idx", Some(&Filter::Gt("n".to_owned(), 24)))
        .await
        .unwrap();
    assert_eq!(filtered.len(), 5, "the filter must reach unfolded rows too");

    let hits = e.search("idx", &[25.0, 1.0], 3, None).await.unwrap();
    assert_eq!(hits[0].0, "d25", "nearest must be found wherever it lives");
}

#[tokio::test]
async fn a_fresh_engine_recovers_acknowledged_writes_from_the_wal() {
    // The property that makes the WAL a WAL rather than a write-only cost. The writer
    // acknowledged these rows and then vanished with its memtable; a replacement holding
    // nothing in memory must still be able to fold them.
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(20);
    {
        let dead = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(1));
        dead.write("idx", vec![doc("a", 1), doc("b", 2)])
            .await
            .unwrap();
        dead.flush().await.unwrap();
        dead.write("other", vec![doc("c", 3)]).await.unwrap();
        dead.flush().await.unwrap();
        // No fold. The process is gone, and so is everything it held in memory.
    }

    // A different lane, holding nothing in memory and told nothing: it must discover the
    // dead writer's lane from the registry and its tail by probing.
    let successor = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(2));
    successor.fold().await.unwrap();

    let idx: Vec<_> = successor.scan("idx", None).await.unwrap();
    let other: Vec<_> = successor.scan("other", None).await.unwrap();
    assert_eq!(idx.len(), 2, "acknowledged rows must survive the writer");
    assert_eq!(other.len(), 1);
    // And a fresh reader with no memtable at all sees them, because they are committed.
    let reader = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(99));
    assert_eq!(reader.scan("idx", None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn folding_twice_does_not_duplicate_rows() {
    // The watermark is what makes a fold idempotent. Without it a retry -- or a second
    // folder -- replays the same bundles and the rows appear twice.
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(21);
    let e = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(1));
    e.write("idx", vec![doc("a", 1)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.fold().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(e.scan("idx", None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_corrupt_bundle_fails_the_fold_rather_than_losing_rows() {
    // Silently folding what decoded and dropping the rest would report success while
    // losing acknowledged writes.
    use pstore_blob::{BlobStore, Key};
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(22);
    let e = Engine::new(Arc::clone(&s), t, pstore_types::LaneId(1));
    e.write("idx", vec![doc("a", 1)]).await.unwrap();
    let seq = e.flush().await.unwrap().unwrap();

    let key = Key::new(format!(
        "{:04x}/wal/{}/{:016x}/{:016}.bundle",
        t.0 as u16, t.0, 1u64, seq.0
    ));
    let good = s.get(&key).await.unwrap();
    s.put(&key, good.slice(..good.len() - 5)).await.unwrap();
    assert!(
        e.fold().await.is_err(),
        "a truncated bundle must fail the fold"
    );
}

#[tokio::test]
async fn a_write_the_format_cannot_store_is_refused_at_the_door() {
    // ⚠️ Refused on `write`, not discovered at `fold`. A document accepted here and found
    // unstorable later is a write the caller believes is durable and which never appears.
    //
    // ⚠️ The example has moved twice, and the movement is the point. It was "any named
    // field" until M3b.3 stored those, then "any sparse field" until M5a.2 stored one. What
    // is left is a SECOND sparse field: the segment addresses postings through a single
    // section id, so the writer would take the first field in name order and write the
    // second as nothing. Narrowing this check has always meant making the loss impossible,
    // never making the check quieter.
    let s = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&s), TenantId(77), pstore_types::LaneId(0));

    let mut two = pstore_format::Document::new("d", vec![1.0]);
    for name in ["s1", "s2"] {
        two.vectors.insert(
            name.to_owned(),
            pstore_format::VectorField::Sparse(vec![(1, pstore_format::Impact::new(0.5))]),
        );
    }
    assert!(
        e.write("idx", vec![two]).await.is_err(),
        "a second sparse field was accepted, and it has no section id to be stored in"
    );

    // Nothing was buffered by a refused write: a rejected batch must not half-land.
    assert!(e.pending_for_test().await.is_empty());

    // One sparse field is stored now, not refused — M5a.2.
    let mut one = pstore_format::Document::new("d2", vec![1.0]);
    one.vectors.insert(
        "s1".to_owned(),
        pstore_format::VectorField::Sparse(vec![(1, pstore_format::Impact::new(0.5))]),
    );
    e.write("idx", vec![one]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(e.scan("idx", None).await.unwrap().len(), 1);

    e.write("idx", vec![doc("ok", 1)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(e.scan("idx", None).await.unwrap().len(), 2);
}
