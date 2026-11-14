//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M231: an index's storage settings. The cluster's default is the engine's; an index's
//! override in HEAD wins over it for every fold and compaction from then on, whichever server
//! seals; segments already written stay as they are, and every combination reads the same
//! rows.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_engine::{Engine, Storage};
use pstore_format::{BlockOptions, Document, Segment, Value};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(231);

type E = Engine<MemoryStore>;

fn doc(i: usize) -> Document {
    let mut d = Document::new(format!("d{i:04}"), vec![1.0, i as f32 / 100.0]);
    d.attrs.insert("n".to_owned(), Value::Int(i as i64));
    d.attrs
        .insert("tag".to_owned(), Value::Str(format!("t{}", i % 3)));
    d.attrs
        .insert("on".to_owned(), Value::Bool(i.is_multiple_of(2)));
    d
}

const ROWS: BlockOptions = BlockOptions {
    columnar: false,
    equality: false,
    aggregates: false,
};

/// The footer's version, `MAGIC(8) VERSION(2) ...` a fixed distance from the end.
fn version(seg: &[u8]) -> u16 {
    let at = seg.len() - (8 + 2 + 8 + 4 + 4 + 8 + 8) + 8;
    u16::from_le_bytes([seg[at], seg[at + 1]])
}

/// What each of `index`'s live segments was written as, oldest first: (columnar, equality
/// section, aggregate section).
async fn written(store: &MemoryStore, e: &E, index: &str) -> Vec<(bool, bool, bool)> {
    let head = e.head_for_test().await;
    let mut out = Vec::new();
    for r in head.indexes.get(index).cloned().unwrap_or_default() {
        let key = Key::new(r.key.clone());
        let bytes = store.get(&key).await.unwrap();
        let seg = Segment::open(store, &key).await.unwrap();
        let mut zones = seg.block_zones();
        let z = zones.next().unwrap();
        out.push((
            version(&bytes) == 3,
            z.equality.is_some(),
            z.aggregates.is_some(),
        ));
    }
    out
}

async fn put(e: &E, index: &str, from: usize, to: usize) {
    e.write(index, (from..to).map(doc).collect()).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

async fn ids(e: &E, index: &str) -> Vec<String> {
    let mut out: Vec<_> = e
        .scan(index, None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn an_override_wins_over_the_default_for_every_later_seal() {
    let store = Arc::new(MemoryStore::new());
    // Two servers, configured alike: the cluster default writes every section.
    let a = Engine::new(Arc::clone(&store), T, LaneId(1));
    let b = Engine::new(Arc::clone(&store), T, LaneId(2));
    assert_eq!(a.block_options(), BlockOptions::default());
    put(&a, "idx", 0, 40).await;
    assert_eq!(written(&store, &a, "idx").await, [(true, true, true)]);

    // An absent index has nothing to override.
    assert!(
        a.set_storage("nope", |_| ROWS_OVERRIDE)
            .await
            .unwrap()
            .is_none()
    );

    // The owner turns every setting off for this index: one commit, then the next fold --
    // by the other server -- writes row blocks with neither section.
    let before = a.head_for_test().await.epoch;
    let (epoch, now) = a
        .set_storage("idx", |_| ROWS_OVERRIDE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(now, ROWS_OVERRIDE);
    assert_eq!(epoch.0, before.0 + 1);
    // The same value again commits nothing.
    let (again, _) = a.set_storage("idx", |s| s).await.unwrap().unwrap();
    assert_eq!(again, epoch);
    put(&b, "idx", 40, 80).await;
    assert_eq!(
        written(&store, &b, "idx").await,
        [(true, true, true), (false, false, false)]
    );
    assert_eq!(ids(&b, "idx").await.len(), 80);

    // A compaction merges both under the override as it stands.
    b.compact("idx").await.unwrap();
    assert_eq!(written(&store, &b, "idx").await, [(false, false, false)]);
    assert_eq!(ids(&a, "idx").await.len(), 80);

    // Changing one setting keeps the others: columnar back on, sections still off.
    let (_, now) = a
        .set_storage("idx", |s| Storage {
            columnar: Some(true),
            ..s
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        now,
        Storage {
            columnar: Some(true),
            ..ROWS_OVERRIDE
        }
    );
    put(&a, "idx", 80, 90).await;
    assert_eq!(
        written(&store, &a, "idx").await.last(),
        Some(&(true, false, false))
    );

    // Cleared, the index takes the default again, and HEAD forgets it.
    a.set_storage("idx", |_| Storage::default()).await.unwrap();
    assert!(a.head_for_test().await.storage.is_empty());
    put(&b, "idx", 90, 100).await;
    assert_eq!(
        written(&store, &b, "idx").await.last(),
        Some(&(true, true, true))
    );
    assert_eq!(
        a.index_stats("idx").await.unwrap().unwrap().storage,
        Storage::default()
    );
    assert_eq!(ids(&a, "idx").await.len(), 100);
}

const ROWS_OVERRIDE: Storage = Storage {
    columnar: Some(false),
    equality: Some(false),
    aggregates: Some(false),
};

#[tokio::test]
async fn a_cluster_default_of_rows_is_overridden_per_index() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), T, LaneId(1)).with_block_options(ROWS);
    put(&e, "plain", 0, 30).await;
    put(&e, "col", 0, 30).await;
    assert_eq!(written(&store, &e, "plain").await, [(false, false, false)]);
    let on = Storage {
        columnar: Some(true),
        equality: Some(true),
        aggregates: None,
    };
    e.set_storage("col", |_| on).await.unwrap().unwrap();
    assert_eq!(e.index_stats("col").await.unwrap().unwrap().storage, on);
    put(&e, "col", 30, 60).await;
    put(&e, "plain", 30, 60).await;
    // The unset setting takes this server's default; the other index is untouched.
    assert_eq!(
        written(&store, &e, "col").await,
        [(false, false, false), (true, true, false)]
    );
    assert_eq!(
        written(&store, &e, "plain").await,
        [(false, false, false), (false, false, false)]
    );
    assert_eq!(ids(&e, "col").await, ids(&e, "plain").await);
    // The override survives a restart: a fresh engine reads it from HEAD.
    let fresh = Engine::new(Arc::clone(&store), T, LaneId(3)).with_block_options(ROWS);
    put(&fresh, "col", 60, 70).await;
    assert_eq!(
        written(&store, &fresh, "col").await.last(),
        Some(&(true, true, false))
    );
}
