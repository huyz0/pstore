//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! When the catalog splits — M108, OQ-8's threshold.
//!
//! ⚠️ **The fullest bucket, not the mean.** The fold that costs most is the fullest bucket's,
//! and criterion 1 measures how far hash skew puts it from the mean rather than assuming.

use bytes::Bytes;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, OpClass,
    Precondition, PutOutcome,
};
use pstore_catalog::{
    Appender, CatalogError, DEFAULT_WIDTH, MAX_WIDTH, Occupancy, Root, SplitOutcome, SplitPolicy,
    TenantRecord, Width, bucket_of, enumerate, fold, head_key, read_head, read_root, root_key,
    split, split_if_due, write_root,
};
use pstore_types::{CasTag, Epoch, TenantId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const TENANTS: u128 = 400;

fn rec(t: u128) -> TenantRecord {
    TenantRecord::live(TenantId(t), Epoch(1), &["idx".to_owned()])
}

fn w(n: u32) -> Width {
    Width::new(n).expect("a width")
}

/// A catalog of `TENANTS` tenants at width `n`, folded so every bucket has a run.
async fn seeded<S: BlobStore>(store: &Arc<S>, n: u32) -> Width {
    let width = w(n);
    write_root(
        store.as_ref(),
        Root {
            epoch: Epoch(1),
            width,
        },
        None,
    )
    .await
    .unwrap();
    let a = Appender::new(Arc::clone(store), width);
    for t in 0..TENANTS {
        a.record(&rec(t)).await.unwrap();
    }
    for b in width.all() {
        fold(store.as_ref(), b).await.unwrap();
    }
    width
}

/// Every tenant an enumeration at `width` finds.
async fn tenants<S: BlobStore>(store: &S, width: Width) -> Vec<u128> {
    enumerate(store, width)
        .await
        .unwrap()
        .records
        .iter()
        .map(|r| r.tenant.0)
        .collect()
}

/// Each bucket's run size, read directly rather than through the census under test.
async fn run_sizes<S: BlobStore>(store: &S, width: Width) -> Vec<u64> {
    let mut out = Vec::new();
    for b in width.all() {
        let (head, _) = read_head(store, b).await.unwrap();
        out.push(match head.run(b) {
            Some(k) => store.get(&k).await.unwrap().len() as u64,
            None => 0,
        });
    }
    out
}

/// The shared store, with switches for one splitter's view of it.
#[derive(Debug, Clone)]
struct View {
    inner: Arc<MemoryStore>,
    /// Served for the root instead of what is there: a splitter that read it before a flip.
    stale_root: Arc<Mutex<Option<(Bytes, CasTag)>>>,
    /// The next root write answers `Io`, having written nothing: a crash before the flip.
    fail_root: Arc<AtomicBool>,
    /// Every head create answers `Contended`, having written nothing: a 409.
    contend_heads: Arc<AtomicBool>,
}

impl View {
    fn over(inner: &Arc<MemoryStore>) -> Self {
        Self {
            inner: Arc::clone(inner),
            stale_root: Arc::default(),
            fail_root: Arc::default(),
            contend_heads: Arc::default(),
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for View {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        if *key == root_key()
            && let Some(stale) = self.stale_root.lock().unwrap().clone()
        {
            return Ok(stale);
        }
        self.inner.get_with_tag(key).await
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
        if *key == root_key() && self.fail_root.swap(false, Ordering::SeqCst) {
            return Err(CasError::Io("refused before writing".to_owned()));
        }
        if self.contend_heads.load(Ordering::SeqCst)
            && matches!(pre, Precondition::NotExists)
            && (0..16).any(|b| *key == head_key(b))
        {
            return Err(CasError::Contended);
        }
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

/// Criterion 1: the default threshold against the corpus's design point.
#[test]
fn the_default_splits_past_twice_the_design_point_and_not_at_it() {
    // Sized so the MEAN bucket at 1M tenants is 640 KiB, the corpus's design point.
    let record = 655_360 * u64::from(DEFAULT_WIDTH) / 1_000_000;
    assert_eq!(record, 10_737);
    let limit = SplitPolicy::default().max_run_bytes;
    let width = Width::default();
    let mut counts = vec![0u64; DEFAULT_WIDTH as usize];
    let mut done = 0u128;
    for (n, under) in [(1_000_000u128, true), (2_000_000, true), (2_500_000, false)] {
        for t in done..n {
            counts[bucket_of(TenantId(t), width) as usize] += 1;
        }
        done = n;
        let max = *counts.iter().max().unwrap();
        let mean = n as f64 / f64::from(DEFAULT_WIDTH);
        let fullest = max * record;
        println!(
            "{n} tenants: fullest bucket {max} tenants, mean {mean:.1}, skew {:.2}, \
             {fullest} bytes against {limit}",
            max as f64 / mean
        );
        assert_eq!(
            fullest <= limit,
            under,
            "{n} tenants put {fullest} bytes in the fullest bucket against a limit of {limit}"
        );
    }
}

/// Criteria 2 and 7: below the threshold, one census and nothing written.
#[tokio::test]
async fn below_the_threshold_nothing_is_written() {
    let t = TenantId(0);
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let view = Arc::new(acct.as_tenant(t));
    let width = seeded(&view, 4).await;
    let sizes = run_sizes(view.as_ref(), width).await;
    let max = *sizes.iter().max().unwrap();
    let fullest = sizes.iter().position(|n| *n == max).unwrap() as u32;
    let root = read_root(view.as_ref()).await.unwrap().0;
    let (reads, writes) = (acct.count(t, OpClass::Read), acct.count(t, OpClass::Write));

    let out = split_if_due(view.as_ref(), &SplitPolicy { max_run_bytes: max })
        .await
        .unwrap();

    assert_eq!(
        out,
        SplitOutcome::NotDue(Occupancy {
            width,
            run_bytes: sizes,
            fullest,
            fullest_bytes: max,
        })
    );
    assert_eq!(acct.count(t, OpClass::Read) - reads, 1 + 2 * 4);
    assert_eq!(acct.count(t, OpClass::Write) - writes, 0);
    assert_eq!(acct.count(t, OpClass::List), 0, "the census listed");
    assert_eq!(read_root(view.as_ref()).await.unwrap().0, root);
}

/// Criteria 3 and 7: above it, one split, reading only the census.
#[tokio::test]
async fn above_the_threshold_it_splits_on_the_census_it_decided_on() {
    let t = TenantId(0);
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let view = Arc::new(acct.as_tenant(t));
    let width = seeded(&view, 4).await;
    let max = *run_sizes(view.as_ref(), width).await.iter().max().unwrap();
    let reads = acct.count(t, OpClass::Read);

    let out = split_if_due(
        view.as_ref(),
        &SplitPolicy {
            max_run_bytes: max - 1,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        out,
        SplitOutcome::Split {
            from: w(4),
            to: w(8)
        }
    );
    assert_eq!(
        acct.count(t, OpClass::Read) - reads,
        1 + 2 * 4,
        "the split read more than the census it decided on"
    );
    assert_eq!(read_root(view.as_ref()).await.unwrap().0.width, w(8));
    assert_eq!(tenants(view.as_ref(), w(8)).await.len(), TENANTS as usize);
    assert_eq!(acct.count(t, OpClass::List), 0, "the split listed");
}

/// Criterion 4: at the key format, over the threshold is reported and nothing moves.
#[tokio::test]
async fn at_the_format_it_says_so() {
    let store = Arc::new(MemoryStore::new());
    let width = w(MAX_WIDTH);
    let root = Root {
        epoch: Epoch(1),
        width,
    };
    write_root(store.as_ref(), root, None).await.unwrap();
    Appender::new(Arc::clone(&store), width)
        .record(&rec(7))
        .await
        .unwrap();
    let b = bucket_of(TenantId(7), width);
    fold(store.as_ref(), b).await.unwrap();

    let out = split_if_due(store.as_ref(), &SplitPolicy { max_run_bytes: 1 })
        .await
        .unwrap();

    match out {
        SplitOutcome::AtFormat(o) => {
            assert_eq!((o.width, o.fullest), (width, b));
            assert!(o.fullest_bytes > 1);
        }
        SplitOutcome::NotDue(_) => panic!("NotDue at a 1-byte threshold"),
        SplitOutcome::Split { from, to } => panic!("split {from:?} to {to:?} past the format"),
    }
    assert_eq!(read_root(store.as_ref()).await.unwrap().0, root);
}

/// Criteria 5 and 7: a record appended at the new width survives a second, stale splitter.
#[tokio::test]
async fn a_record_appended_between_two_splits_survives() {
    let store = Arc::new(MemoryStore::new());
    seeded(&store, 4).await;
    let b = View::over(&store);
    // B read the root before A flipped it.
    *b.stale_root.lock().unwrap() = Some(store.get_with_tag(&root_key()).await.unwrap());
    assert_eq!(split(store.as_ref()).await.unwrap(), w(8));
    // Recorded at the new width into a sibling bucket, one A wrote.
    let late = (10_000..)
        .find(|t| bucket_of(TenantId(*t), w(8)) >= 4)
        .unwrap();
    Appender::new(Arc::clone(&store), w(8))
        .record(&TenantRecord::live(
            TenantId(late),
            Epoch(1),
            &["late".to_owned()],
        ))
        .await
        .unwrap();

    let err = split(&b)
        .await
        .expect_err("a stale splitter flipped the root");

    assert!(matches!(err, CatalogError::RootContended), "{err}");
    let found = tenants(store.as_ref(), w(8)).await;
    assert!(
        found.contains(&late),
        "tenant {late}, appended at the new width, was lost to a stale splitter"
    );
    assert_eq!(found.len(), TENANTS as usize + 1);
}

/// Criterion 6: a split that crashed before the flip completes when run again.
#[tokio::test]
async fn a_rerun_after_a_crash_still_completes() {
    let store = Arc::new(MemoryStore::new());
    seeded(&store, 4).await;
    let crashing = View::over(&store);
    crashing.fail_root.store(true, Ordering::SeqCst);
    split(&crashing)
        .await
        .expect_err("the root write was refused");
    assert_eq!(read_root(store.as_ref()).await.unwrap().0.width, w(4));
    let (_, sibling) = read_head(store.as_ref(), 4).await.unwrap();
    assert!(sibling.is_some(), "the crashed split wrote no sibling");

    assert_eq!(split(store.as_ref()).await.unwrap(), w(8));
    assert_eq!(tenants(store.as_ref(), w(8)).await.len(), TENANTS as usize);
}

/// A 409 on a sibling's head create wrote nothing, so the split stops before the flip. Losing
/// the create (412) is success; failing to evaluate it is not.
#[tokio::test]
async fn a_contended_sibling_head_fails_the_split_before_the_flip() {
    let store = Arc::new(MemoryStore::new());
    seeded(&store, 4).await;
    let contending = View::over(&store);
    contending.contend_heads.store(true, Ordering::SeqCst);
    let err = split(&contending)
        .await
        .expect_err("a sibling head was never written");
    assert!(
        matches!(err, CatalogError::Contended(b, _) if b >= 4),
        "not a sibling's contention: {err:?}"
    );
    assert_eq!(read_root(store.as_ref()).await.unwrap().0.width, w(4));
    assert_eq!(tenants(store.as_ref(), w(4)).await.len(), TENANTS as usize);
}

/// On a tie the fullest bucket is the lowest-numbered: a catalog never folded has every run
/// at 0 bytes, and names bucket 0.
#[tokio::test]
async fn a_tie_names_the_lowest_bucket() {
    let store = MemoryStore::new();
    let width = w(4);
    write_root(
        &store,
        Root {
            epoch: Epoch(1),
            width,
        },
        None,
    )
    .await
    .unwrap();
    let out = split_if_due(&store, &SplitPolicy { max_run_bytes: 1 })
        .await
        .unwrap();
    assert_eq!(
        out,
        SplitOutcome::NotDue(Occupancy {
            width,
            run_bytes: vec![0; 4],
            fullest: 0,
            fullest_bytes: 0,
        })
    );
}
