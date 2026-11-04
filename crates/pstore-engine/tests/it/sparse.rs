//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M5a.2 — a sparse field survives the whole write path.
//!
//! ⚠️ **The failure this file exists to catch is silent.** Every stage between `write` and a
//! merged segment reconstructs a document from something narrower than the document: the
//! bundle carried one dense vector, a segment block carries no vector at all, and a
//! compaction merges whatever `scan` handed back. A field that any one of them drops comes
//! back as *absent*, which is indistinguishable from never having been written.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::{DEFAULT_FIELD, Document, Impact, VectorField, sparse};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const FIELD: &str = "body_sparse";

fn hybrid(i: usize) -> Document {
    let mut d = Document::new(format!("d{i:04}"), vec![i as f32, 1.0, 0.5]);
    d.vectors.insert(
        FIELD.into(),
        VectorField::Sparse(vec![
            ((i as u32 % 17).to_string(), Impact::new(0.75)),
            ((100 + (i as u32 % 3)).to_string(), Impact::new(0.25)),
        ]),
    );
    d
}

/// A document's pairs in key byte order, the order a scan answers them in (M177).
fn sparse_of(d: &Document) -> Vec<(String, f32)> {
    let mut pairs: Vec<(String, f32)> = match d.vectors.get(FIELD) {
        Some(VectorField::Sparse(p)) => p.iter().map(|(x, w)| (x.clone(), w.get())).collect(),
        _ => Vec::new(),
    };
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

#[tokio::test]
async fn a_sparse_field_survives_write_fold_and_compaction() {
    // ⚠️ Criterion 6. Three narrowings in a row, each of which loses the field silently:
    // the bundle (one dense vector until M5a.2), the segment block (no vectors at all), and
    // the compaction (merges what `scan` returned). Asserted after each, so a failure names
    // the stage rather than the outcome.
    let store = Accounted::new(MemoryStore::new());
    let t = TenantId(500);
    let e = Engine::new(Arc::new(store.as_tenant(t)), t, LaneId(1));

    let want: Vec<Document> = (0..40).map(hybrid).collect();
    for batch in want.chunks(20) {
        e.write("hybrid", batch.to_vec()).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }

    let folded = e.scan("hybrid", None).await.unwrap();
    assert_eq!(folded.len(), 40, "rows were lost before compaction");
    check(&folded, &want, "after the fold");

    e.compact("hybrid").await.unwrap();
    let merged = e.scan("hybrid", None).await.unwrap();
    assert_eq!(merged.len(), 40, "rows were lost in the merge");
    check(&merged, &want, "after the compaction");

    assert_eq!(store.count(t, OpClass::List), 0, "the sparse path listed");
}

fn check(got: &[Document], want: &[Document], when: &str) {
    for w in want {
        let g = got
            .iter()
            .find(|g| g.id == w.id)
            .unwrap_or_else(|| panic!("{when}: {} is missing entirely", w.id));
        let (gs, ws) = (sparse_of(g), sparse_of(w));
        assert!(
            !gs.is_empty(),
            "{when}: {} came back with NO sparse field -- which reads exactly like never \
             having written one",
            w.id
        );
        assert_eq!(
            gs.iter().map(|(d, _)| d).collect::<Vec<_>>(),
            ws.iter().map(|(d, _)| d).collect::<Vec<_>>(),
            "{when}: {}'s dimensions changed",
            w.id
        );
        for ((_, a), (_, b)) in gs.iter().zip(&ws) {
            assert!(
                (a - b).abs() <= b.abs() / 254.0 + 1e-6,
                "{when}: impact {a} vs {b}"
            );
        }
        assert_eq!(
            g.field(DEFAULT_FIELD),
            w.field(DEFAULT_FIELD),
            "{when}: {}'s dense field was disturbed by the sparse one",
            w.id
        );
    }
}

#[tokio::test]
async fn a_folded_segment_has_its_dictionary_beside_it() {
    // The sidecar is addressed by derivation, so a compaction can reach the dictionary of
    // every input it merges without a LIST. If the fold does not write it there, the merge
    // reads nothing and loses the field -- with the postings still in the segment, intact
    // and unreachable.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(501);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    e.write("hybrid", (0..8).map(hybrid).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let head = e.head_for_test().await;
    let refs = head.indexes.get("hybrid").expect("no segments");
    assert!(!refs.is_empty());
    for r in refs {
        let seg = Key::new(&r.key);
        let dict = sparse::dict_key(&seg);
        let raw = store
            .get(&dict)
            .await
            .unwrap_or_else(|_| panic!("no dictionary beside {}", seg.as_str()));
        assert!(
            sparse::Dictionary::decode(&raw).is_some(),
            "the sidecar is not a dictionary"
        );
    }
}

#[tokio::test]
async fn a_dense_only_index_writes_no_sidecar_and_costs_nothing_extra() {
    // ⚠️ The regression the sparse path could cause everywhere else. A dictionary written
    // for every segment would add a PUT per fold to indexes that have no sparse field at
    // all, and the write budget is 1 PUT per segment.
    let store = Accounted::new(MemoryStore::new());
    let t = TenantId(502);
    let e = Engine::new(Arc::new(store.as_tenant(t)), t, LaneId(1));
    let docs: Vec<Document> = (0..8)
        .map(|i| Document::new(format!("d{i}"), vec![i as f32, 1.0]))
        .collect();
    e.write("dense", docs).await.unwrap();
    e.flush().await.unwrap();
    let before = store.count(t, OpClass::Write);
    e.fold().await.unwrap();
    let spent = store.count(t, OpClass::Write) - before;
    assert!(
        spent <= 2,
        "a dense fold spent {spent} writes: one segment and one HEAD is the budget, and a \
         sidecar was written for an index with no sparse field"
    );
}

#[tokio::test]
async fn a_reaped_sparse_segment_takes_its_dictionary_with_it() {
    // ⚠️ A sidecar is reachable ONLY by derivation from its segment. Reap the segment and
    // leave the dictionary, and nothing in the system can ever name that object again — it
    // is not in the graveyard, not in HEAD, and not findable without the LIST this design
    // does not have.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(503);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    for batch in 0..2 {
        e.write("hybrid", (batch * 8..batch * 8 + 8).map(hybrid).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let before: Vec<Key> = e
        .head_for_test()
        .await
        .indexes
        .get("hybrid")
        .unwrap()
        .iter()
        .map(|r| Key::new(&r.key))
        .collect();
    assert_eq!(before.len(), 2, "the fixture needs two segments to merge");

    e.compact("hybrid").await.unwrap();
    // Enough epochs to pass the retention window, then reap.
    for _ in 0..3 {
        e.write("other", vec![Document::new("x", vec![1.0])])
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.gc(1).await.unwrap();

    for seg in &before {
        assert!(
            store.get(seg).await.is_err(),
            "the merged-away segment {} survived gc",
            seg.as_str()
        );
        assert!(
            store.get(&sparse::dict_key(seg)).await.is_err(),
            "the dictionary of {} outlived it, and nothing can name it now",
            seg.as_str()
        );
    }
}

// ---------------------------------------------------------------------------
// M5c.1 — full text through the same path. The failures are the same shape as
// sparse's, which is the point: one codec, one sidecar rule, one fold.
// ---------------------------------------------------------------------------

use pstore_format::{Value, text};

fn texted(i: usize) -> Document {
    let mut d = hybrid(i);
    d.attrs.insert(
        text::DEFAULT_TEXT_FIELD.to_owned(),
        Value::Str(format!("document {i} about quarterly revenue and the fox")),
    );
    d
}

#[tokio::test]
async fn a_text_field_survives_a_compaction() {
    // ⚠️ Postings cannot be inverted back into text, so a merge that had only the postings
    // would rewrite every document as a bag of words. The string lives in the attributes and
    // the fold RE-ANALYZES it -- which means the test worth writing is that the postings of
    // the merged segment are the postings of the merged text, not that the text survived.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(510);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    let want: Vec<Document> = (0..24).map(texted).collect();
    for batch in want.chunks(12) {
        e.write("txt", batch.to_vec()).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.compact("txt").await.unwrap();

    let merged = e.scan("txt", None).await.unwrap();
    assert_eq!(merged.len(), 24);
    for w in &want {
        let g = merged.iter().find(|g| g.id == w.id).expect("row lost");
        assert_eq!(
            g.attrs.get(text::DEFAULT_TEXT_FIELD),
            w.attrs.get(text::DEFAULT_TEXT_FIELD),
            "{}'s text did not survive the merge",
            w.id
        );
    }

    // The merged segment's own postings, re-analyzed rather than copied.
    let head = e.head_for_test().await;
    let refs = head.indexes.get("txt").unwrap();
    assert_eq!(refs.len(), 1, "the fixture did not merge to one segment");
    let seg = Key::new(&refs[0].key);
    let raw = store
        .get(&text::dict_key(&seg))
        .await
        .expect("no term dictionary beside the merged segment");
    let dict = text::TermDict::decode(&raw).expect("the sidecar is not a term dictionary");
    let e_rev = dict.lookup("revenue").expect("\"revenue\" is not indexed");
    assert_eq!(
        e_rev.df, 24,
        "every document says \"revenue\"; df says {}",
        e_rev.df
    );
    assert!(dict.lookup("aardvark").is_none());
}

#[tokio::test]
async fn a_reaped_text_segment_takes_its_dictionary_with_it() {
    // The same rule as the sparse sidecar, and the same reason: a `.tdict` is reachable only
    // by derivation from its segment, so one left behind can never be named again.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(511);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    for batch in 0..2 {
        e.write("txt", (batch * 6..batch * 6 + 6).map(texted).collect())
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let before: Vec<Key> = e
        .head_for_test()
        .await
        .indexes
        .get("txt")
        .unwrap()
        .iter()
        .map(|r| Key::new(&r.key))
        .collect();
    e.compact("txt").await.unwrap();
    for _ in 0..3 {
        e.write("other", vec![Document::new("x", vec![1.0])])
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e.gc(1).await.unwrap();
    for seg in &before {
        assert!(
            store.get(&text::dict_key(seg)).await.is_err(),
            "the term dictionary of {} outlived it",
            seg.as_str()
        );
    }
}

#[tokio::test]
async fn a_text_index_and_a_sparse_index_share_a_segment_without_sharing_a_term_space() {
    // ⚠️ OQ-127's answer, exercised. `texted` documents carry BOTH a sparse field and a text
    // field; the two must land in different sections with different sidecars, or a numeric
    // dimension and a string term add into one list and every score is the sum of two
    // unrelated signals.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(512);
    let e = Engine::new(Arc::clone(&store), t, LaneId(1));
    e.write("both", (0..10).map(texted).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    let head = e.head_for_test().await;
    let seg = Key::new(&head.indexes.get("both").unwrap()[0].key);
    assert!(store.get(&sparse::dict_key(&seg)).await.is_ok());
    assert!(store.get(&text::dict_key(&seg)).await.is_ok());
    assert_ne!(sparse::dict_key(&seg), text::dict_key(&seg));

    let out = e.scan("both", None).await.unwrap();
    assert_eq!(out.len(), 10);
    assert!(matches!(
        out[4].vectors.get(FIELD),
        Some(VectorField::Sparse(_))
    ));
    assert!(out[4].attrs.contains_key(text::DEFAULT_TEXT_FIELD));
}

#[tokio::test]
async fn a_sparse_leg_reaches_unfolded_rows() {
    // ⚠️ Every other test here folds first. The fresh segment is built by `fresh_target`, which
    // stores the dictionary sidecar only when it is non-empty -- and M8g's sweep found that
    // filter's `!` deletable: the dictionary is then never stored, and a sparse leg over
    // unfolded rows fails, which no test asked.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(502), LaneId(1));
    let docs: Vec<Document> = (0..6).map(hybrid).collect();
    e.write("idx", docs.clone()).await.unwrap();

    let leg = vec![pstore_query::Prefetch::Sparse {
        field: FIELD.to_owned(),
        query: sparse_of(&docs[3]),
        limit: 5,
    }];
    let a = e
        .query("idx", &leg, pstore_query::Fusion::default(), 5)
        .await
        .expect("a sparse leg over unfolded rows must run");
    assert!(
        a.hits
            .iter()
            .any(|h| h.segment == a.unfolded_at && a.unfolded[h.row].id == docs[3].id),
        "the unfolded row the sparse query names was not returned"
    );
}

/// M177: keys a mapping to numbers would merge, truncate or misorder: spellings of one number,
/// both normal forms of one letter, long keys differing in their last byte, keys sharing their
/// first 8 bytes, and one of the longest a key may be.
fn adversarial_keys() -> Vec<String> {
    let long = "k".repeat(199);
    vec![
        "1".to_owned(),
        "01".to_owned(),
        "1.0".to_owned(),
        "\u{e9}".to_owned(),
        "e\u{301}".to_owned(),
        format!("{long}a"),
        format!("{long}b"),
        "prefix00-x".to_owned(),
        "prefix00-y".to_owned(),
        "z".repeat(pstore_format::SPARSE_KEY_MAX),
    ]
}

/// Row `i` names two adjacent keys, in an order that depends on `flip`.
fn keyed(i: usize, flip: bool) -> Document {
    let keys = adversarial_keys();
    let (a, b) = (&keys[i % keys.len()], &keys[(i + 1) % keys.len()]);
    // One weight, so U8's per-key scale holds it exactly and `check`'s bound is the error's.
    let mut pairs = vec![(a.clone(), Impact::new(0.5)), (b.clone(), Impact::new(0.5))];
    if flip {
        pairs.reverse();
    }
    let mut d = Document::new(format!("r{i:03}"), vec![1.0, 0.0, 0.0]);
    d.vectors.insert(FIELD.into(), VectorField::Sparse(pairs));
    d
}

/// The ids a sparse query for `key` answers, from `e`.
async fn by_key(e: &Engine<MemoryStore>, index: &str, key: &str) -> Vec<String> {
    let leg = vec![pstore_query::Prefetch::Sparse {
        field: FIELD.to_owned(),
        query: vec![(key.to_owned(), 1.0)],
        limit: 1_000,
    }];
    let a = e
        .query(index, &leg, pstore_query::Fusion::default(), 1_000)
        .await
        .unwrap();
    let mut ids: Vec<String> = e.resolve_rows(&a).into_iter().map(|r| r.0).collect();
    ids.sort();
    ids
}

/// The ids of `docs` naming `key`.
fn naming(docs: &[Document], key: &str) -> Vec<String> {
    let mut ids: Vec<String> = docs
        .iter()
        .filter(|d| sparse_of(d).iter().any(|(k, _)| k == key))
        .map(|d| d.id.clone())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn string_keys_survive_the_bundle_the_fold_and_the_compaction_each_their_own() {
    // M177 criterion 1, through the engine: each stage keeps every key whole and apart.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(503), LaneId(1));
    let want: Vec<Document> = (0..30).map(|i| keyed(i, i % 2 == 0)).collect();
    for batch in want.chunks(10) {
        e.write("keys", batch.to_vec()).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    check(
        &e.scan("keys", None).await.unwrap(),
        &want,
        "after the folds",
    );
    e.compact("keys").await.unwrap();
    check(
        &e.scan("keys", None).await.unwrap(),
        &want,
        "after the compaction",
    );
    for key in adversarial_keys() {
        assert_eq!(
            by_key(&e, "keys", &key).await,
            naming(&want, &key),
            "{:?}",
            key.get(..12)
        );
    }
}

#[tokio::test]
async fn two_writers_naming_one_key_write_one_dimension() {
    // M177 criterion 3: no mapping to agree on. Two processes write rows naming the same keys
    // in opposite orders and batches; after the folds and a restart one query finds both's.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(504);
    let a = Engine::new(Arc::clone(&store), t, LaneId(1));
    let b = Engine::new(Arc::clone(&store), t, LaneId(2));
    let mine: Vec<Document> = (0..20).map(|i| keyed(i, false)).collect();
    let theirs: Vec<Document> = (100..120).map(|i| keyed(i, true)).collect();
    a.write("two", mine[..7].to_vec()).await.unwrap();
    b.write("two", theirs.clone()).await.unwrap();
    a.write("two", mine[7..].to_vec()).await.unwrap();
    for e in [&a, &b] {
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let all: Vec<Document> = mine.iter().chain(&theirs).cloned().collect();
    let restarted = Engine::new(Arc::clone(&store), t, LaneId(3));
    for key in adversarial_keys() {
        let got = by_key(&restarted, "two", &key).await;
        assert_eq!(got, naming(&all, &key), "{:?}", key.get(..12));
        assert!(got.iter().any(|id| id.starts_with("r1")));
    }
}
