//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M71.2: the multi-vector clusters are asked for only by an `fde` leg over a segment HEAD says
//! has them -- a dense query reads none, and a small segment is never asked (M27/M32's rule).

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::Engine;
use pstore_format::{Document, VectorField};
use pstore_query::{Fusion, Prefetch};
use pstore_types::{CasTag, LaneId, TenantId};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const T: TenantId = TenantId(7100);

/// Counts the reads of the clusters and of their vectors.
#[derive(Debug, Default)]
struct Counting {
    inner: MemoryStore,
    clusters: AtomicU64,
    vectors: AtomicU64,
}

impl Counting {
    fn saw(&self, key: &Key) {
        if key.as_str().ends_with(".fdec") {
            self.clusters.fetch_add(1, Ordering::SeqCst);
        }
        if key.as_str().ends_with(".fdev") {
            self.vectors.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// `(clusters, vectors)` reads so far.
    fn counts(&self) -> (u64, u64) {
        (
            self.clusters.load(Ordering::SeqCst),
            self.vectors.load(Ordering::SeqCst),
        )
    }
}

#[async_trait::async_trait]
impl BlobStore for Counting {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        _: Class,
    ) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get_range(key, range).await
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        self.saw(key);
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        _: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.saw(key);
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get_suffix(key, n).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, _: Class) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get_suffix(key, n).await
    }
    async fn get_immutable(&self, key: &Key, _: Class) -> Result<Bytes, BlobError> {
        self.saw(key);
        self.inner.get(key).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.saw(key);
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.saw(key);
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.saw(key);
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

/// Documents of two vectors near one of twelve topics, so a clustering has lists to find.
fn topical(i: usize) -> Document {
    let topic = i % 12;
    let mut s = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut r = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s % 2000) as f32 / 1000.0 - 1.0
    };
    let base = |t: usize, d: usize| ((t * 7 + d * 3) % 11) as f32 - 5.0;
    let v = |r: &mut dyn FnMut() -> f32| -> Vec<f32> {
        (0..4).map(|d| base(topic, d) + 0.3 * r()).collect()
    };
    let mut doc = Document::new(format!("d{i:05}"), vec![1.0, 0.5, 0.25, 0.125]);
    doc.vectors.insert(
        "late".to_owned(),
        VectorField::Dense(vec![v(&mut r), v(&mut r)]),
    );
    doc
}

#[tokio::test]
async fn the_clusters_are_asked_for_only_by_an_fde_leg_of_a_flagged_segment() {
    let s = Arc::new(Counting::default());
    let e = Engine::new(Arc::clone(&s), T, LaneId(1));
    e.write("big", (0..9_000).map(topical).collect())
        .await
        .unwrap();
    e.write("small", (0..40).map(topical).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let fde = Prefetch::MultiFde {
        field: "late".to_owned(),
        query: vec![vec![-2.0, 1.0, 4.0, 0.0]],
        limit: 5,
        rerank: None,
        probes: None,
    };
    let dense = Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.5, 0.25, 0.125],
        limit: 5,
        tune: pstore_index::vec_index::Query {
            k: 5,
            ..pstore_index::vec_index::Query::default()
        },
    };
    let ask = |index: &'static str, leg: &Prefetch| {
        let (e, s) = (&e, &s);
        let leg = leg.clone();
        async move {
            let from = s.counts();
            e.query(index, &[leg], Fusion::default(), 5).await.unwrap();
            let now = s.counts();
            (now.0 - from.0, now.1 - from.1)
        }
    };
    let (fdec, fdev) = ask("big", &fde).await;
    assert_eq!(
        fdec, 1,
        "an fde leg over a flagged segment reads its clusters once"
    );
    assert!(fdev >= 1, "and its lists");
    assert_eq!(
        ask("big", &dense).await,
        (0, 0),
        "a dense leg reads neither"
    );
    assert_eq!(
        ask("small", &fde).await,
        (0, 0),
        "a segment HEAD does not flag is never asked: no known-404 read"
    );
}

/// Bytes of `.fdev` a query over `big` moves at `probes`.
async fn fdev_bytes(probes: Option<usize>) -> (u64, f32) {
    let acct = Arc::new(pstore_blob::Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("big", (0..9_000).map(topical).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let leg = Prefetch::MultiFde {
        field: "late".to_owned(),
        query: vec![vec![-2.0, 1.0, 4.0, 0.0]],
        limit: 10,
        rerank: None,
        probes,
    };
    acct.record_ranges();
    let a = e.query("big", &[leg], Fusion::default(), 10).await.unwrap();
    let moved: u64 = acct
        .ranges()
        .iter()
        .filter(|(k, _)| k.as_str().ends_with(".fdev"))
        .map(|(_, r)| r.end - r.start)
        .sum();
    (
        moved,
        a.hits.iter().map(|h| h.score).sum::<f32>() / a.hits.len() as f32,
    )
}

#[tokio::test]
async fn more_probes_read_more_and_all_of_them_read_everything() {
    let (one, one_score) = fdev_bytes(Some(1)).await;
    let (default, default_score) = fdev_bytes(None).await;
    let (all, all_score) = fdev_bytes(Some(1024)).await;
    eprintln!("1: {one} default: {default} all: {all}");
    assert!(
        one > 0 && one < default && default < all,
        "{one} {default} {all}"
    );
    assert!(all_score >= default_score && default_score >= one_score * 0.999);
}

/// A `topical` document that also carries a second multi-vector field, `other`.
fn two_field(i: usize) -> Document {
    let mut d = topical(i);
    let j = (i * 5) % 9_000;
    let mut o = topical(j);
    let v = o.vectors.remove("late").unwrap();
    d.vectors.insert("other".to_owned(), v);
    d
}

#[tokio::test]
async fn two_fields_in_one_segment_each_use_their_clusters() {
    let acct = Arc::new(pstore_blob::Accounted::new(MemoryStore::new()));
    let store = Arc::new(acct.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(1));
    e.write("two", (0..9_000).map(two_field).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let head = e.head_for_test().await;
    let seg = pstore_blob::Key::new(head.indexes["two"][0].key.clone());
    let set = pstore_index::fde_clusters::FdeSet::open(&*store, &seg)
        .await
        .unwrap();
    assert_eq!(set.len(), 2, "one sidecar names both fields");
    let leg = |field: &str, probes| Prefetch::MultiFde {
        field: field.to_owned(),
        query: vec![vec![-2.0, 1.0, 4.0, 0.0]],
        limit: 10,
        rerank: None,
        probes,
    };
    let moved = |field: &'static str, probes| {
        let (e, acct) = (&e, Arc::clone(&acct));
        async move {
            acct.record_ranges();
            let a = e
                .query("two", &[leg(field, probes)], Fusion::default(), 10)
                .await
                .unwrap();
            assert_eq!(a.hits.len(), 10, "{field}");
            acct.ranges()
                .iter()
                .filter(|(k, _)| k.as_str().ends_with(".fdev"))
                .map(|(_, r)| r.end - r.start)
                .sum::<u64>()
        }
    };
    for field in ["late", "other"] {
        let some = moved(field, Some(1)).await;
        let every = moved(field, Some(1024)).await;
        assert!(some > 0 && some < every, "{field}: {some} of {every}");
    }
}
