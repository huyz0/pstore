//! A fold that keeps losing to folds that already did its work (M111).
//!
//! A fold is tenant-scoped: every attempt folds every lane's unfolded span, so a loser
//! rebases onto the winner's HEAD and tries again with whatever is left. When other lanes
//! keep flushing, something is always left, and a fold that is slower than its peers loses
//! every attempt and returns `Lost` -- although the first winner already folded every row
//! the caller flushed. CI's ubuntu runner did exactly that in
//! `every_acknowledged_document_survives_contention`; this forces the same schedule rather
//! than waiting for a slow machine to.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_types::{CasTag, LaneId, TenantId};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const T: TenantId = TenantId(7);

fn doc(id: &str) -> Document {
    doc_of_width(id, 4)
}

fn doc_of_width(id: &str, width: usize) -> Document {
    Document {
        id: id.to_owned(),
        vectors: std::collections::BTreeMap::from([(
            pstore_format::DEFAULT_FIELD.into(),
            pstore_format::VectorField::dense(vec![0.0; width]),
        )]),
        attrs: Default::default(),
    }
}

/// The victim's view of the store. Before each of its HEAD commits, another writer on the
/// same store folds everything -- the victim's rows included -- and then flushes one more
/// row, so the victim's commit loses and its rebase still finds work.
struct Overtaken {
    inner: Arc<MemoryStore>,
    rival: Engine<MemoryStore>,
    armed: AtomicBool,
    /// HEAD commits the victim attempted while armed.
    attempts: AtomicU64,
    /// Holds the victim's next HEAD commit: says so on `reached`, waits for `go`.
    hold: AtomicBool,
    /// The rival's next fold also creates the index `fresh`, which the victim never wrote.
    fresh: AtomicBool,
    reached: tokio::sync::Notify,
    go: tokio::sync::Notify,
}

impl Overtaken {
    fn new() -> Self {
        let inner = Arc::new(MemoryStore::new());
        Self {
            rival: Engine::new(Arc::clone(&inner), T, LaneId(2)),
            inner,
            armed: AtomicBool::new(false),
            attempts: AtomicU64::new(0),
            hold: AtomicBool::new(false),
            fresh: AtomicBool::new(false),
            reached: tokio::sync::Notify::new(),
            go: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Overtaken {
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
        if key.as_str().ends_with("/HEAD") && self.hold.swap(false, Ordering::SeqCst) {
            self.reached.notify_one();
            self.go.notified().await;
        }
        if self.armed.load(Ordering::SeqCst) && key.as_str().ends_with("/HEAD") {
            let n = self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.fresh.swap(false, Ordering::SeqCst) {
                self.rival.write("fresh", vec![doc("f")]).await.unwrap();
            }
            // The rival goes through the inner store, so it never comes back here.
            self.rival
                .write("idx", vec![doc(&format!("rival-{n}"))])
                .await
                .unwrap();
            self.rival.flush().await.unwrap();
            self.rival.fold().await.unwrap();
            self.rival
                .write("idx", vec![doc(&format!("rival-{n}-late"))])
                .await
                .unwrap();
            self.rival.flush().await.unwrap();
        }
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        self.inner.get_range_as(key, range, class).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        self.inner.get_suffix_as(key, n, class).await
    }
}

#[tokio::test]
async fn a_fold_whose_rows_a_winner_folded_does_not_keep_losing() {
    let store = Arc::new(Overtaken::new());
    let victim = Engine::new(Arc::clone(&store), T, LaneId(1));
    victim.write("idx", vec![doc("mine")]).await.unwrap();
    victim.flush().await.unwrap();

    store.armed.store(true, Ordering::SeqCst);
    let folded = victim.fold().await;
    store.armed.store(false, Ordering::SeqCst);

    let attempts = store.attempts.load(Ordering::SeqCst);
    // ⚠️ The property, not the count: the first loss's winner folded "mine", so the caller's
    // obligation was met there. One lost commit, then the rebase sees it and stops.
    assert!(
        folded.is_ok(),
        "the fold returned {folded:?} after {attempts} lost commits, \
         though the first winner had already folded its row"
    );
    assert_eq!(
        attempts, 1,
        "the fold kept committing after its rows were folded"
    );
    // A yield prunes as a commit does: the winner folded "mine", so it is not held unfolded,
    // and the tenant is not reported due for rows already folded.
    assert!(
        victim.unfolded().is_none(),
        "the victim still holds rows the winner folded: {:?}",
        victim.unfolded()
    );

    let reader = Engine::new(Arc::clone(&store.inner), T, LaneId(9));
    reader.fold().await.unwrap();
    let ids: Vec<String> = reader
        .scan("idx", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert!(
        ids.contains(&"mine".to_owned()),
        "the victim's row is not visible: {ids:?}"
    );
}

#[tokio::test]
async fn a_drop_still_commits_against_the_same_rival() {
    // A drop needs a commit of its own: the rival folding the victim's rows does not drop
    // the index. So a drop never answers "nothing to delete" for an index that exists --
    // it drops it, or it reports the contention.
    let store = Arc::new(Overtaken::new());
    let victim = Engine::new(Arc::clone(&store), T, LaneId(1));
    victim.write("idx", vec![doc("mine")]).await.unwrap();
    victim.flush().await.unwrap();

    store.armed.store(true, Ordering::SeqCst);
    let dropped = victim.delete_index("idx").await;
    store.armed.store(false, Ordering::SeqCst);

    assert!(
        !matches!(dropped, Ok(None)),
        "a drop of an index that exists answered that there was nothing to delete"
    );
}

#[tokio::test]
async fn a_fold_whose_winner_missed_a_bundle_it_read_folds_it() {
    // The safety half. The winner read HEAD and the lanes BEFORE a third lane's bundle landed;
    // the victim read after, so its first attempt names that bundle. The winner covering
    // every lane it read is not enough: the victim may stop only once the bundles ITS first
    // read named are folded. It flushed nothing itself, which is the case a check of its own
    // batches would wave through.
    let store = Arc::new(Overtaken::new());
    // An index for the rival to drop, so its commit is one whose read can be held open.
    store.rival.write("other", vec![doc("o")]).await.unwrap();
    store.rival.flush().await.unwrap();
    store.rival.fold().await.unwrap();

    let third = Engine::new(Arc::clone(&store.inner), T, LaneId(3));
    let victim = Arc::new(Engine::new(Arc::clone(&store), T, LaneId(1)));
    store.hold.store(true, Ordering::SeqCst);
    let mut folding = None;
    let dropped = store
        .rival
        .delete_index_with_interference_for_test("other", async {
            // The rival has read. Now the bundle lands, and the victim reads it and stops
            // at its commit.
            third.write("idx", vec![doc("third")]).await.unwrap();
            third.flush().await.unwrap();
            let v = Arc::clone(&victim);
            folding = Some(tokio::spawn(async move { v.fold().await }));
            store.reached.notified().await;
        })
        .await
        .unwrap();
    assert!(dropped.is_some(), "the rival's commit did not land");
    store.go.notify_one();
    let folded = folding.unwrap().await.unwrap().unwrap();

    // A reader that folds nothing sees exactly what the victim's epoch holds.
    let reader = Engine::new(Arc::clone(&store.inner), T, LaneId(9));
    let ids: Vec<String> = reader
        .scan("idx", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert!(
        ids.contains(&"third".to_owned()),
        "the fold returned epoch {folded:?} with a bundle its first read named still unfolded"
    );
}

#[tokio::test]
async fn a_fold_that_stops_knows_the_schemas_its_winner_recorded() {
    // A yield remembers the winner's schemas as a commit would. The winner created `fresh`
    // at width 4; the victim had seen HEAD before then, so without them its door has no
    // schema for `fresh` and takes a width-8 row that every other process refuses.
    let store = Arc::new(Overtaken::new());
    let victim = Engine::new(Arc::clone(&store), T, LaneId(1));
    victim.write("idx", vec![doc("mine")]).await.unwrap();
    victim.flush().await.unwrap();

    store.fresh.store(true, Ordering::SeqCst);
    store.armed.store(true, Ordering::SeqCst);
    victim.fold().await.unwrap();
    store.armed.store(false, Ordering::SeqCst);
    assert_eq!(
        store.attempts.load(Ordering::SeqCst),
        1,
        "the fold did not stop"
    );

    let wrong = victim.write("fresh", vec![doc_of_width("w", 8)]).await;
    assert!(
        wrong.is_err(),
        "a width-8 row into a width-4 index the winner recorded was accepted"
    );
}
