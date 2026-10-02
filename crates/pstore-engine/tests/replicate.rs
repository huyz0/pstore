//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Replication (M22.2): a destination index that follows a source index -- in its own
//! tenant, another tenant, or another store -- by pulling the source's folded state.

use bytes::Bytes;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, OpClass,
    Precondition, PutOutcome, TenantView,
};
use pstore_engine::{
    Engine, EngineError, ReplicaRefusal, ReplicaSource, Sources, SyncState, Synced,
};
use pstore_format::{Document, Impact, Value, VectorField};
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, OrderBy, Prefetch};
use pstore_types::{CasTag, Epoch, LaneId, TenantId};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::{Arc, Mutex};

const SRC: TenantId = TenantId(71);
const DST: TenantId = TenantId(72);
const THRESHOLD: usize = 16;

type E = Engine<TenantView<Hooked>>;

/// A store that can fail or intercept chosen reads, over one shared `MemoryStore`.
#[derive(Clone, Default)]
struct Hooked {
    inner: Arc<MemoryStore>,
    /// Reads of keys containing any of these fail as an outage would.
    fail: Arc<Mutex<Vec<String>>>,
    /// Run once, before the first read of a key containing the pattern.
    hook: Arc<Mutex<Option<(String, Hook)>>>,
    /// The next read of a key containing this fails as an outage would, once.
    fail_once: Arc<Mutex<Option<String>>>,
    /// Reads of exactly this key answer 404 this many more times, though it exists: what a
    /// source that moved between its HEAD and the read looks like.
    missing: Arc<Mutex<BTreeMap<String, u32>>>,
    /// The next this-many conditional writes are contended (a 409).
    contend_next: Arc<std::sync::atomic::AtomicU32>,
    /// The next this-many conditional writes lose, as to a writer that landed first.
    lose_next: Arc<std::sync::atomic::AtomicU32>,
    /// The next this-many conditional writes fail as an outage would.
    fail_cas: Arc<std::sync::atomic::AtomicU32>,
}

type Hook = Box<dyn FnOnce() -> futures_util::future::BoxFuture<'static, ()> + Send>;

impl std::fmt::Debug for Hooked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooked").finish_non_exhaustive()
    }
}

impl Hooked {
    async fn before(&self, key: &Key) -> Result<(), BlobError> {
        let hook = {
            let mut h = self.hook.lock().unwrap();
            if h.as_ref()
                .is_some_and(|(p, _)| key.as_str().contains(p.as_str()))
            {
                h.take().map(|(_, f)| f)
            } else {
                None
            }
        };
        if let Some(f) = hook {
            f().await;
        }
        {
            let mut once = self.fail_once.lock().unwrap();
            if once
                .as_ref()
                .is_some_and(|p| key.as_str().contains(p.as_str()))
            {
                *once = None;
                return Err(BlobError::Other("injected outage".to_owned()));
            }
        }
        if self
            .fail
            .lock()
            .unwrap()
            .iter()
            .any(|p| key.as_str().contains(p.as_str()))
        {
            return Err(BlobError::Other("injected outage".to_owned()));
        }
        if let Some(n) = self.missing.lock().unwrap().get_mut(key.as_str())
            && *n > 0
        {
            *n -= 1;
            return Err(BlobError::NotFound(key.to_string()));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl BlobStore for Hooked {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.before(key).await?;
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.before(key).await?;
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.before(key).await?;
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.before(key).await?;
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
        let failing = self
            .fail_cas
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok();
        if failing {
            return Err(CasError::Io("injected outage".to_owned()));
        }
        if self
            .contend_next
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(CasError::Contended);
        }
        if self
            .lose_next
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(CasError::Lost);
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

/// One store, accounted per tenant.
#[derive(Clone)]
struct World {
    hooked: Hooked,
    acct: Accounted<Hooked>,
}

impl World {
    fn new() -> Self {
        let hooked = Hooked::default();
        Self {
            acct: Accounted::new(hooked.clone()),
            hooked,
        }
    }
    fn engine(&self, tenant: TenantId, lane: u64) -> E {
        Engine::new(Arc::new(self.acct.as_tenant(tenant)), tenant, LaneId(lane)).with_index_params(
            pstore_index::cluster::Params {
                exact_scan_threshold: THRESHOLD,
                target_list_size: 8,
                ..pstore_index::cluster::Params::default()
            },
        )
    }
    fn reads(&self, tenant: TenantId) -> u64 {
        self.acct.count(tenant, OpClass::Read)
    }
    fn writes(&self, tenant: TenantId) -> u64 {
        self.acct.count(tenant, OpClass::Write)
    }
    async fn keys(&self, prefix: &str) -> BTreeSet<String> {
        self.hooked
            .inner
            .list_unrestricted(&Key::new(prefix))
            .await
            .unwrap()
            .into_iter()
            .map(|k| k.as_str().to_owned())
            .collect()
    }
}

/// Named remote stores, as the server configures them.
#[derive(Default)]
struct Remotes(BTreeMap<String, Arc<dyn BlobStore>>);

impl Sources for Remotes {
    fn store(&self, name: &str) -> Option<Arc<dyn BlobStore>> {
        self.0.get(name).cloned()
    }
}

fn doc(id: &str, x: f32) -> Document {
    let mut d = Document::new(id, vec![x.sin(), x.cos()]);
    d.attrs.insert("n".to_owned(), Value::Int(x as i64));
    d.attrs.insert(
        "text".to_owned(),
        Value::Str(format!("the quick fox number {}", x as u32 % 5)),
    );
    d.vectors.insert(
        "s".to_owned(),
        VectorField::Sparse(vec![
            (3, Impact::new(0.5)),
            ((x as u32) % 7, Impact::new(0.25)),
        ]),
    );
    d
}

async fn fold(e: &E) {
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

async fn write(e: &E, index: &str, ids: Range<u32>) {
    e.write(
        index,
        ids.map(|i| doc(&format!("d{i:04}"), i as f32)).collect(),
    )
    .await
    .unwrap();
    fold(e).await;
}

/// Every query kind's answer, as text to compare: all rows by id, and dense, filtered,
/// sparse and text rankings. `None` when the index does not exist.
async fn answers(e: &E, index: &str) -> Option<String> {
    let by = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    let all = e.ordered(index, &by, None, 0, 100_000, None).await.unwrap();
    if !all.exists {
        return None;
    }
    let mut out = format!(
        "{:?}\n",
        all.rows
            .iter()
            .map(|d| (d.id.clone(), d.attrs.get("n").cloned()))
            .collect::<Vec<_>>()
    );
    let dense = vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.0],
        limit: 10,
        tune: Query::default(),
    }];
    let sparse = vec![Prefetch::Sparse {
        field: "s".to_owned(),
        query: vec![(3, 1.0), (2, 0.5)],
        limit: 10,
    }];
    let text = vec![Prefetch::Text {
        field: "text".to_owned(),
        query: "fox 3".to_owned(),
        limit: 10,
    }];
    let gt = pstore_query::Predicate::Cmp("n".to_owned(), pstore_query::Op::Gt, Value::Int(20));
    for (p, f) in [
        (&dense, None),
        (&dense, Some(&gt)),
        (&sparse, None),
        (&text, None),
    ] {
        let a = e
            .query_filtered(index, p, f, Fusion::default(), 10)
            .await
            .unwrap();
        out.push_str(&format!("{:?}\n", e.resolve(&a)));
    }
    Some(out)
}

/// A replication's three kinds of source: this tenant, another tenant in this store, and
/// another tenant in another store.
#[derive(Debug, Clone, Copy)]
enum Kind {
    SameTenant,
    CrossTenant,
    Remote,
}

struct Setup {
    here: World,
    src: E,
    dst: E,
    source: ReplicaSource,
    remotes: Remotes,
}

fn setup(kind: Kind) -> Setup {
    let here = World::new();
    let there = World::new();
    let (src, source) = match kind {
        Kind::SameTenant => (here.engine(DST, 9), ReplicaSource::local(DST, "src")),
        Kind::CrossTenant => (here.engine(SRC, 9), ReplicaSource::local(SRC, "src")),
        Kind::Remote => (
            there.engine(SRC, 9),
            ReplicaSource {
                store: "far".to_owned(),
                tenant: SRC,
                index: "src".to_owned(),
            },
        ),
    };
    let mut remotes = Remotes::default();
    // Billed to the destination's tenant: the puller pays.
    remotes.0.insert(
        "far".to_owned(),
        Arc::new(there.acct.as_tenant(DST)) as Arc<dyn BlobStore>,
    );
    Setup {
        dst: here.engine(DST, 1),
        here,
        src,
        source,
        remotes,
    }
}

impl Setup {
    async fn create(&self) -> Epoch {
        let from: Arc<dyn BlobStore> = match self.source.store.as_str() {
            "" => Arc::new(self.here.acct.as_tenant(DST)),
            _ => self.remotes.store("far").unwrap(),
        };
        self.dst
            .create_replication("dst", self.source.clone(), &*from)
            .await
            .unwrap()
    }
    async fn sync(&self, st: &mut SyncState) -> Synced {
        self.dst.replicate(&self.remotes, st).await.unwrap()
    }
    async fn same(&self, why: &str) {
        let (s, d) = (
            answers(&self.src, "src").await,
            answers(&self.dst, "dst").await,
        );
        assert!(s.is_some(), "{why}: the source is gone");
        assert_eq!(s, d, "{why}: the replica differs from its source");
        // And scores by the same schema: its metric, analyzer and sketches.
        let (hs, hd) = (
            self.src.head_for_test().await,
            self.dst.head_for_test().await,
        );
        assert_eq!(
            hd.schemas.get("dst"),
            hs.schemas.get("src"),
            "{why}: the schema"
        );
    }
}

/// Every object under the destination index that its HEAD does not name, after a GC: what a
/// replication leaked.
async fn leaked(w: &World, e: &E, index: &str) -> BTreeSet<String> {
    e.gc(0).await.unwrap();
    let head = e.head_for_test().await;
    let mut named: BTreeSet<String> = BTreeSet::new();
    for r in head.indexes.get(index).into_iter().flatten() {
        for suffix in ["", ".cen", ".sdict", ".tdict"] {
            named.insert(format!("{}{suffix}", r.key));
        }
    }
    named.extend(head.deletes.values().map(|(k, _)| k.clone()));
    let prefix = format!("{:04x}/tnt/{}/idx/{index}/", DST.0 as u16, DST.0);
    w.keys(&prefix)
        .await
        .into_iter()
        .filter(|k| !named.contains(k))
        .collect()
}

/// What the source did, applied to any kind of source.
async fn follows(kind: Kind) {
    let s = setup(kind);
    write(&s.src, "src", 0..40).await;
    write(&s.src, "src", 40..50).await;
    s.create().await;
    let mut st = SyncState::default();
    let first = s.sync(&mut st).await;
    assert_eq!(first.committed, vec!["dst".to_owned()], "{kind:?}");
    s.same("first sync").await;

    // Writes, upserts and deletes.
    s.src
        .write("src", vec![doc("d0003", 99.0), doc("n0001", 7.0)])
        .await
        .unwrap();
    s.src
        .delete("src", vec!["d0010".into(), "d0045".into()])
        .await
        .unwrap();
    fold(&s.src).await;
    s.sync(&mut st).await;
    s.same("after writes and deletes").await;
    // A segment the replica already holds a vector for gains another delete: the source's
    // vector key changes, so the replica's must.
    s.src.delete("src", vec!["d0011".into()]).await.unwrap();
    fold(&s.src).await;
    s.sync(&mut st).await;
    s.same("after a second delete in one segment").await;

    // Compaction replaces every segment.
    s.src.compact("src").await.unwrap();
    s.sync(&mut st).await;
    s.same("after compaction").await;

    // Drop and recreate: the replica follows the new index, not the old one.
    s.src.delete_index("src").await.unwrap();
    s.src.gc(0).await.unwrap();
    write(&s.src, "src", 100..120).await;
    s.sync(&mut st).await;
    s.same("after drop and recreate").await;

    // A source that borrows its parent's segments (M16), with deletes of its own; then
    // dropped and branched again from a parent with none.
    write(&s.src, "parent", 200..230).await;
    s.src.delete_index("src").await.unwrap();
    s.src.gc(0).await.unwrap();
    s.src.branch("parent", "src").await.unwrap();
    s.src
        .delete("src", vec!["d0201".into(), "d0202".into()])
        .await
        .unwrap();
    fold(&s.src).await;
    s.sync(&mut st).await;
    s.same("after a branch with deletes").await;
    // Again, with as many deletes of other rows of the same parent segment: equal counts, a
    // different vector. Deciding by count would keep the old one forever (spec review B2).
    s.src.delete_index("src").await.unwrap();
    s.src.gc(0).await.unwrap();
    s.src.branch("parent", "src").await.unwrap();
    s.src
        .delete("src", vec!["d0203".into(), "d0204".into()])
        .await
        .unwrap();
    fold(&s.src).await;
    s.sync(&mut st).await;
    s.same("after a re-branch with as many other deletes").await;
    s.src.delete_index("src").await.unwrap();
    s.src.gc(0).await.unwrap();
    s.src.branch("parent", "src").await.unwrap();
    s.sync(&mut st).await;
    s.same("after a re-branch with no deletes").await;
    assert!(
        s.dst.head_for_test().await.deletes.is_empty(),
        "{kind:?}: a vector the source no longer has outlived it"
    );
    assert_eq!(
        leaked(&s.here, &s.dst, "dst").await,
        BTreeSet::new(),
        "{kind:?}"
    );
}

#[tokio::test]
async fn follows_a_same_tenant_source() {
    follows(Kind::SameTenant).await;
}

#[tokio::test]
async fn follows_a_cross_tenant_source() {
    follows(Kind::CrossTenant).await;
}

#[tokio::test]
async fn follows_a_remote_source() {
    follows(Kind::Remote).await;
}

fn dst_prefix() -> String {
    format!("{:04x}/tnt/{}/idx/dst/", DST.0 as u16, DST.0)
}

#[tokio::test]
async fn copies_only_what_changed() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..40).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    let before = s.here.keys(&dst_prefix()).await;

    // One new segment: it and its sidecars, and nothing else.
    write(&s.src, "src", 40..60).await;
    s.sync(&mut st).await;
    let after = s.here.keys(&dst_prefix()).await;
    let new: Vec<&String> = after.difference(&before).collect();
    let segs: Vec<_> = new.iter().filter(|k| k.ends_with(".seg")).collect();
    assert_eq!(segs.len(), 1, "{new:?}");
    let seg = segs[0].as_str();
    let mut expected = BTreeSet::from([
        seg.to_owned(),
        format!("{seg}.tdict"),
        format!("{seg}.sdict"),
        format!("{seg}.cen"),
    ]);
    // Only the sidecars the source segment has.
    let src_new: Vec<String> = s
        .here
        .keys(&format!("{:04x}/tnt/{}/idx/src/", SRC.0 as u16, SRC.0))
        .await
        .into_iter()
        .filter(|k| k.contains(".seg."))
        .collect();
    if !src_new.iter().any(|k| k.ends_with(".cen")) {
        expected.remove(&format!("{seg}.cen"));
    }
    assert_eq!(new.into_iter().cloned().collect::<BTreeSet<_>>(), expected);
    s.same("one new segment").await;

    // A delete in one segment: one vector, no segment.
    let before = after;
    s.src.delete("src", vec!["d0050".into()]).await.unwrap();
    fold(&s.src).await;
    s.sync(&mut st).await;
    let after = s.here.keys(&dst_prefix()).await;
    let new: Vec<&String> = after.difference(&before).collect();
    assert_eq!(new.len(), 1, "{new:?}");
    assert!(new[0].ends_with(".dv"), "{new:?}");
    s.same("one delete").await;
    // Another new segment: the vector already copied is recognised by the source key it
    // carries, not copied again.
    let before = after;
    write(&s.src, "src", 60..70).await;
    s.sync(&mut st).await;
    let after = s.here.keys(&dst_prefix()).await;
    let new: Vec<&String> = after.difference(&before).collect();
    assert!(
        new.iter().all(|k| !k.ends_with(".dv")),
        "a vector was copied again: {new:?}"
    );
    s.same("a segment after a delete").await;
}

#[tokio::test]
async fn an_idle_sync_reads_once_per_source() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    s.sync(&mut st).await;
    let (r, w) = (s.here.reads(DST), s.here.writes(DST));
    let idle = s.sync(&mut st).await;
    assert!(idle.idle, "{idle:?}");
    assert_eq!(s.here.reads(DST) - r, 1);
    assert_eq!(s.here.writes(DST), w);
}

#[tokio::test]
async fn two_replications_of_one_source_read_it_once() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let from = s.here.acct.as_tenant(DST);
    s.dst
        .create_replication("dst2", s.source.clone(), &from)
        .await
        .unwrap();
    let mut st = SyncState::default();
    let first = s.sync(&mut st).await;
    assert_eq!(first.committed.len(), 2, "{first:?}");
    s.sync(&mut st).await;
    let r = s.here.reads(DST);
    assert!(s.sync(&mut st).await.idle);
    assert_eq!(s.here.reads(DST) - r, 1, "one source, one read");
}

#[tokio::test]
async fn compaction_and_gc_on_both_sides_keep_the_replica_whole() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    write(&s.src, "src", 30..60).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    let old: BTreeSet<String> = s.dst.head_for_test().await.indexes["dst"]
        .iter()
        .map(|r| r.key.clone())
        .collect();
    s.src.compact("src").await.unwrap();
    s.src.gc(0).await.unwrap();
    s.sync(&mut st).await;
    s.dst.gc(0).await.unwrap();
    s.same("after both sides compact and collect").await;
    let left = s.here.keys(&dst_prefix()).await;
    assert!(
        old.iter().all(|k| !left.contains(k)),
        "replaced segments were not reaped"
    );
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn a_source_segment_reaped_mid_copy_is_remapped() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    write(&s.src, "src", 30..60).await;
    s.create().await;
    // The first segment read of the sync: before it is answered, the source compacts and
    // collects, so that segment is gone.
    let src = Arc::new(s.here.engine(SRC, 8));
    *s.here.hooked.hook.lock().unwrap() = Some((
        format!("tnt/{}/idx/src/seg/", SRC.0),
        Box::new(move || {
            Box::pin(async move {
                src.compact("src").await.unwrap();
                src.gc(0).await.unwrap();
            })
        }),
    ));
    let mut st = SyncState::default();
    let out = s.sync(&mut st).await;
    assert_eq!(out.committed, vec!["dst".to_owned()], "{out:?}");
    s.same("after a remap").await;
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn a_sidecar_reaped_mid_copy_is_remapped_not_taken_as_absent() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    // One new segment, so it is the only copy in flight. Its text dictionary read: the segment
    // was read whole a moment ago, and before the dictionary is answered the source compacts
    // and collects -- so it is gone, though the segment had one. Taken as absent, the replica
    // would commit a text segment with no dictionary.
    write(&s.src, "src", 30..60).await;
    let src = Arc::new(s.here.engine(SRC, 8));
    *s.here.hooked.hook.lock().unwrap() = Some((
        ".seg.tdict".to_owned(),
        Box::new(move || {
            Box::pin(async move {
                src.compact("src").await.unwrap();
                src.gc(0).await.unwrap();
            })
        }),
    ));
    let out = s.sync(&mut st).await;
    assert_eq!(out.committed, vec!["dst".to_owned()], "{out:?}");
    assert!(
        s.here.hooked.hook.lock().unwrap().is_none(),
        "the hook never ran"
    );
    s.same("after a dictionary vanished mid-copy").await;
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn a_branch_from_a_replica_survives_the_replica_moving_on() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    write(&s.src, "src", 30..60).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    s.dst.branch("dst", "copy").await.unwrap();
    let frozen = answers(&s.dst, "copy").await;
    s.src.compact("src").await.unwrap();
    s.sync(&mut st).await;
    s.dst.gc(0).await.unwrap();
    assert_eq!(answers(&s.dst, "copy").await, frozen);
    s.same("the replica moved on").await;
}

#[tokio::test]
async fn a_lost_commit_reuses_its_copies() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    write(&s.src, "src", 30..60).await;
    s.create().await;
    // Another index of the destination tenant commits while the sync copies.
    let other = s.here.engine(DST, 2);
    let mut st = SyncState::default();
    let out = s
        .dst
        .replicate_with_interference_for_test(&s.remotes, &mut st, async {
            write(&other, "elsewhere", 0..5).await;
        })
        .await
        .unwrap();
    assert_eq!(out.committed, vec!["dst".to_owned()]);
    let segs = s
        .here
        .keys(&dst_prefix())
        .await
        .into_iter()
        .filter(|k| k.ends_with(".seg"))
        .count();
    assert_eq!(segs, 2, "a lost commit copied its segments again");
    s.same("after a lost commit").await;
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn a_rivals_duplicate_copies_are_buried() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    // Two workers on one tenant (claims are advisory): the second commits while the first
    // copies, so the first's copies map segments the HEAD already has.
    let rival = s.here.engine(DST, 2);
    let remotes = Remotes::default();
    let mut st = SyncState::default();
    let out = s
        .dst
        .replicate_with_interference_for_test(&s.remotes, &mut st, async {
            let mut theirs = SyncState::default();
            let r = rival.replicate(&remotes, &mut theirs).await.unwrap();
            assert_eq!(r.committed, vec!["dst".to_owned()]);
        })
        .await
        .unwrap();
    assert!(out.committed.is_empty(), "{out:?}");
    s.same("after a rival's sync").await;
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn an_older_source_never_commits() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let rival = s.here.engine(DST, 2);
    let remotes = Remotes::default();
    let src = &s.src;
    let mut st = SyncState::default();
    // This sync read the source at its first state; meanwhile the source deletes rows and a
    // rival replicates that. Committing now would bring the deleted rows back.
    let out = s
        .dst
        .replicate_with_interference_for_test(&s.remotes, &mut st, async {
            src.delete("src", vec!["d0001".into(), "d0002".into()])
                .await
                .unwrap();
            fold(src).await;
            let mut theirs = SyncState::default();
            rival.replicate(&remotes, &mut theirs).await.unwrap();
        })
        .await
        .unwrap();
    assert!(out.committed.is_empty(), "{out:?}");
    s.same("an older source did not roll the replica back")
        .await;
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

fn refused(r: Result<impl std::fmt::Debug, EngineError>, want: &ReplicaRefusal) {
    match r {
        Err(EngineError::Replica(got)) => assert_eq!(&got, want),
        other => panic!("{want:?} was not refused: {other:?}"),
    }
}

#[tokio::test]
async fn a_replica_refuses_everything_that_would_write_it() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    let ro = ReplicaRefusal::ReadOnly("dst".to_owned());
    refused(s.dst.write("dst", vec![doc("x", 1.0)]).await, &ro);
    refused(s.dst.delete("dst", vec!["d0001".into()]).await, &ro);
    let gt = pstore_query::Predicate::Cmp("n".to_owned(), pstore_query::Op::Gt, Value::Int(1));
    let patch = || pstore_engine::Patch {
        id: "d0001".to_owned(),
        set: BTreeMap::from([("n".to_owned(), Value::Int(5))]),
        unset: Vec::new(),
    };
    refused(s.dst.patch("dst", vec![patch()], None).await, &ro);
    refused(s.dst.delete_if("dst", vec!["d0001".into()], &gt).await, &ro);
    refused(s.dst.delete_by_filter("dst", &gt).await, &ro);
    refused(s.dst.patch_by_filter("dst", &gt, patch()).await, &ro);
    refused(
        s.dst
            .write_if(
                "dst",
                vec![doc("x", 1.0)],
                pstore_engine::Metric::DotProduct,
                &gt,
            )
            .await,
        &ro,
    );
    refused(
        s.dst
            .write_declared(
                "dst",
                vec![doc("x", 1.0)],
                pstore_engine::Metric::DotProduct,
                None,
                &pstore_engine::Declared::default(),
            )
            .await,
        &ro,
    );
    refused(s.dst.compact("dst").await, &ro);
    write(&s.dst, "other", 0..3).await;
    refused(s.dst.branch("other", "dst").await, &ro);
    refused(
        s.dst.delete_index("dst").await,
        &ReplicaRefusal::Active("dst".to_owned()),
    );
    let by = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    let at = s.dst.head_for_test().await.epoch;
    refused(
        s.dst.ordered("dst", &by, None, 0, 10, Some(at)).await,
        &ReplicaRefusal::NoHistory("dst".to_owned()),
    );
    let dense = vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.0],
        limit: 5,
        tune: Query::default(),
    }];
    refused(
        s.dst
            .query_as_of("dst", at, &dense, Fusion::default(), 5)
            .await,
        &ReplicaRefusal::NoHistory("dst".to_owned()),
    );
    // And a scan, which builds its own fresh view.
    let scanned = s.dst.scan("dst", None).await.unwrap();
    assert_eq!(scanned.len(), 30);
    s.same("nothing a refusal touched changed the replica")
        .await;
}

#[tokio::test]
async fn create_refuses_by_name() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    write(&s.dst, "taken", 0..3).await;
    let from = s.here.acct.as_tenant(DST);
    let src = |index: &str| ReplicaSource::local(SRC, index);
    refused(
        s.dst.create_replication("taken", src("src"), &from).await,
        &ReplicaRefusal::IndexExists("taken".to_owned()),
    );
    refused(
        s.dst.create_replication("dst", src("nope"), &from).await,
        &ReplicaRefusal::SourceNotFound("nope".to_owned()),
    );
    refused(
        s.dst
            .create_replication("dst", ReplicaSource::local(DST, "dst"), &from)
            .await,
        &ReplicaRefusal::Invalid("an index cannot replicate itself".to_owned()),
    );
    s.dst
        .create_replication("dst", src("src"), &from)
        .await
        .unwrap();
    refused(
        s.dst.create_replication("dst", src("src"), &from).await,
        &ReplicaRefusal::Exists("dst".to_owned()),
    );
    // A name dropped within GC's window, as a branch refuses it (M16).
    s.dst.delete_index("taken").await.unwrap();
    refused(
        s.dst.create_replication("taken", src("src"), &from).await,
        &ReplicaRefusal::IndexExists("taken".to_owned()),
    );
}

#[tokio::test]
async fn a_row_past_a_stale_door_is_counted_and_never_served() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    // A process that has never read this tenant's HEAD: its door cannot know.
    let cold = s.here.engine(DST, 3);
    cold.write_without_schema_check_for_test("dst", vec![doc("intruder", 5.0)])
        .await;
    cold.flush_without_schema_check_for_test().await.unwrap();
    s.same("an unfolded row is not served").await;
    assert!(answers(&cold, "dst").await.unwrap().contains("d0001"));
    assert!(!answers(&cold, "dst").await.unwrap().contains("intruder"));
    let scanned = cold.scan("dst", None).await.unwrap();
    assert!(
        scanned.iter().all(|d| d.id != "intruder"),
        "a scan served it"
    );
    cold.fold().await.unwrap();
    s.same("the fold dropped it").await;
    assert_eq!(s.dst.replications().await.unwrap()["dst"].rejected, 1);
}

#[tokio::test]
async fn cancel_leaves_a_normal_index_whose_history_starts_there() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    let cancelled = s.dst.cancel_replication("dst").await.unwrap();
    assert!(s.dst.replications().await.unwrap().is_empty());
    s.dst.write("dst", vec![doc("mine", 1.0)]).await.unwrap();
    fold(&s.dst).await;
    assert!(answers(&s.dst, "dst").await.unwrap().contains("mine"));
    let by = OrderBy {
        attr: "id".to_owned(),
        desc: false,
    };
    let before = Epoch(cancelled.0 - 1);
    let o = s
        .dst
        .ordered("dst", &by, None, 0, 10, Some(before))
        .await
        .unwrap();
    assert!(!o.exists, "history before the cancel was answered");
    // The source moves on; the cancelled replica does not.
    write(&s.src, "src", 30..40).await;
    assert!(s.sync(&mut st).await.committed.is_empty());
    refused(
        s.dst.cancel_replication("dst").await,
        &ReplicaRefusal::NotFound("dst".to_owned()),
    );
}

#[tokio::test]
async fn a_pause_fences_a_racing_sync() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let controller = s.here.engine(DST, 2);
    let mut st = SyncState::default();
    let out = s
        .dst
        .replicate_with_interference_for_test(&s.remotes, &mut st, async {
            controller.pause_replication("dst").await.unwrap();
        })
        .await
        .unwrap();
    assert!(out.committed.is_empty(), "{out:?}");
    let r = &s.dst.replications().await.unwrap()["dst"];
    assert!(!r.running && r.applied.is_none(), "{r:?}");
    assert_eq!(answers(&s.dst, "dst").await, None);
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
    // Paused: syncing does nothing at all.
    st.invalidate();
    assert!(s.sync(&mut st).await.committed.is_empty());
    s.dst.resume_replication("dst").await.unwrap();
    st.invalidate();
    assert_eq!(s.sync(&mut st).await.committed, vec!["dst".to_owned()]);
    s.same("resumed").await;
}

#[tokio::test]
async fn a_resume_over_an_idle_source_commits_nothing() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    s.dst.pause_replication("dst").await.unwrap();
    let run = s.dst.replications().await.unwrap()["dst"].run;
    s.dst.resume_replication("dst").await.unwrap();
    assert!(
        s.dst.replications().await.unwrap()["dst"].run > run,
        "resume kept the run"
    );
    // A new worker's first sync: the source matches `applied`, so it commits nothing.
    let mut fresh = SyncState::default();
    let (r, w) = (s.here.reads(DST), s.here.writes(DST));
    let out = s.sync(&mut fresh).await;
    assert!(out.committed.is_empty() && out.failed.is_empty(), "{out:?}");
    assert_eq!(out.current, vec!["dst".to_owned()]);
    assert_eq!(s.here.reads(DST) - r, 2, "dest HEAD and source HEAD");
    assert_eq!(s.here.writes(DST), w);
    let r = s.here.reads(DST);
    assert!(s.sync(&mut fresh).await.idle);
    assert_eq!(s.here.reads(DST) - r, 1);
}

#[tokio::test]
async fn one_failing_replication_does_not_stop_another() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    write(&s.src, "bad", 0..30).await;
    s.create().await;
    let from = s.here.acct.as_tenant(DST);
    s.dst
        .create_replication("dst2", ReplicaSource::local(SRC, "bad"), &from)
        .await
        .unwrap();
    // `bad`'s segments cannot be read; `src`'s can.
    s.here
        .hooked
        .fail
        .lock()
        .unwrap()
        .push(format!("tnt/{}/idx/bad/seg/", SRC.0));
    let mut st = SyncState::default();
    let out = s.sync(&mut st).await;
    assert_eq!(out.committed, vec!["dst".to_owned()], "{out:?}");
    assert!(out.failed.contains_key("dst2"), "{out:?}");
    s.same("the healthy one committed").await;
    assert_eq!(answers(&s.dst, "dst2").await, None);
    assert_eq!(leaked(&s.here, &s.dst, "dst2").await, BTreeSet::new());
    // And a store this process does not have fails only its own replication.
    let st2 = &mut SyncState::default();
    s.dst
        .create_replication(
            "dst3",
            ReplicaSource {
                store: "nowhere".to_owned(),
                tenant: SRC,
                index: "src".to_owned(),
            },
            &from,
        )
        .await
        .unwrap();
    let out = s.sync(st2).await;
    assert!(out.failed.contains_key("dst3"), "{out:?}");
}

#[tokio::test]
async fn the_running_set_digest_changes_with_every_control() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    let none = s.dst.replication_gen().await.unwrap();
    assert_eq!(none, None);
    s.create().await;
    let a = s.dst.replication_gen().await.unwrap().unwrap();
    s.dst.pause_replication("dst").await.unwrap();
    assert_eq!(s.dst.replication_gen().await.unwrap(), None);
    s.dst.resume_replication("dst").await.unwrap();
    let b = s.dst.replication_gen().await.unwrap().unwrap();
    assert_ne!(a, b, "a resume must look new to a holder");
    // A sync changes nothing a holder must re-read for.
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    assert_eq!(s.dst.replication_gen().await.unwrap(), Some(b));
}

#[test]
fn head_round_trips_replications_and_a_head_without_them_is_unchanged() {
    use pstore_engine::{Head, Replication};
    // Without replications the encoding is byte for byte the one before M22: nothing trails.
    // The base sections and the five optional ones every HEAD carries are 56 bytes; a branch
    // adds the full-text and trigram counts before it, and nothing after.
    assert_eq!(Head::default().encode().len(), 56);
    let mut b = Head::default();
    b.branched.insert("x".to_owned(), 1);
    assert_eq!(b.encode().len(), 56 + 4 + 4 + (4 + 4 + 1 + 8));
    let mut h = Head::default();
    // With one, and every earlier optional section empty: each is written with a count of 0
    // so the replications are not read as one of them.
    h.replications.insert(
        "dst".to_owned(),
        Replication {
            source: ReplicaSource {
                store: "far".to_owned(),
                tenant: TenantId(u128::MAX - 5),
                index: "src".to_owned(),
            },
            running: false,
            run: 7,
            applied: Some((9, 0xdead_beef)),
            rejected: 3,
        },
    );
    h.replications.insert(
        "other".to_owned(),
        Replication {
            source: ReplicaSource::local(TenantId(1), "x"),
            running: true,
            run: 2,
            applied: None,
            rejected: 0,
        },
    );
    let back = Head::decode(&h.encode()).unwrap();
    assert_eq!(back, h);
    assert!(back.branched.is_empty() && back.schemas.is_empty());
}

#[tokio::test]
async fn a_refusal_from_a_stale_cache_is_re_read_before_it_stands() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    // This engine remembers "dst" as a replica; another process cancels it.
    assert!(s.dst.replications().await.unwrap().contains_key("dst"));
    let other = s.here.engine(DST, 2);
    other.cancel_replication("dst").await.unwrap();
    s.dst.write("dst", vec![doc("mine", 1.0)]).await.unwrap();
}

#[tokio::test]
async fn a_compaction_or_branch_that_races_a_new_replica_never_lands_in_it() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    write(&s.dst, "x", 0..10).await;
    write(&s.dst, "x", 10..20).await;
    let other = s.here.engine(DST, 2);
    let from = s.here.acct.as_tenant(DST);
    // Between the compaction's read and its commit, x is dropped, collected and made a
    // replica: the retry must not merge into it. Its inputs are gone from HEAD, so it stops
    // there -- before any replica check could matter, which is why there is none per attempt.
    let r = s
        .dst
        .compact_with_interference_for_test("x", async {
            other.delete_index("x").await.unwrap();
            other.gc(0).await.unwrap();
            other
                .create_replication("x", ReplicaSource::local(SRC, "src"), &from)
                .await
                .unwrap();
        })
        .await;
    assert_eq!(r.unwrap(), None);
    assert!(!s.dst.head_for_test().await.indexes.contains_key("x"));
    refused(
        s.dst.compact("x").await,
        &ReplicaRefusal::ReadOnly("x".to_owned()),
    );
    // A branch into a name that becomes a replica mid-branch.
    write(&s.dst, "y", 0..10).await;
    let r = s
        .dst
        .branch_with_interference_for_test("y", "z", async {
            other
                .create_replication("z", ReplicaSource::local(SRC, "src"), &from)
                .await
                .unwrap();
        })
        .await;
    refused(r, &ReplicaRefusal::ReadOnly("z".to_owned()));
    assert_eq!(leaked(&s.here, &s.dst, "x").await, BTreeSet::new());
}

#[tokio::test]
async fn a_cached_plan_never_commits_another_sources_data() {
    // Code review B1: the plan said dst follows A's src; it is cancelled, dropped and made
    // again from B's. A sync with the old plan must not copy A's segments into it.
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    s.dst.cancel_replication("dst").await.unwrap();
    s.dst.delete_index("dst").await.unwrap();
    s.dst.gc(0).await.unwrap();
    let b = s.here.engine(TenantId(73), 9);
    write(&b, "src", 500..510).await;
    let from = s.here.acct.as_tenant(DST);
    s.dst
        .create_replication("dst", ReplicaSource::local(TenantId(73), "src"), &from)
        .await
        .unwrap();
    // The old source moves on, so the old plan has something to copy -- and nothing of it
    // may be committed.
    write(&s.src, "src", 30..40).await;
    let out = s.sync(&mut st).await;
    assert!(out.committed.is_empty(), "{out:?}");
    // A fresh read of the plan follows B.
    st.invalidate();
    assert_eq!(s.sync(&mut st).await.committed, vec!["dst".to_owned()]);
    assert_eq!(answers(&s.dst, "dst").await, answers(&b, "src").await);
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn a_regressed_source_fails_without_copying() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    write(&s.src, "src", 30..60).await;
    // The HEAD restored later names two segments the replica will never have held: it is
    // taken before a compaction, and nothing is collected, so both still exist.
    let head_key = Key::new(format!("{:04x}/tnt/{}/HEAD", SRC.0 as u16, SRC.0));
    let old = s.here.hooked.inner.get(&head_key).await.unwrap();
    s.src.compact("src").await.unwrap();
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    // Disaster recovery restores the source's older HEAD.
    s.here.hooked.inner.put(&head_key, old).await.unwrap();
    let (keys, epoch, w) = (
        s.here.keys(&dst_prefix()).await,
        s.dst.head_for_test().await.epoch,
        s.here.writes(DST),
    );
    let out = s.sync(&mut st).await;
    assert!(out.failed.contains_key("dst"), "{out:?}");
    assert_eq!(
        s.here.keys(&dst_prefix()).await,
        keys,
        "a regressed source was copied"
    );
    assert_eq!(s.dst.head_for_test().await.epoch, epoch);
    assert_eq!(s.here.writes(DST), w);
}

#[tokio::test]
async fn a_failed_replications_copies_are_buried() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    write(&s.src, "bad", 0..30).await;
    s.src.delete("bad", vec!["d0003".into()]).await.unwrap();
    fold(&s.src).await;
    let from = s.here.acct.as_tenant(DST);
    s.dst
        .create_replication("dst2", ReplicaSource::local(SRC, "bad"), &from)
        .await
        .unwrap();
    s.create().await;
    // `bad`'s segment copies; its delete vector cannot be read.
    s.here.hooked.fail.lock().unwrap().push(".dv".to_owned());
    let mut st = SyncState::default();
    let out = s.sync(&mut st).await;
    assert!(out.failed.contains_key("dst2"), "{out:?}");
    assert_eq!(out.committed, vec!["dst".to_owned()]);
    let written = s
        .here
        .keys(&format!("{:04x}/tnt/{}/idx/dst2/", DST.0 as u16, DST.0))
        .await;
    assert!(!written.is_empty(), "the failure struck before any copy");
    assert_eq!(leaked(&s.here, &s.dst, "dst2").await, BTreeSet::new());
}

#[tokio::test]
async fn an_abandoned_sync_buries_what_it_wrote() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    // The commit fails outright; the burial after it does not.
    s.here
        .hooked
        .fail_cas
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let mut st = SyncState::default();
    assert!(s.dst.replicate(&s.remotes, &mut st).await.is_err());
    assert!(!s.here.keys(&dst_prefix()).await.is_empty());
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn a_sync_that_cannot_reread_its_head_buries_what_it_wrote() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    // The first commit loses to another index's fold; the re-read that follows fails.
    let other = s.here.engine(DST, 2);
    let hooked = s.here.hooked.clone();
    let head = format!("tnt/{}/HEAD", DST.0);
    let mut st = SyncState::default();
    let r = s
        .dst
        .replicate_with_interference_for_test(&s.remotes, &mut st, async {
            write(&other, "elsewhere", 0..5).await;
            *hooked.fail_once.lock().unwrap() = Some(head);
        })
        .await;
    assert!(r.is_err(), "{r:?}");
    assert!(!s.here.keys(&dst_prefix()).await.is_empty());
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn create_refuses_every_name_a_branch_refuses() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..5).await;
    let from = s.here.acct.as_tenant(DST);
    let long = "x".repeat(129);
    for bad in ["", ".", "..", "a/b", "a b", long.as_str()] {
        let r = s
            .dst
            .create_replication(bad, ReplicaSource::local(SRC, "src"), &from)
            .await;
        assert!(
            matches!(r, Err(EngineError::Replica(ReplicaRefusal::Invalid(_)))),
            "{bad:?}: {r:?}"
        );
    }
    // And a name with unfolded rows is taken, as a branch decides.
    s.dst.write("pend", vec![doc("p", 1.0)]).await.unwrap();
    refused(
        s.dst
            .create_replication("pend", ReplicaSource::local(SRC, "src"), &from)
            .await,
        &ReplicaRefusal::IndexExists("pend".to_owned()),
    );
}

#[tokio::test]
async fn a_control_call_retries_a_lost_commit_up_to_its_limit() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..5).await;
    s.create().await;
    // Every attempt but the last loses: it still lands.
    s.here
        .hooked
        .lose_next
        .store(23, std::sync::atomic::Ordering::SeqCst);
    s.dst.pause_replication("dst").await.unwrap();
    assert!(!s.dst.replications().await.unwrap()["dst"].running);
    // Every attempt loses: it gives up, as lost.
    s.here
        .hooked
        .lose_next
        .store(24, std::sync::atomic::Ordering::SeqCst);
    let r = s.dst.resume_replication("dst").await;
    assert!(matches!(r, Err(EngineError::Lost)), "{r:?}");
    assert!(!s.dst.replications().await.unwrap()["dst"].running);
    // Every attempt contended: the last one's own error is what the caller sees.
    s.here
        .hooked
        .contend_next
        .store(24, std::sync::atomic::Ordering::SeqCst);
    let r = s.dst.resume_replication("dst").await;
    assert!(matches!(r, Err(EngineError::Contended)), "{r:?}");
}

#[tokio::test]
async fn replication_status_answers_from_one_read() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    let r0 = s.here.reads(DST);
    let (r, stats, generation) = s.dst.replication_status("dst").await.unwrap().unwrap();
    assert_eq!(s.here.reads(DST) - r0, 1);
    assert!(r.running && r.applied.is_some(), "{r:?}");
    let stats = stats.unwrap();
    assert_eq!((stats.documents, stats.segments), (30, 1));
    assert_eq!(generation, s.dst.replication_gen().await.unwrap());
    assert!(generation.is_some());
    assert!(s.dst.replication_status("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn a_sync_after_the_plan_is_re_read_with_nothing_changed_is_idle() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..10).await;
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    // A holder that saw the register's generation move re-reads the plan; the replication it
    // already knows is still current, so the sync stops at the source read.
    st.invalidate();
    assert!(s.sync(&mut st).await.idle);
}

fn src_keys_ending(keys: &BTreeSet<String>, suffix: &str) -> Vec<String> {
    keys.iter()
        .filter(|k| k.ends_with(suffix))
        .cloned()
        .collect()
}

fn src_prefix() -> String {
    format!("{:04x}/tnt/{}/idx/src/", SRC.0 as u16, SRC.0)
}

/// 26 segments in the source, the j-th answering 404 to its first j reads: each round of a
/// sync copies exactly one and remaps for the rest -- 26 rounds, every one making progress.
async fn one_segment_a_round(s: &Setup) {
    for k in 0..26 {
        write(&s.src, "src", k * 2..k * 2 + 2).await;
    }
    let segs = src_keys_ending(&s.here.keys(&src_prefix()).await, ".seg");
    assert_eq!(segs.len(), 26, "one segment a fold");
    let mut m = s.here.hooked.missing.lock().unwrap();
    for (j, k) in segs.into_iter().enumerate() {
        m.insert(k, u32::try_from(j).unwrap());
    }
}

#[tokio::test]
async fn a_sync_making_progress_every_round_never_gives_up_or_copies_twice() {
    // More rounds than MAX_STALLS, each copying one segment: a stall is a round that copies
    // nothing, and these never are.
    let plain = setup(Kind::CrossTenant);
    for k in 0..26 {
        write(&plain.src, "src", k * 2..k * 2 + 2).await;
    }
    plain.create().await;
    let w0 = plain.here.writes(DST);
    plain.sync(&mut SyncState::default()).await;
    let once = plain.here.writes(DST) - w0;

    let s = setup(Kind::CrossTenant);
    one_segment_a_round(&s).await;
    s.create().await;
    let w0 = s.here.writes(DST);
    let out = s.sync(&mut SyncState::default()).await;
    assert_eq!(out.committed, vec!["dst".to_owned()], "{out:?}");
    s.same("after 26 rounds").await;
    // No copy made twice: a copy whose sidecar was absent all along is kept across a remap.
    assert_eq!(s.here.writes(DST) - w0, once);
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn a_sync_copying_nothing_round_after_round_gives_up() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..10).await;
    s.create().await;
    let seg = src_keys_ending(&s.here.keys(&src_prefix()).await, ".seg");
    // Named by every source HEAD, and never there: more rounds than MAX_STALLS, then it is.
    s.here
        .hooked
        .missing
        .lock()
        .unwrap()
        .insert(seg[0].clone(), 30);
    let out = s.sync(&mut SyncState::default()).await;
    assert!(out.committed.is_empty(), "{out:?}");
    assert!(
        out.failed
            .get("dst")
            .is_some_and(|why| why.contains("kept changing")),
        "{out:?}"
    );
    // It stopped at the stall limit, not when the source settled.
    assert!(s.here.hooked.missing.lock().unwrap()[&seg[0]] > 0);
}

#[tokio::test]
async fn a_sync_copying_one_delete_vector_a_round_never_gives_up() {
    let s = setup(Kind::CrossTenant);
    for k in 0..26 {
        write(&s.src, "src", k * 2..k * 2 + 2).await;
    }
    s.create().await;
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    // A delete in every segment: 26 vectors, the j-th answering 404 to its first j reads.
    s.src
        .delete("src", (0..26).map(|k| format!("d{:04}", k * 2)).collect())
        .await
        .unwrap();
    fold(&s.src).await;
    let dvs = src_keys_ending(&s.here.keys(&src_prefix()).await, ".dv");
    assert_eq!(dvs.len(), 26, "one vector a segment");
    {
        let mut m = s.here.hooked.missing.lock().unwrap();
        for (j, k) in dvs.into_iter().enumerate() {
            m.insert(k, u32::try_from(j).unwrap());
        }
    }
    let out = s.sync(&mut st).await;
    assert_eq!(out.committed, vec!["dst".to_owned()], "{out:?}");
    s.same("after 26 rounds of vectors").await;
}

#[tokio::test]
async fn a_sync_retries_a_lost_commit_up_to_its_limit() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..10).await;
    s.create().await;
    let mut st = SyncState::default();
    let lose = |n| {
        s.here
            .hooked
            .lose_next
            .store(n, std::sync::atomic::Ordering::SeqCst)
    };
    lose(23);
    assert_eq!(s.sync(&mut st).await.committed, vec!["dst".to_owned()]);
    write(&s.src, "src", 10..20).await;
    lose(24);
    let r = s.dst.replicate(&s.remotes, &mut st).await;
    assert!(matches!(r, Err(EngineError::Lost)), "{r:?}");
    s.here
        .hooked
        .contend_next
        .store(24, std::sync::atomic::Ordering::SeqCst);
    let r = s.dst.replicate(&s.remotes, &mut st).await;
    assert!(matches!(r, Err(EngineError::Contended)), "{r:?}");
    // And what the failed syncs wrote is buried; the next sync lands.
    assert_eq!(s.sync(&mut st).await.committed, vec!["dst".to_owned()]);
    s.same("after two abandoned syncs").await;
    s.dst.gc(0).await.unwrap();
    s.same("after a reap").await;
    assert_eq!(leaked(&s.here, &s.dst, "dst").await, BTreeSet::new());
}

#[tokio::test]
async fn a_sync_after_abandoned_ones_never_reuses_a_buried_key() {
    // Two abandoned syncs, each burying its copies in a commit of its own. The next sync must
    // name its copies past every buried one: a reap between its PUTs and its commit takes a
    // buried key it rewrote, and it commits a segment that is gone.
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let mut st = SyncState::default();
    for _ in 0..2 {
        s.here
            .hooked
            .fail_cas
            .store(1, std::sync::atomic::Ordering::SeqCst);
        assert!(s.dst.replicate(&s.remotes, &mut st).await.is_err());
    }
    let reaper = s.here.engine(DST, 2);
    let out = s
        .dst
        .replicate_with_interference_for_test(&s.remotes, &mut st, async {
            reaper.gc(0).await.unwrap();
        })
        .await
        .unwrap();
    assert_eq!(out.committed, vec!["dst".to_owned()], "{out:?}");
    s.same("after a reap that raced the commit").await;
}

#[tokio::test]
async fn a_commit_reports_only_what_did_not_change_as_current() {
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..10).await;
    write(&s.src, "other", 0..10).await;
    s.create().await;
    let from = s.here.acct.as_tenant(DST);
    s.dst
        .create_replication("dst2", ReplicaSource::local(SRC, "other"), &from)
        .await
        .unwrap();
    let mut st = SyncState::default();
    s.sync(&mut st).await;
    write(&s.src, "src", 10..20).await;
    let out = s.sync(&mut st).await;
    assert_eq!(out.committed, vec!["dst".to_owned()], "{out:?}");
    assert_eq!(out.current, vec!["dst2".to_owned()], "{out:?}");
}

#[tokio::test]
async fn a_resume_racing_a_sync_fences_it_though_the_source_is_the_same() {
    // Paused and resumed under the sync: the same source, a new run. The plan's run is gone.
    let s = setup(Kind::CrossTenant);
    write(&s.src, "src", 0..30).await;
    s.create().await;
    let controller = s.here.engine(DST, 2);
    let mut st = SyncState::default();
    let out = s
        .dst
        .replicate_with_interference_for_test(&s.remotes, &mut st, async {
            controller.pause_replication("dst").await.unwrap();
            controller.resume_replication("dst").await.unwrap();
        })
        .await
        .unwrap();
    assert!(out.committed.is_empty(), "{out:?}");
    assert!(s.dst.replications().await.unwrap()["dst"].applied.is_none());
}
