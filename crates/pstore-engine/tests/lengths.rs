//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M45: HEAD records each segment's length, and every open of a segment it knows reads an
//! absolute range -- never the suffix read Azure has no form of (C-14).

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::{Consistency, Engine, ReplicaSource, Sources, SyncState};
use pstore_format::{Document, Impact, Value, VectorField};
use pstore_query::{Aggregate, AggregateSpec, Fusion, OrderBy, Prefetch};
use pstore_types::{CasTag, LaneId, TenantId};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const SRC: TenantId = TenantId(4500);
const DST: TenantId = TenantId(4501);

/// A store that can refuse suffix reads, as Azure does, and counts every read by key:
/// `(reads, bytes)`.
#[derive(Debug, Default)]
struct Store {
    inner: MemoryStore,
    refuse_suffix: AtomicBool,
    suffixes: AtomicU64,
    reads: Mutex<BTreeMap<String, (u64, u64)>>,
}

impl Store {
    fn refusing() -> Arc<Self> {
        let s = Self::default();
        s.refuse_suffix.store(true, Ordering::SeqCst);
        Arc::new(s)
    }
    fn saw(&self, key: &Key, bytes: usize) {
        let mut r = self.reads.lock().unwrap();
        let e = r.entry(key.as_str().to_owned()).or_default();
        e.0 += 1;
        e.1 += bytes as u64;
    }
    fn suffix(&self) -> Result<(), BlobError> {
        self.suffixes.fetch_add(1, Ordering::SeqCst);
        if self.refuse_suffix.load(Ordering::SeqCst) {
            return Err(BlobError::Other("no suffix range here (C-14)".to_owned()));
        }
        Ok(())
    }
    /// Every read since the last call, but HEAD's.
    fn take_reads(&self) -> BTreeMap<String, (u64, u64)> {
        let mut r = std::mem::take(&mut *self.reads.lock().unwrap());
        r.retain(|k, _| !k.ends_with("/HEAD"));
        r
    }
}

fn counted(s: &Store, key: &Key, r: Result<Bytes, BlobError>) -> Result<Bytes, BlobError> {
    if let Ok(b) = &r {
        s.saw(key, b.len());
    }
    r
}

#[async_trait::async_trait]
impl BlobStore for Store {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        counted(self, key, self.inner.get(key).await)
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        counted(self, key, self.inner.get_range(key, range).await)
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        _: Class,
    ) -> Result<Bytes, BlobError> {
        counted(self, key, self.inner.get_range(key, range).await)
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        let out = self.inner.get_ranges(key, ranges).await?;
        for b in &out {
            self.saw(key, b.len());
        }
        Ok(out)
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        _: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.get_ranges(key, ranges).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.suffix()?;
        counted(self, key, self.inner.get_suffix(key, n).await)
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, _: Class) -> Result<Bytes, BlobError> {
        self.get_suffix(key, n).await
    }
    async fn get_immutable(&self, key: &Key, _: Class) -> Result<Bytes, BlobError> {
        counted(self, key, self.inner.get(key).await)
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        let (b, t) = self.inner.get_with_tag(key).await?;
        self.saw(key, b.len());
        Ok((b, t))
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
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

/// No remote stores: the replica's source is another tenant in the same store.
struct NoRemotes;

impl Sources for NoRemotes {
    fn store(&self, _: &str) -> Option<Arc<dyn BlobStore>> {
        None
    }
}

fn engine(s: &Arc<Store>, tenant: TenantId, lane: u64) -> Engine<Store> {
    // A threshold of 4 clusters every segment past four rows, so the dense leg reads
    // centroids and lists, not only the exact-scan path.
    Engine::new(Arc::clone(s), tenant, LaneId(lane)).with_index_params(
        pstore_index::cluster::Params {
            exact_scan_threshold: 4,
            ..pstore_index::cluster::Params::default()
        },
    )
}

fn doc(i: u32) -> Document {
    let x = i as f32;
    let mut d = Document::new(format!("d{i:04}"), vec![x.sin(), x.cos(), 1.0]);
    d.attrs.insert("n".to_owned(), Value::Int(i64::from(i)));
    d.attrs.insert(
        "text".to_owned(),
        Value::Str(format!("word{} word{}", i % 5, i % 3)),
    );
    d.vectors.insert(
        "s".to_owned(),
        VectorField::Sparse(vec![(3, Impact::new(0.5)), (i % 7, Impact::new(0.25))]),
    );
    d
}

/// Three folded segments of eight rows each.
async fn written(s: &Arc<Store>) -> Engine<Store> {
    let w = engine(s, SRC, 1);
    for k in 0..3u32 {
        w.write("idx", (k * 8..k * 8 + 8).map(doc).collect())
            .await
            .unwrap();
        w.flush().await.unwrap();
        w.fold().await.unwrap();
    }
    w
}

fn legs() -> [Vec<Prefetch>; 3] {
    [
        vec![Prefetch::Dense {
            field: pstore_format::DEFAULT_FIELD.to_owned(),
            query: vec![0.3, 0.9, 1.0],
            limit: 5,
            tune: pstore_index::vec_index::Query::default(),
        }],
        vec![Prefetch::Text {
            field: "text".to_owned(),
            query: "word2".to_owned(),
            limit: 5,
        }],
        vec![Prefetch::Sparse {
            field: "s".to_owned(),
            query: vec![(3, 1.0), (2, 0.5)],
            limit: 5,
        }],
    ]
}

/// Every leg's ranked ids, as text to compare.
async fn ranked(e: &Engine<Store>, index: &str) -> String {
    let mut out = String::new();
    for p in legs() {
        let a = e.query(index, &p, Fusion::default(), 5).await.unwrap();
        out.push_str(&format!("{:?}\n", e.resolve(&a)));
    }
    out
}

/// Everything that opens a segment, and what it answered.
async fn everything(s: &Arc<Store>) -> String {
    let w = written(s).await;
    let mut out = ranked(&w, "idx").await;

    let gt = pstore_query::Predicate::Cmp("n".to_owned(), pstore_query::Op::Gt, Value::Int(5));
    let by = OrderBy {
        attr: "n".to_owned(),
        desc: true,
    };
    let o = w.ordered("idx", &by, Some(&gt), 0, 4, None).await.unwrap();
    out.push_str(&format!(
        "{:?}\n",
        o.rows.iter().map(|d| d.id.clone()).collect::<Vec<_>>()
    ));
    let spec = AggregateSpec {
        labels: vec![("c".to_owned(), Aggregate::Count(None))],
        group_by: vec![],
        top_k: 10,
    };
    let a = w
        .aggregate_as("idx", spec, Some(&gt), None, Consistency::Eventual)
        .await
        .unwrap();
    out.push_str(&format!("{a:?}\n"));
    out.push_str(&format!("{}\n", w.scan("idx", None).await.unwrap().len()));

    w.warm("idx").await.unwrap();
    // A fresh reader, so its warm opens rather than finding what the writer's fold left.
    engine(s, SRC, 3).warm("idx").await.unwrap();

    w.compact("idx").await.unwrap().expect("a merge");
    out.push_str(&ranked(&engine(s, SRC, 4), "idx").await);

    let d = engine(s, DST, 1);
    d.create_replication("rep", ReplicaSource::local(SRC, "idx"), &**s)
        .await
        .unwrap();
    let mut st = SyncState::default();
    d.replicate(&NoRemotes, &mut st).await.unwrap();
    out.push_str(&ranked(&engine(s, DST, 2), "rep").await);
    out
}

#[tokio::test]
async fn every_open_reads_a_range_when_head_knows_the_length() {
    let refusing = Store::refusing();
    let got = everything(&refusing).await;
    assert_eq!(refusing.suffixes.load(Ordering::SeqCst), 0);

    let plain = Arc::new(Store::default());
    assert_eq!(got, everything(&plain).await, "the answers differ");
}

/// Forgets every ref's length, as a HEAD a pre-M45 node wrote has none.
async fn forget(e: &Engine<Store>) {
    e.commit_head_for_test(|h| {
        for r in h.indexes.values_mut().flatten() {
            r.len = None;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_known_length_costs_what_the_suffix_cost() {
    let s = Arc::new(Store::default());
    let w = written(&s).await;
    s.take_reads();
    let known = ranked(&engine(&s, SRC, 2), "idx").await;
    let with = s.take_reads();
    assert_eq!(s.suffixes.load(Ordering::SeqCst), 0);

    forget(&w).await;
    s.take_reads();
    let unknown = ranked(&engine(&s, SRC, 3), "idx").await;
    let without = s.take_reads();
    assert!(
        s.suffixes.load(Ordering::SeqCst) > 0,
        "no suffix read without lengths"
    );

    assert_eq!(known, unknown);
    assert!(!with.is_empty());
    assert_eq!(with, without, "reads or bytes differ by key");
}

#[tokio::test]
async fn a_head_without_lengths_reads_by_suffix() {
    let s = Arc::new(Store::default());
    let w = written(&s).await;
    let before = ranked(&engine(&s, SRC, 2), "idx").await;
    forget(&w).await;
    let after = ranked(&engine(&s, SRC, 3), "idx").await;
    assert_eq!(before, after);
    assert!(
        s.suffixes.load(Ordering::SeqCst) > 0,
        "no suffix read without lengths"
    );

    // The next fold records a length for the segment it seals, and only for it.
    w.write("idx", (100..108).map(doc).collect()).await.unwrap();
    w.flush().await.unwrap();
    w.fold().await.unwrap();
    let h = w.head_for_test().await;
    let lens: Vec<bool> = h.indexes["idx"].iter().map(|r| r.len.is_some()).collect();
    assert_eq!(lens.iter().filter(|k| **k).count(), 1, "{lens:?}");
}
