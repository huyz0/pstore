//! M201: GC announces its horizon in HEAD before it deletes anything, and prunes exactly the
//! graveyard entries it decided on afterwards.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_types::{CasTag, LaneId, TenantId};
use std::ops::Range;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

type Hook = Box<dyn FnOnce() -> futures_util::future::BoxFuture<'static, ()> + Send>;

/// A store that runs a hook before its next delete batch, can refuse delete batches, and
/// counts conditional writes.
#[derive(Clone, Default)]
struct Gate {
    inner: Arc<MemoryStore>,
    on_delete: Arc<Mutex<Option<Hook>>>,
    refuse_deletes: Arc<AtomicU32>,
    puts: Arc<AtomicU32>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl BlobStore for Gate {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
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
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        let hook = self.on_delete.lock().unwrap().take();
        if let Some(f) = hook {
            f().await;
        }
        if self
            .refuse_deletes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(BlobError::Other("injected outage".to_owned()));
        }
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

fn doc(id: &str) -> Document {
    Document::new(id, vec![0.0; 4])
}

/// An engine over `store` with `n` folds, so its graveyard holds what they superseded.
async fn seeded(store: &Gate, t: TenantId, n: usize) -> Engine<Gate> {
    let e = Engine::new(Arc::new(store.clone()), t, LaneId(1));
    for i in 0..n {
        e.write("idx", vec![doc(&format!("d{i}"))]).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e
}

/// Every graveyard pair, as `(epoch, key)`.
fn pairs(h: &pstore_engine::Head) -> Vec<(u64, String)> {
    h.graveyard
        .iter()
        .flat_map(|(e, keys)| keys.iter().map(move |k| (*e, k.clone())))
        .collect()
}

async fn exists(store: &Gate, key: &str) -> bool {
    store.inner.head(&Key::new(key.to_owned())).await.is_ok()
}

#[tokio::test]
async fn gc_announces_its_horizon_before_it_deletes() {
    let store = Gate::default();
    let t = TenantId(20_100);
    let e = seeded(&store, t, 4).await;
    let before = e.head_for_test().await;
    let buried = pairs(&before);
    assert!(!buried.is_empty(), "the fixture buries nothing");
    let seen = Arc::new(Mutex::new(None));
    {
        let slot = Arc::clone(&store.on_delete);
        let (store, seen) = (store.clone(), Arc::clone(&seen));
        *slot.lock().unwrap() = Some(Box::new(move || {
            Box::pin(async move {
                let other = Engine::new(Arc::new(store), t, LaneId(9));
                *seen.lock().unwrap() = Some(other.head_for_test().await.reaped_before);
            })
        }));
    }
    let reaped = e.gc(0).await.unwrap();
    // With retention 0 the horizon is the epoch the pass read.
    assert_eq!(
        *seen.lock().unwrap(),
        Some(before.epoch.0),
        "GC deleted before HEAD said how far it reaps"
    );
    let distinct: std::collections::BTreeSet<&String> = buried.iter().map(|(_, k)| k).collect();
    assert_eq!(reaped, distinct.len(), "the count GC returns");
    let after = e.head_for_test().await;
    assert!(after.graveyard.is_empty(), "{:?}", after.graveyard);
    for (_, k) in &buried {
        assert!(!exists(&store, k).await, "{k} survived");
    }
}

#[tokio::test]
async fn an_announced_reap_is_finished_by_the_next_pass() {
    // The second pass's horizon is below what was announced (a wider retention), or equal to
    // it: either way nothing new is announced.
    for (i, below) in [true, false].into_iter().enumerate() {
        let store = Gate::default();
        let t = TenantId(20_101 + 10 * i as u128);
        let e = seeded(&store, t, 4).await;
        let before = e.head_for_test().await;
        let buried = pairs(&before);
        assert!(!buried.is_empty(), "the fixture buries nothing");

        // The pass's first delete batch fails: the announcement has landed, nothing is deleted.
        store.refuse_deletes.store(1, Ordering::SeqCst);
        assert!(
            e.gc(0).await.is_err(),
            "a refused delete is the pass's error"
        );
        let announced = e.head_for_test().await;
        assert_eq!(announced.reaped_before, before.epoch.0, "not announced");
        assert_eq!(
            e.epoch(),
            announced.epoch,
            "the announcement is a commit this engine reports"
        );
        assert_eq!(pairs(&announced), buried, "the graveyard was not kept");
        for (_, k) in &buried {
            assert!(exists(&store, k).await, "{k} was deleted");
        }

        let puts = store.puts.load(Ordering::SeqCst);
        let distinct: std::collections::BTreeSet<&String> = buried.iter().map(|(_, k)| k).collect();
        let reaped = if below {
            // A retention so wide its horizon is 0.
            e.gc(1_000_000).await.unwrap()
        } else {
            e.gc_through(pstore_types::Epoch(announced.reaped_before))
                .await
                .unwrap()
        };
        assert_eq!(reaped, distinct.len(), "below: {below}");
        assert_eq!(
            store.puts.load(Ordering::SeqCst) - puts,
            1,
            "below: {below}: nothing new to announce, so one prune and no announcement"
        );
        assert!(e.head_for_test().await.graveyard.is_empty());
        for (_, k) in &buried {
            assert!(!exists(&store, k).await, "{k} survived");
        }

        // And a third pass has nothing to do.
        let puts = store.puts.load(Ordering::SeqCst);
        assert_eq!(e.gc(1_000_000).await.unwrap(), 0);
        assert_eq!(
            store.puts.load(Ordering::SeqCst),
            puts,
            "an idle pass wrote"
        );
    }
}

#[tokio::test]
async fn a_rivals_burial_survives_the_prune() {
    let store = Gate::default();
    let t = TenantId(20_102);
    let e = seeded(&store, t, 4).await;
    let before = e.head_for_test().await;
    let own = pairs(&before);
    let rival = format!("tnt/{}/rival.obj", t.0);
    {
        let slot = Arc::clone(&store.on_delete);
        let (store, rival) = (store.clone(), rival.clone());
        *slot.lock().unwrap() = Some(Box::new(move || {
            Box::pin(async move {
                store
                    .inner
                    .put(&Key::new(rival.clone()), Bytes::from_static(b"x"))
                    .await
                    .unwrap();
                // Filed at epoch 1, at or below what the pass announced.
                Engine::new(Arc::new(store), t, LaneId(9))
                    .commit_head_for_test(|h| h.graveyard.entry(1).or_default().push(rival))
                    .await
                    .unwrap();
            })
        }));
    }
    let distinct: std::collections::BTreeSet<&String> = own.iter().map(|(_, k)| k).collect();
    assert_eq!(
        e.gc(0).await.unwrap(),
        distinct.len(),
        "the pass's own count"
    );
    let after = pairs(&e.head_for_test().await);
    for p in &own {
        assert!(!after.contains(p), "{p:?} was not pruned");
    }
    let listed = after.iter().any(|(_, k)| *k == rival);
    assert!(
        listed || !exists(&store, &rival).await,
        "the rival's object is present and nothing lists it"
    );

    e.gc(0).await.unwrap();
    assert!(
        !pairs(&e.head_for_test().await)
            .iter()
            .any(|(_, k)| *k == rival),
        "still listed"
    );
    assert!(!exists(&store, &rival).await, "never deleted");
}
