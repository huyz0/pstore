//! **M5's exit criterion**: BM25 on MS MARCO passage ranking, which the roadmap recorded as
//! `NOT-RUN` — *"no network and no dataset here"*.
//!
//! ⚠️ **Both halves of that were false when re-tested.** A range request to the official
//! corpus returns `206`, and the archive is 1.06 GB against 680 GB free. The entry was written
//! when it was true and never re-checked.
//!
//! ## Why this and not `ndcg.sh`
//!
//! `ndcg.sh` reports NDCG@10 = 1.0000 and M5c's ledger says why that is not a quality claim:
//! relevance is *planted*, so any IDF-weighted scorer finds it. What that gate shows is that
//! its corpus **discriminates**. This one measures whether the BM25 here ranks real text, and
//! it comes with a number nobody here chose: **BM25 scores ≈0.18 MRR@10** on this task.
//!
//! ## ⚠️ Sharded, because this machine has died of memory twice
//!
//! `AGENTS.md` records the build ceilings existing because "a mutation sweep on top of that
//! killed the WSL2 VM twice". 8.8M passages through one `text::build` is tens of gigabytes.
//! Each shard is built, sealed into a store, and its construction memory dropped before the
//! next — so peak is one shard's postings, not the corpus's. That also makes this an
//! evaluation of the **multi-segment path**, with statistics merged across shards, which is
//! what a deployment actually runs.
//!
//! ⚠️ **A hard RSS guard aborts rather than swapping.** Reporting a number after taking the
//! machine down is not a trade this project makes.
//!
//!   scripts/msmarco.sh
#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::{Document, Section, SegmentWriter, Value, text};
use pstore_query::{Fusion, Prefetch, Target};
use std::collections::HashMap;
use std::io::BufRead;

/// Passages per shard. ⚠️ The memory knob: bigger shards mean fewer, wider fan-outs per query
/// and more peak memory while one is being built.
const SHARD: usize = 400_000;
/// Abort above this. Well under the machine's free memory, because the point is not to find
/// the ceiling.
const RSS_LIMIT_GB: f64 = 8.0;

fn rss_gb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<f64>()
                .ok()
        })
        .map_or(0.0, |kb| kb / 1024.0 / 1024.0)
}

fn guard(peak: &mut f64, where_: &str) {
    let now = rss_gb();
    *peak = peak.max(now);
    assert!(
        now < RSS_LIMIT_GB,
        "resident memory {now:.1} GB exceeded the {RSS_LIMIT_GB:.0} GB guard at {where_} — \
         aborting rather than taking the machine down; lower SHARD"
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let dir = std::env::var("PSTORE_MSMARCO_DIR")
        .unwrap_or_else(|_| format!("{}/.cache/pstore-msmarco", std::env::var("HOME").unwrap()));
    let limit: usize = std::env::var("PSTORE_MSMARCO_QUERIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);

    let mut peak = 0.0f64;
    let store = MemoryStore::new();
    // Segment row -> passage id, per shard. The segment stores the passage text because
    // `text::build` reads it from the document, and the row order is the push order.
    let mut ids: Vec<Vec<u32>> = Vec::new();
    let mut targets: Vec<Target> = Vec::new();

    let f = std::fs::File::open(format!("{dir}/collection.tsv")).expect("collection.tsv");
    let mut lines = std::io::BufReader::with_capacity(1 << 20, f).lines();
    let mut passages = 0usize;
    loop {
        let mut docs: Vec<Document> = Vec::with_capacity(SHARD);
        let mut shard_ids: Vec<u32> = Vec::with_capacity(SHARD);
        for _ in 0..SHARD {
            let Some(Ok(line)) = lines.next() else { break };
            let Some((id, body)) = line.split_once('\t') else {
                continue;
            };
            let Ok(pid) = id.parse::<u32>() else { continue };
            let mut d = Document::new(String::new(), Vec::new());
            d.attrs.insert(
                text::DEFAULT_TEXT_FIELD.to_owned(),
                Value::Str(body.to_owned()),
            );
            shard_ids.push(pid);
            docs.push(d);
        }
        if docs.is_empty() {
            break;
        }
        passages += docs.len();
        guard(&mut peak, "after reading a shard");

        let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
        let mut w = SegmentWriter::new(512);
        for d in docs {
            w.push(d);
        }
        let key = Key::new(format!("ms/{}.seg", targets.len()));
        store
            .put(
                &text::dict_key(&key),
                bytes::Bytes::from(built.dictionary.clone()),
            )
            .await
            .unwrap();
        let seg = w
            .with_section(Section::TextPostings, built.postings)
            .with_section(Section::Fieldnorms, text::encode_norms(&built.fieldnorms))
            .with_text_fields(&[text::DEFAULT_TEXT_FIELD.to_owned()])
            .try_finish()
            .expect("segment");
        store.put(&key, seg).await.unwrap();
        targets.push(Target {
            centroids: Key::new(format!("{}.cen", key.as_str())),
            segment: key,
        });
        ids.push(shard_ids);
        guard(&mut peak, "after sealing a shard");
        eprintln!(
            "  shard {:>2}: {passages} passages, rss {:.1} GB",
            targets.len() - 1,
            rss_gb()
        );
    }

    // Queries and judgments.
    let queries: Vec<(u32, String)> =
        std::fs::read_to_string(format!("{dir}/queries.dev.small.tsv"))
            .expect("queries")
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter_map(|(q, t)| Some((q.parse().ok()?, t.to_owned())))
            .take(limit)
            .collect();
    let mut rel: HashMap<u32, Vec<u32>> = HashMap::new();
    for l in std::fs::read_to_string(format!("{dir}/qrels.dev.small.tsv"))
        .expect("qrels")
        .lines()
    {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() == 4
            && f[3] != "0"
            && let (Ok(q), Ok(d)) = (f[0].parse::<u32>(), f[2].parse::<u32>())
        {
            rel.entry(q).or_default().push(d);
        }
    }

    println!("# MS MARCO passage ranking, dev-small. ⚠️ provisional: WSL2, one run.");
    println!(
        "# {passages} passages in {} shards of {SHARD}, {} queries, peak rss {peak:.1} GB",
        targets.len(),
        queries.len()
    );

    // ⚠️ **Two arms, and the second is M5c's defect made visible on real text.** M5c measured
    // that global IDF changes the top-1 on a planted corpus; this is the first place the size
    // of that effect can be seen. `merged` is the shipped path — one census of statistics
    // across every shard. `per_shard` is what a naive implementation does: query each shard
    // with only its own statistics and sort the results together, which compares scores
    // computed against different corpora.
    let per_shard = std::env::var("PSTORE_MSMARCO_PER_SHARD").is_ok();
    let mut rr = 0.0f64;
    let mut ndcg = 0.0f64;
    let mut judged = 0usize;
    for (qid, qtext) in &queries {
        let Some(want) = rel.get(qid) else { continue };
        judged += 1;
        let leg = |limit: usize| {
            vec![Prefetch::Text {
                field: text::DEFAULT_TEXT_FIELD.to_owned(),
                query: qtext.clone(),
                limit,
            }]
        };
        let hits = if per_shard {
            // ⚠️ **Raw BM25, not the fused score.** The first version of this arm called
            // `pstore_query::query` per shard and sorted the results together — but
            // `Fusion::default()` is RRF, so every shard's rank-1 hit comes back at exactly
            // `1/(k+1)` and the merge is a tie lottery. It measured 0.0038, and that number is
            // a fact about comparing independent fusions, **not** about statistics. Isolating
            // the statistics means comparing the scores the statistics actually enter.
            let mut all: Vec<(usize, usize, f32)> = Vec::new();
            for (i, t) in targets.iter().enumerate() {
                let idx = pstore_index::text::TextIndex::open(&store, &t.segment)
                    .await
                    .expect("open");
                let stats = idx.summary();
                let terms = text::analyze(qtext);
                for (row, score) in idx
                    .search(&store, &t.segment, &terms, &stats, 10)
                    .await
                    .expect("search")
                {
                    all.push((i, row, score));
                }
            }
            all.sort_by(|a, b| b.2.total_cmp(&a.2));
            all.truncate(10);
            all.into_iter()
                .map(|(segment, row, score)| pstore_query::Hit {
                    segment,
                    row,
                    score,
                })
                .collect()
        } else {
            pstore_query::query(&store, &targets, &leg(10), Fusion::default(), 10)
                .await
                .expect("query")
        };
        for (rank, h) in hits.iter().enumerate() {
            let pid = ids[h.segment][h.row];
            // ⚠️ `break` on the FIRST relevant hit: MRR is the reciprocal rank of the first,
            // not a sum over all of them.
            if want.contains(&pid) {
                rr += 1.0 / (rank + 1) as f64;
                ndcg += 1.0 / ((rank + 2) as f64).log2();
                break;
            }
        }
        if judged.is_multiple_of(500) {
            eprintln!("  {judged} queries, mrr so far {:.4}", rr / judged as f64);
        }
    }

    println!();
    println!(
        "statistics     : {}",
        if per_shard {
            "PER SHARD — each shard scored against only its own corpus"
        } else {
            "merged across every shard (the shipped path)"
        }
    );
    println!("judged queries : {judged}");
    println!("MRR@10         : {:.4}", rr / judged as f64);
    println!("NDCG@10        : {:.4}", ndcg / judged as f64);
    println!();
    println!("reference      : BM25 ≈ 0.18 MRR@10 on this task");
}
