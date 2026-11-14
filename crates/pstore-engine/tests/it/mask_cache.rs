//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M234: a filtered query answers the same with the server's filter-mask cache as without,
//! and the second query of a filter reads fewer bytes with it -- unless the index overrides
//! `filter_cache` off.

use pstore_blob::{Accounted, MemoryStore, OpClass, TenantView};
use pstore_engine::{Engine, Storage};
use pstore_format::{Document, Value};
use pstore_index::vec_index::Query;
use pstore_query::masks::MaskCache;
use pstore_query::{Fusion, Op, Predicate, Prefetch};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(234);

type E = Engine<TenantView<MemoryStore>>;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn doc(i: usize) -> Document {
    let a = i as f32 / 37.0;
    let mut d = Document::new(format!("d{i:05}"), vec![a.cos(), a.sin()]);
    d.attrs
        .insert("q".to_owned(), Value::Int((i * 7 % 41) as i64 - 10));
    d.attrs
        .insert("s".to_owned(), Value::Str(format!("s{}", i % 5)));
    d.attrs.insert(
        "title".to_owned(),
        Value::Str(format!("title {i} {}", "y".repeat(i % 11))),
    );
    d
}

/// An engine over its own store, with or without a cache, given three segments, deletes in
/// two, and a few rows unfolded.
async fn engine(cache: Option<Arc<MaskCache>>) -> (Accounted<MemoryStore>, E) {
    let store = Accounted::new(MemoryStore::new());
    let e = Engine::new(Arc::new(store.as_tenant(T)), T, LaneId(1)).with_mask_cache(cache);
    for k in 0..3 {
        e.write("idx", (k * 400..(k + 1) * 400).map(doc).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.delete("idx", vec!["d00011".to_owned(), "d00444".to_owned()])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("idx", (1200..1210).map(doc).collect())
        .await
        .unwrap();
    (store, e)
}

fn dense(angle: f32) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![angle.cos(), angle.sin()],
        limit: 10,
        tune: Query::default(),
    }]
}

fn cmp(r: &mut Rng) -> Predicate {
    let (name, want) = match r.below(3) {
        0 => ("q", Value::Int(r.below(50) as i64 - 15)),
        1 => ("s", Value::Str(format!("s{}", r.below(6)))),
        _ => ("id", Value::Str(format!("d{:05}", r.below(1_300)))),
    };
    let op = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte][r.below(5) as usize];
    Predicate::Cmp(name.to_owned(), op, want)
}

fn predicate(r: &mut Rng) -> Predicate {
    match r.below(4) {
        0 => Predicate::And(vec![cmp(r), cmp(r)]),
        1 => Predicate::Or(vec![cmp(r), cmp(r)]),
        2 => Predicate::Not(Box::new(cmp(r))),
        _ => cmp(r),
    }
}

/// The ids `e` answers, and the bytes it read for them.
async fn ask(
    store: &Accounted<MemoryStore>,
    e: &E,
    angle: f32,
    filter: &Predicate,
) -> (Vec<Option<String>>, u64) {
    let before = store.bytes(T, OpClass::Read);
    let got = e
        .query_filtered("idx", &dense(angle), Some(filter), Fusion::default(), 10)
        .await
        .unwrap();
    (got.ids, store.bytes(T, OpClass::Read) - before)
}

#[tokio::test]
async fn a_cached_mask_answers_as_a_computed_one() {
    let cache = Arc::new(MaskCache::new(16 << 20));
    let (cs, cached) = engine(Some(Arc::clone(&cache))).await;
    let (ps, plain) = engine(None).await;
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut saved, mut kept) = (0u64, 0usize);
    for n in 0..200 {
        let f = predicate(&mut r);
        let (angle, again) = (n as f32 / 9.0, n as f32 / 9.0 + 1.3);
        let (a1, _) = ask(&cs, &cached, angle, &f).await;
        let (b1, _) = ask(&ps, &plain, angle, &f).await;
        assert_eq!(a1, b1, "{f:?}");
        // The same filter, another vector: the masks are the cache's now.
        let (a2, with) = ask(&cs, &cached, again, &f).await;
        let (b2, without) = ask(&ps, &plain, again, &f).await;
        assert_eq!(a2, b2, "again: {f:?}");
        assert!(with <= without, "{f:?}: {with} bytes cached, {without} not");
        saved += without - with;
        kept = cache.len();
    }
    assert!(saved > 0, "no second query read less");
    assert!(kept > 0 && cache.held() <= cache.budget());

    // A filter that keeps every block: the second query reads no block for its masks.
    let every = Predicate::Cmp("q".to_owned(), Op::Gte, Value::Int(-100));
    let (_, first) = ask(&cs, &cached, 0.5, &every).await;
    let (_, second) = ask(&cs, &cached, 2.5, &every).await;
    let (_, plain_second) = ask(&ps, &plain, 2.5, &every).await;
    assert!(
        second < first && second < plain_second,
        "{first} then {second}, plain {plain_second}"
    );

    // The index overrides the cache off: the same shape of query reads as an uncached one.
    cached
        .set_storage("idx", |s| Storage {
            filter_cache: Some(false),
            ..s
        })
        .await
        .unwrap()
        .unwrap();
    let off = Predicate::Cmp("q".to_owned(), Op::Lt, Value::Int(7));
    let held = cache.len();
    let (a1, first) = ask(&cs, &cached, 0.1, &off).await;
    let (a2, again) = ask(&cs, &cached, 0.1, &off).await;
    let (b1, _) = ask(&ps, &plain, 0.1, &off).await;
    assert_eq!((&a1, &a2), (&b1, &b1));
    // Its HEAD now holds the override, a few bytes more than the plain one's: compared with
    // its own first query, the same query again reads every block again.
    assert_eq!(again, first, "an index that opts out still read from the cache");
    assert_eq!(
        cache.len(),
        held,
        "an index that opts out still filled the cache"
    );
}
