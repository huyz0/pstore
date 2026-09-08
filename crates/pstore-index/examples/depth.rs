#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

//! The round-trip and byte gates, **at gate scale** (M5a c9/c11, M5c c6/c7).
//!
//! ⚠️ Runs **outside `cargo test`**, and the reason is arithmetic. `cargo-mutants` rebuilds
//! and reruns the suite once per mutant — 462 times in M5 — so a fixture that costs 36
//! seconds inside the suite costs **four and a half hours** across a sweep. `scripts/recall.sh`
//! says the same thing in its own header, and this file exists because M5c ignored it: the
//! 20,000-document text corpus was the slowest binary in the workspace the day it landed.
//!
//! ⚠️ **Scale is the whole point.** M1's depth invariant held at 500 rows and broke at 40,000,
//! and the sidecar deviation C-10 argues for only exists because a real vocabulary does not
//! fit the index section. The suite keeps small-scale versions that pin the *shape*; what
//! cannot be shrunk without measuring the wrong thing lives here.
//!
//!   scripts/depth.sh

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_format::sparse::{self, ImpactEncoding};
use pstore_format::{Document, Impact, Section, SegmentWriter, Value, VectorField, text};
use pstore_index::sparse::SparseIndex;
use pstore_index::text::TextIndex;
use pstore_testkit::depth::DepthCounting;
use pstore_types::TenantId;

/// ⚠️ Pinned by the specs, and it decides the result: at the 64 KiB default the coalescer
/// merges most of a multi-megabyte section, and the whole-section read the byte bound exists
/// to refuse becomes the measured behaviour.
const GAP: u64 = 256;
const SPARSE_FIELD: &str = "body_sparse";

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        sparse_gate().await;
        text_gate().await;
    });
    println!("ok: depth and byte gates hold at gate scale");
}

// ---------------------------------------------------------------------------
// Sparse: 20,000 rows x 32 non-zero dimensions over a 30,000-dimension vocabulary.
// ---------------------------------------------------------------------------

fn sparse_corpus(rows: usize, vocab: u32, nnz: usize) -> Vec<Document> {
    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    (0..rows)
        .map(|i| {
            let mut pairs: Vec<(u32, Impact)> = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            while seen.len() < nnz {
                // Zipf-ish: squaring the uniform draw makes low dimensions dominate, which
                // is what a real vocabulary does and what a uniform draw hides.
                let u = (next() % 1_000_000) as f64 / 1_000_000.0;
                let d = ((u * u) * f64::from(vocab)) as u32;
                if seen.insert(d) {
                    pairs.push((d, Impact::new((next() % 900) as f32 / 1000.0 + 0.1)));
                }
            }
            pairs.sort_by_key(|(d, _)| *d);
            Document {
                id: format!("d{i:05}"),
                vectors: std::collections::BTreeMap::from([(
                    SPARSE_FIELD.to_owned(),
                    VectorField::Sparse(pairs),
                )]),
                attrs: std::collections::BTreeMap::new(),
            }
        })
        .collect()
}

async fn sparse_gate() {
    let docs = sparse_corpus(20_000, 30_000, 32);
    let built = sparse::build(&docs, SPARSE_FIELD, ImpactEncoding::U8);
    let acct = Accounted::new(MemoryStore::with_coalesce_gap(GAP));
    let tenant = TenantId(1);
    let view = acct.as_tenant(tenant);
    let key = Key::new("gate/sparse.seg");

    let mut w = SegmentWriter::new(64);
    for d in &docs {
        w.push(d.clone());
    }
    view.put(
        &key,
        w.with_section(Section::SparsePostings, built.section)
            .try_finish()
            .expect("segment"),
    )
    .await
    .expect("put");
    view.put(
        &sparse::dict_key(&key),
        bytes::Bytes::from(built.dictionary),
    )
    .await
    .expect("put dict");

    // The query: every dimension one document uses.
    let VectorField::Sparse(pairs) = &docs[7].vectors[SPARSE_FIELD] else {
        panic!("fixture is not sparse")
    };
    let query: Vec<(u32, f32)> = pairs.iter().map(|(d, w)| (*d, w.get())).collect();

    // Depth, on the same corpus, through a counting store.
    let counting = DepthCounting::new(MemoryStore::with_coalesce_gap(GAP));
    for k in [key.clone(), sparse::dict_key(&key)] {
        let body = view.get(&k).await.expect("read back");
        counting.put(&k, body).await.expect("copy");
    }
    counting.reset();
    let idx = SparseIndex::open(&counting, &key, SPARSE_FIELD)
        .await
        .expect("open");
    assert_eq!(
        counting.depth(),
        1,
        "opening cost {} rounds",
        counting.depth()
    );
    counting.reset();
    let hits = idx
        .search(&counting, &key, &query, 10)
        .await
        .expect("search");
    assert!(!hits.is_empty(), "the gate query matched nothing");
    assert_eq!(
        counting.depth(),
        1,
        "the postings fetch cost {} rounds, so the lists went out in a loop",
        counting.depth()
    );

    // Bytes, scoped to the postings span.
    let idx = SparseIndex::open(&view, &key, SPARSE_FIELD)
        .await
        .expect("open");
    let want = idx.list_bytes(&query);
    assert!(
        want > 0,
        "the query matched no list, so the bound is vacuous"
    );
    acct.record_ranges();
    idx.search(&view, &key, &query, 10).await.expect("search");
    let moved = acct.bytes_in(&key, idx.span());
    assert!(
        moved as f64 <= want as f64 * 1.2,
        "sparse: a {}-term query moved {moved} bytes of postings against {want} in its own \
         lists",
        query.len()
    );
    assert_eq!(
        acct.count(tenant, OpClass::List),
        0,
        "a sparse query listed"
    );
    println!("sparse   20,000 rows / 30,000 dims: depth 1 + 1, {moved} bytes against {want}");
}

// ---------------------------------------------------------------------------
// Text: 20,000 documents of ~120 terms over a 30,000-term vocabulary.
// ---------------------------------------------------------------------------

fn text_corpus(rows: usize, vocab: usize, len: usize) -> Vec<Document> {
    let mut rng: u64 = 0xbeef_dead_c0de_1234;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    (0..rows)
        .map(|i| {
            let body: Vec<String> = (0..len)
                .map(|_| {
                    let u = (next() % 1_000_000) as f64 / 1_000_000.0;
                    format!("t{}", ((u * u) * vocab as f64) as usize)
                })
                .collect();
            let mut d = Document::new(format!("d{i:05}"), vec![1.0, 0.0]);
            d.attrs.insert(
                text::DEFAULT_TEXT_FIELD.to_owned(),
                Value::Str(body.join(" ")),
            );
            d
        })
        .collect()
}

async fn text_gate() {
    let docs = text_corpus(20_000, 30_000, 120);
    let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
    let acct = Accounted::new(MemoryStore::with_coalesce_gap(GAP));
    let tenant = TenantId(2);
    let view = acct.as_tenant(tenant);
    let key = Key::new("gate/text.seg");

    let mut w = SegmentWriter::new(64);
    for d in &docs {
        w.push(d.clone());
    }
    view.put(
        &key,
        w.with_section(Section::TextPostings, built.postings)
            .with_section(Section::Fieldnorms, text::encode_norms(&built.fieldnorms))
            .try_finish()
            .expect("segment"),
    )
    .await
    .expect("put");
    view.put(&text::dict_key(&key), bytes::Bytes::from(built.dictionary))
        .await
        .expect("put dict");

    let Some(Value::Str(body)) = docs[11].attrs.get(text::DEFAULT_TEXT_FIELD) else {
        panic!("fixture has no text")
    };
    let mut query = text::analyze(body);
    query.sort();
    query.dedup();
    query.truncate(3);

    let counting = DepthCounting::new(MemoryStore::with_coalesce_gap(GAP));
    for k in [key.clone(), text::dict_key(&key)] {
        let bytes = view.get(&k).await.expect("read back");
        counting.put(&k, bytes).await.expect("copy");
    }
    counting.reset();
    let idx = TextIndex::open(&counting, &key).await.expect("open");
    assert_eq!(
        counting.depth(),
        1,
        "opening cost {} rounds",
        counting.depth()
    );
    let stats = idx.summary();
    counting.reset();
    let hits = idx
        .search(&counting, &key, &query, &stats, 10)
        .await
        .expect("search");
    assert!(!hits.is_empty(), "the gate query matched nothing");
    assert_eq!(
        counting.depth(),
        1,
        "the postings and fieldnorms cost {} rounds, so one waited on the other",
        counting.depth()
    );

    let idx = TextIndex::open(&view, &key).await.expect("open");
    let want = idx.list_bytes(&query);
    assert!(
        want > 0,
        "the query matched no list, so the bound is vacuous"
    );
    acct.record_ranges();
    idx.search(&view, &key, &query, &idx.summary(), 10)
        .await
        .expect("search");
    let moved = acct.bytes_in(&key, idx.span());
    assert!(
        moved as f64 <= want as f64 * 1.2,
        "text: a {}-term query moved {moved} bytes of postings against {want} in its own lists",
        query.len()
    );
    assert_eq!(acct.count(tenant, OpClass::List), 0, "a text query listed");
    println!("text     20,000 docs / 30,000 terms: depth 1 + 1, {moved} bytes against {want}");
}
