//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! What a commit loop says when it runs out of attempts.
//!
//! ⚠️ **The distinction is the whole retry protocol.** `Lost` means someone else won, so the
//! caller must re-read HEAD and rebase; `Contended` means the backend could not evaluate the
//! condition, so the same attempt should be retried unchanged. A loop that exhausts its budget
//! against a contending backend and reports `Lost` tells its caller to rebase against a world
//! that never changed.
//!
//! Found by a mutation sweep run for M6c: `Engine<S>::compact`'s guard
//! `attempt < MAX_COMMIT_ATTEMPTS - 1` survived four mutations, including replacing it with
//! `true`. Every one of them makes the loop fall out of its `for` and return `Err(Lost)`
//! instead of the error it actually saw, and nothing in the tree said so.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::{Engine, EngineError};
use pstore_format::Document;
use pstore_types::{CasTag, LaneId, TenantId};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A correct store whose conditional writes start contending once armed.
///
/// ⚠️ `Flaky::always_contended` cannot do this: the setup a compaction needs — two folds, each
/// a CAS — has to succeed before the contention starts.
#[derive(Debug, Default)]
struct ArmedContention {
    inner: MemoryStore,
    armed: AtomicBool,
    refused: AtomicU64,
}

impl ArmedContention {
    fn arm(&self) {
        self.armed.store(true, Ordering::Relaxed);
    }

    /// Conditional writes refused since arming.
    ///
    /// ⚠️ **Asserting the error kind is not enough**, and the mutation gate said so: a guard
    /// mutated to `false` gives up on the *first* contention and returns `Contended` — the
    /// same error this test wanted, from a loop that never retried. `gc`'s guard survived
    /// three such mutations against a test that checked only the kind.
    fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl BlobStore for ArmedContention {
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
        if self.armed.load(Ordering::Relaxed) {
            self.refused.fetch_add(1, Ordering::Relaxed);
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
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        self.inner.get_immutable(key, class).await
    }
}

fn doc(i: usize) -> Document {
    Document::new(format!("d{i:04}"), vec![i as f32, 1.0, 0.5])
}

#[tokio::test]
async fn a_compaction_that_keeps_contending_reports_contention_not_a_lost_race() {
    let store = Arc::new(ArmedContention::default());
    let t = TenantId(640);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    for batch in 0..2 {
        e.write("idx", (batch * 6..batch * 6 + 6).map(doc).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }

    store.arm();
    let err = e
        .compact("idx")
        .await
        .expect_err("a compaction committed against a backend refusing every CAS");
    assert!(
        matches!(err, EngineError::Contended),
        "the loop ran out of attempts and reported {err:?} rather than what it actually saw. \
         `Lost` tells the caller to rebase against a HEAD that never moved"
    );
    assert!(
        store.refused() > 1,
        "the compaction gave up after {} refusal(s): it reported the right error from a loop \
         that never retried, which is what a guard mutated to `false` also does",
        store.refused()
    );
}

#[tokio::test]
async fn a_fold_that_keeps_contending_reports_contention_not_a_lost_race() {
    // ⚠️ The same guard, one function up. The sweep that found `compact`'s only examined
    // `compact`; `fold` and `gc` carry the identical `attempt < MAX_COMMIT_ATTEMPTS - 1` and
    // the identical fall-through to `Err(Lost)`, so covering one and leaving the others is
    // fixing the site rather than the shape.
    let store = Arc::new(ArmedContention::default());
    let t = TenantId(641);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    e.write("idx", (0..6).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();

    store.arm();
    let err = e
        .fold()
        .await
        .expect_err("a fold committed against a backend refusing every CAS");
    assert!(
        matches!(err, EngineError::Contended),
        "the fold loop reported {err:?} rather than what it saw"
    );
    assert!(
        store.refused() > 1,
        "the fold gave up after {} refusal(s): it reported the right error from a loop \
         that never retried, which is what a guard mutated to `false` also does",
        store.refused()
    );
}

#[tokio::test]
async fn a_gc_that_keeps_contending_reports_contention_not_a_lost_race() {
    let store = Arc::new(ArmedContention::default());
    let t = TenantId(642);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    for batch in 0..2 {
        e.write("idx", (batch * 6..batch * 6 + 6).map(doc).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.compact("idx").await.unwrap();

    store.arm();
    let err = e
        .gc(0)
        .await
        .expect_err("a gc committed against a backend refusing every CAS");
    assert!(
        matches!(err, EngineError::Contended),
        "the gc loop reported {err:?} rather than what it saw"
    );
    assert!(
        store.refused() > 1,
        "the gc gave up after {} refusal(s): it reported the right error from a loop \
         that never retried, which is what a guard mutated to `false` also does",
        store.refused()
    );
}
