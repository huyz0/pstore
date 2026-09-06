//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M1.10 — the milestone's exit condition, exercised as one flow: write, read, filter,
//! exact-search a single index, with the request cost asserted throughout.
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::{Document, Filter, Value};
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn product(i: i64) -> Document {
    let mut d = Document::new(
        format!("sku-{i:04}"),
        vec![(i % 100) as f32, (i / 100) as f32, 1.0],
    );
    d.attrs.insert("price".to_owned(), Value::Int(i * 7 % 500));
    d.attrs
        .insert("brand".to_owned(), Value::Str(format!("b{}", i % 5)));
    d
}

#[tokio::test]
async fn write_read_filter_and_search_one_index() {
    let counted = Accounted::new(MemoryStore::new());
    let t = TenantId(100);
    let e = Engine::new(Arc::new(counted.as_tenant(t)), t, LaneId(1));

    // Write in three batches, folding between them, so the index ends up with several
    // segments AND unfolded rows -- the shape a real index is in most of the time.
    for batch in 0..3i64 {
        let docs: Vec<_> = (batch * 300..(batch + 1) * 300).map(product).collect();
        e.write("catalog", docs).await.unwrap();
        e.flush().await.unwrap();
        if batch < 2 {
            e.fold().await.unwrap();
        }
    }
    assert_eq!(counted.count(t, OpClass::List), 0, "nothing may LIST");

    // Read everything: 600 folded rows plus 300 still in the memtable.
    let all = e.scan("catalog", None).await.unwrap();
    assert_eq!(all.len(), 900);

    // Filter, and check it against a reference computed from the same rows.
    let f = Filter::Gt("price".to_owned(), 400);
    let filtered = e.scan("catalog", Some(&f)).await.unwrap();
    let reference: Vec<_> = all.iter().filter(|d| f.matches(d)).collect();
    assert_eq!(filtered.len(), reference.len());
    assert!(
        !filtered.is_empty(),
        "the filter must actually select something"
    );

    // Exact search, filtered, verified against brute force over the same set.
    let q = [50.0f32, 4.0, 1.0];
    let hits = e.search("catalog", &q, 5, Some(&f)).await.unwrap();
    let mut want: Vec<(String, f32)> = reference
        .iter()
        .map(|d| {
            let dist: f32 = d
                .vector()
                .iter()
                .zip(&q)
                .map(|(a, b)| (a - b) * (a - b))
                .sum();
            (d.id.clone(), dist)
        })
        .collect();
    want.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    want.truncate(5);
    assert_eq!(
        hits.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        want.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        "filtered search disagrees with brute force"
    );
}

#[tokio::test]
async fn the_whole_flow_stays_inside_its_request_budget() {
    // The milestone's exit condition as one assertion: RA(write) = 1 W, and a cold read
    // is at most three sequential round trips however the index is laid out.
    let s = Arc::new(DepthCounting::new(MemoryStore::new()));
    let t = TenantId(101);
    let e = Engine::new(Arc::clone(&s), t, LaneId(1));

    for batch in 0..4i64 {
        e.write(
            "catalog",
            (batch * 100..(batch + 1) * 100).map(product).collect(),
        )
        .await
        .unwrap();
        s.reset();
        e.flush().await.unwrap();
        // RA(write batch) = 1 PUT, regardless of how many rows or indexes it carries.
        // The lane's FIRST flush also registers the lane -- a read of the registry and a
        // CAS to publish -- which is one-off per lane lifetime, not per batch. Stating it
        // rather than resetting the counter after it keeps the cost visible.
        assert_eq!(
            s.requests(),
            if batch == 0 { 3 } else { 1 },
            "a batch cost {} requests",
            s.requests()
        );
        e.fold().await.unwrap();
    }

    for (label, depth) in [
        ("scan", {
            s.reset();
            e.scan("catalog", None).await.unwrap();
            s.depth()
        }),
        ("filtered scan", {
            s.reset();
            e.scan("catalog", Some(&Filter::Lt("price".to_owned(), 50)))
                .await
                .unwrap();
            s.depth()
        }),
        ("search", {
            s.reset();
            e.search("catalog", &[1.0, 1.0, 1.0], 10, None)
                .await
                .unwrap();
            s.depth()
        }),
    ] {
        assert!(
            depth <= 3,
            "{label} took {depth} sequential round trips over 4 segments"
        );
    }
}
