//! OQ-6 (M105): does the ABA guard hold when the backend's tags are multipart ETags, or
//! opaque SSE-KMS-style ones?
//!
//! A content-derived tag (S3's single-part and multipart ETags alike) repeats exactly when the
//! bytes repeat. So the guard is not the nonce: it is that **no two HEADs the engine writes
//! are byte-identical**, which holds because every HEAD write advances the epoch. These tests
//! pin that from the outside, under every tag style, rather than trusting each commit path.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
    TagStyle,
};
use pstore_engine::{Engine, EngineError, Head};
use pstore_format::Document;
use pstore_types::{CasTag, Epoch, LaneId, TenantId};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

const T: TenantId = TenantId(6);

/// The styles OQ-6 is about: the two content-derived ones and the opaque one.
const STYLES: [TagStyle; 3] = [
    TagStyle::ContentHash,
    TagStyle::Multipart { part_bytes: 64 },
    TagStyle::Opaque,
];

fn doc(id: &str) -> Document {
    Document {
        id: id.to_owned(),
        vectors: std::collections::BTreeMap::from([(
            pstore_format::DEFAULT_FIELD.to_owned(),
            pstore_format::VectorField::dense(vec![1.0, 0.0, 0.0, 0.0]),
        )]),
        attrs: Default::default(),
    }
}

/// Records every HEAD body that lands, with the tag the store gave it, and counts the HEAD
/// CASes that lost.
#[derive(Debug)]
struct Recording {
    inner: MemoryStore,
    head: Key,
    landed: Mutex<Vec<(Bytes, CasTag)>>,
    lost: Mutex<usize>,
}

impl Recording {
    fn new(style: TagStyle) -> Self {
        Self {
            inner: MemoryStore::with_tag_style(style),
            head: Head::key(T),
            landed: Mutex::new(Vec::new()),
            lost: Mutex::new(0),
        }
    }

    fn lost(&self) -> usize {
        *self.lost.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl BlobStore for Recording {
    fn capabilities(&self) -> &Capabilities {
        // ⚠️ `ContentHash` and `Multipart` would be refused by `require_fencing` if the
        // engine read a measured profile; it reads the declared one, which is the point of
        // driving the engine over an ABA-prone store at all.
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
        let r = self.inner.put_conditional(key, body.clone(), pre).await;
        if *key == self.head {
            match &r {
                Ok(out) => self.landed.lock().unwrap().push((body, out.tag.clone())),
                Err(CasError::Lost) => *self.lost.lock().unwrap() += 1,
                Err(_) => {}
            }
        }
        r
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

async fn committed(e: &Engine<Recording>, index: &str, id: &str) {
    e.write(index, vec![doc(id)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

#[tokio::test]
async fn a_head_that_does_not_advance_the_epoch_is_refused() {
    let s = Arc::new(Recording::new(TagStyle::ContentHash));
    let e = Engine::new(Arc::clone(&s), T, LaneId(1));
    committed(&e, "idx", "a").await;
    committed(&e, "idx", "b").await;
    let before = e.head_for_test().await;
    assert!(before.epoch.0 >= 2, "the lower-epoch case needs room");

    // `commit_head_for_test` has already advanced the epoch; these put it back to the one
    // it read, and below it.
    for (what, back) in [("equal", 1), ("lower", 2)] {
        let r = e
            .commit_head_for_test(|h| h.epoch = Epoch(h.epoch.0 - back))
            .await;
        assert!(
            matches!(r, Err(EngineError::Refused(_))),
            "the {what} epoch was not refused: {r:?}"
        );
        assert_eq!(
            e.head_for_test().await,
            before,
            "the {what} epoch changed HEAD"
        );
    }
}

#[tokio::test]
async fn no_two_heads_share_bytes_or_a_tag_under_any_tag_style() {
    // The answer to OQ-6, on the engine. On a content-derived style, distinct bytes are
    // distinct tags, so a stale tag can never match a HEAD that changed and changed back.
    for style in STYLES {
        let s = Arc::new(Recording::new(style));
        let a = Engine::new(Arc::clone(&s), T, LaneId(1));
        let b = Engine::new(Arc::clone(&s), T, LaneId(2));
        committed(&a, "idx", "a").await;
        committed(&b, "idx", "b").await;
        committed(&a, "idx", "c").await;
        assert!(
            a.compact("idx").await.unwrap().is_some(),
            "{style:?}: no compaction"
        );
        a.delete_index("idx").await.unwrap();
        // The same index again, with the same documents: the state a HEAD could most
        // plausibly return to.
        committed(&a, "idx", "a").await;
        committed(&b, "idx", "b").await;
        a.gc(0).await.unwrap();
        // A commit that changes nothing, from the lane that made the last one: if the epoch
        // did not advance, this would rewrite the HEAD it read byte for byte.
        let last = s.landed.lock().unwrap().len();
        a.commit_head_for_test(|_| {}).await.unwrap();
        assert_eq!(s.landed.lock().unwrap().len(), last + 1);

        let landed = s.landed.lock().unwrap().clone();
        assert!(landed.len() >= 9, "{style:?}: only {} HEADs", landed.len());
        let bodies: HashSet<&Bytes> = landed.iter().map(|(b, _)| b).collect();
        assert_eq!(
            bodies.len(),
            landed.len(),
            "{style:?}: a HEAD's bytes repeated"
        );
        let tags: HashSet<&str> = landed.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(tags.len(), landed.len(), "{style:?}: a HEAD's tag repeated");
    }
}

#[tokio::test]
async fn a_held_tag_is_refused_and_the_retry_keeps_the_other_write() {
    for style in STYLES {
        let s = Arc::new(Recording::new(style));
        let a = Engine::new(Arc::clone(&s), T, LaneId(1));
        let b = Engine::new(Arc::clone(&s), T, LaneId(2));
        committed(&a, "idx", "a").await;
        committed(&a, "idx", "b").await;

        // A compaction holds HEAD's tag while another engine commits a new id to the same
        // index. (An id already in an input would make a delete vector, and the compaction
        // would give up rather than retry.)
        let lost = s.lost();
        let merged = a
            .compact_with_interference_for_test("idx", committed(&b, "idx", "new"))
            .await
            .unwrap();
        assert!(merged.is_some(), "{style:?}: the compaction gave up");
        assert!(s.lost() > lost, "{style:?}: the held tag was not refused");
        let ids: HashSet<String> = a
            .scan("idx", None)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        for id in ["a", "b", "new"] {
            assert!(
                ids.contains(id),
                "{style:?}: {id} lost after the compaction: {ids:?}"
            );
        }

        // A drop holds HEAD's tag while another engine commits to another index.
        let lost = s.lost();
        a.delete_index_with_interference_for_test("idx", committed(&b, "other", "o"))
            .await
            .unwrap();
        assert!(
            s.lost() > lost,
            "{style:?}: the drop's held tag was not refused"
        );
        let head = a.head_for_test().await;
        assert!(
            !head.indexes.contains_key("idx"),
            "{style:?}: the drop did not land"
        );
        let other: Vec<String> = a
            .scan("other", None)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(other, ["o"], "{style:?}: the other index lost its write");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_hundred_writers_lose_nothing_under_any_tag_style() {
    for style in STYLES {
        let s = Arc::new(Recording::new(style));
        let mut tasks = Vec::new();
        for w in 0..100u64 {
            let s = Arc::clone(&s);
            tasks.push(tokio::spawn(async move {
                let e = Engine::new(s, T, LaneId(w));
                committed(&e, "idx", &format!("w{w}")).await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let e = Engine::new(Arc::clone(&s), T, LaneId(1000));
        let ids: HashSet<String> = e
            .scan("idx", None)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids.len(), 100, "{style:?}: acknowledged writes lost");
        let landed = s.landed.lock().unwrap();
        let bodies: HashSet<&Bytes> = landed.iter().map(|(b, _)| b).collect();
        assert_eq!(
            bodies.len(),
            landed.len(),
            "{style:?}: a HEAD's bytes repeated"
        );
    }
}
