//! Does block-max pruning save bytes **in this system's shape**? — the measurement
//! [M5d](../../../docs/milestones/M5d/SPEC.md) says M5e cannot be specified without.
//!
//! ⚠️ **This writes no format and prunes nothing.** It answers M5d's questions 2 and 3 with a
//! number, against the same corpus `scripts/depth.sh` uses — 20,000 documents of ~120 terms
//! over a 30,000-term vocabulary, Zipf-ish — rather than a fixture arranged to make pruning
//! fire, which M5d names in advance as the failure to avoid.
//!
//! ## The condition being measured, in full
//!
//! `TextIndex::search` is **disjunctive**: it sums each term's contribution over the union of
//! rows. So a block's upper bound bounds *one term's addend*, never a document's score, and
//! the sound skip condition for a block β of term t is
//!
//! ```text
//! upper_t(β) + Σ_{i≠t} U_i^max  <  θ
//! ```
//!
//! Everything in it must be known **before** the one postings fetch, because deciding after
//! the fetch saves no bytes. That fixes what each part can be:
//!
//! - `upper_t(β)` from the block's `(max_tf, min_fieldnorm)`, `lower_t(β)` from its
//!   `(min_tf, max_fieldnorm)`. Local exact facts; the bound is computed here from the
//!   query's own `(k1, b, avgdl, idf)`, never stored.
//! - `U_i^max` = max of term i's block uppers. ⚠️ A term with `df < BLOCK` carries no block
//!   table, so it gets the loose `idf·(k1+1)` — the supremum of the addend.
//! - `θ` witnessed pre-fetch: a **full** block of `BLOCK` postings puts `BLOCK` documents at
//!   or above its `lower`, so it witnesses the k-th best whenever `k <= BLOCK`.
//!
//! ⚠️ It also rests on every other term's contribution being **non-negative**, which holds
//! for this scorer's Lucene-form idf and is load-bearing enough to state.
//!
//! ## Two numbers, and the second is the point
//!
//! - **sound** — the condition above, decided pre-fetch. What M5e could actually ship.
//! - **oracle** — the same condition with θ taken from the *true* top-k, which no
//!   implementation can know before fetching. It is the ceiling on what any pre-fetch
//!   pruning could ever save on this corpus, so it separates "the condition is too weak"
//!   from "the corpus has no headroom". ⚠️ Reported so a zero cannot be misread.
//!
//!   cargo run --release -p pstore-index --example blockmax

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::panic,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

use pstore_format::{Document, Value, text};
use pstore_index::text::{B, K1, Stats};

/// Postings per block. Tantivy's skip threshold, and M5d's pinned value.
const BLOCK: usize = 128;
/// Rows in the measured corpus — `scripts/depth.sh`'s text gate, unchanged.
const ROWS: usize = 20_000;
/// Vocabulary size.
const VOCAB: usize = 30_000;
/// Terms per document.
const LEN: usize = 120;
/// Queries measured.
const QUERIES: usize = 200;
/// Terms per query. ⚠️ Two and three are the shapes this system serves; M5d chose MaxScore
/// over Block-Max WAND for exactly that reason.
/// ⚠️ **1 is the harness's own control.** At one term `Sigma_(i!=t) U_i^max` is zero and the
/// condition reduces to `upper_t(beta) < theta` — the textbook MaxScore case, which must
/// prune. A harness reporting 0% there is broken, and a zero at widths 2 and 3 would be
/// unreadable without it. This is `ndcg.rs`'s control-ranker discipline applied to a
/// measurement instead of a gate.
const TERMS: [usize; 3] = [1, 2, 3];
/// ⚠️ The control. A term with `df < BLOCK` carries no block table and gets the loose
/// `idf*(k1+1)` bound, which inflates `Sigma U_i^max` — so a zero over mixed queries could be
/// an artifact of the loose bound rather than a fact about pruning. Restricting to queries
/// whose every term is tabled removes it entirely, and if the answer is still zero the finding
/// is structural.
const ALL_TABLED: [bool; 2] = [false, true];
/// Depths of the top-k the pruning must preserve.
///
/// ⚠️ Swept, because `theta` rises as `k` falls: `k = 1` is the most favourable case pruning
/// can ever have, and M5d pins the rule that pruning is disabled above `k = BLOCK` because a
/// full block no longer witnesses `k` documents. A finding that held only at one `k` would be
/// a statement about that `k`.
const TOP_KS: [usize; 3] = [1, 10, 100];

/// What a block table would record about one block. Exactly M5d's 24 bytes, minus the two
/// offsets a measurement does not need.
struct Blk {
    max_tf: u32,
    min_len: u32,
    min_tf: u32,
    max_len: u32,
    bytes: u64,
    /// ⚠️ A **full** block witnesses `BLOCK` documents at or above its `lower`; a short tail
    /// block witnesses fewer, so its `lower` is not a bound on the k-th best.
    full: bool,
}

/// One term's blocks and the facts a block table would carry about them.
struct Blocks {
    idf: f32,
    rows: Vec<Blk>,
    /// Whether this term is long enough to carry a table at all.
    tabled: bool,
    bytes: u64,
}

fn main() {
    let docs = corpus(ROWS, VOCAB, LEN);
    let built = text::build(&docs, text::DEFAULT_TEXT_FIELD);
    let dict = text::TermDict::decode(&built.dictionary).expect("dictionary");
    let stats = Stats {
        doc_count: u64::from(dict.doc_count()),
        total_tokens: dict.total_tokens(),
        df: dict.terms().map(|t| (t.to_owned(), 0)).collect(),
    };
    let n = stats.doc_count as f32;
    let avgdl = stats.avgdl();

    // Terms in descending df, so a query drawn from them looks like a real one: a couple of
    // common terms and a rarer one.
    let mut by_df: Vec<(String, u32)> = dict
        .terms()
        .map(|t| {
            let e = dict.lookup(t).expect("entry");
            (t.to_owned(), e.df)
        })
        .collect();
    by_df.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    println!(
        "corpus   {ROWS} documents, {LEN} terms each, {VOCAB}-term vocabulary, \
         {} distinct terms, avgdl {avgdl:.1}",
        dict.len()
    );
    println!(
        "blocks   BLOCK={BLOCK}, top_k {TOP_KS:?}, {QUERIES} queries per row, widths {TERMS:?}"
    );
    println!();
    println!(
        "width  query shape     k  postings bytes   sound skipped        oracle skipped   \
         tabled terms"
    );

    let mut rng: u64 = 0x5eed_1234_abcd_ef01;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for (width, all_tabled, top_k) in TERMS.iter().copied().flat_map(|w| {
        ALL_TABLED
            .iter()
            .copied()
            .flat_map(move |a| TOP_KS.iter().copied().map(move |k| (w, a, k)))
    }) {
        let (mut total, mut sound, mut oracle, mut tabled_terms, mut all_terms) =
            (0u64, 0u64, 0u64, 0usize, 0usize);
        // ⚠️ Diagnostics, because a zero from a broken harness looks exactly like a zero from
        // a real finding. These say WHY the condition never fires, which is the answer M5d's
        // question 2 actually asked for.
        let (mut d_theta_pre, mut d_theta_true, mut d_others, mut d_slack, mut d_n) =
            (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for _ in 0..QUERIES {
            let mut terms: Vec<String> = Vec::new();
            let mut draws = 0u32;
            while terms.len() < width {
                draws += 1;
                if draws > 100_000 {
                    panic!("could not draw {width} tabled terms");
                }
                // Drawn with the same square bias the corpus was generated with, so a query
                // is made of terms the corpus actually favours.
                let u = (next() % 1_000_000) as f64 / 1_000_000.0;
                let i = ((u * u) * by_df.len() as f64) as usize;
                let (t, df) = by_df[i.min(by_df.len() - 1)].clone();
                if all_tabled && (df as usize) < BLOCK {
                    continue;
                }
                if !terms.contains(&t) {
                    terms.push(t);
                }
            }

            let per_term: Vec<Blocks> = terms
                .iter()
                .map(|t| blocks_of(&dict, &built, t, n))
                .collect();
            all_terms += per_term.len();
            tabled_terms += per_term.iter().filter(|b| b.tabled).count();
            total += per_term.iter().map(|b| b.bytes).sum::<u64>();

            // Σ_i U_i^max, so a term's own can be subtracted out.
            let sum_max: f32 = per_term.iter().map(|b| term_max(b, avgdl)).sum();
            let theta_pre = per_term
                .iter()
                .filter(|b| b.tabled)
                .flat_map(|b| {
                    b.rows
                        .iter()
                        // ⚠️ FULL blocks only. A partial tail block witnesses fewer than
                        // BLOCK documents, so its `lower` is not a bound on the k-th best.
                        .filter(|r| r.full)
                        .map(|r| lower(b.idf, r, avgdl))
                })
                .fold(0.0f32, f32::max);
            let theta_true = true_theta(&dict, &built, &terms, &stats, avgdl, top_k);

            d_theta_pre += f64::from(theta_pre);
            d_theta_true += f64::from(theta_true);
            d_n += 1.0;
            let mut best_slack = f32::INFINITY;
            for b in &per_term {
                if !b.tabled {
                    continue;
                }
                let others = sum_max - term_max(b, avgdl);
                d_others += f64::from(others);
                for r in &b.rows {
                    let u = upper(b.idf, r, avgdl);
                    if u + others < theta_pre {
                        sound += r.bytes;
                    }
                    if u + others < theta_true {
                        oracle += r.bytes;
                    }
                    // How far the *most* prunable block is from qualifying, against the
                    // strongest θ any implementation could have.
                    best_slack = best_slack.min(u + others - theta_true);
                }
            }
            if best_slack.is_finite() {
                d_slack += f64::from(best_slack);
            }
        }
        let pct = |x: u64| {
            if total == 0 {
                0.0
            } else {
                x as f64 * 100.0 / total as f64
            }
        };
        let label = if all_tabled { "all-tabled" } else { "mixed" };
        println!(
            "{width:>5}  {label:<11}  {top_k:>4}  {total:>13}   {sound:>9} ({:>5.2}%)   \
             {oracle:>9} ({:>5.2}%)   {tabled_terms:>3} of {all_terms}",
            pct(sound),
            pct(oracle)
        );
        println!(
            "       mean theta_pre {:.3}, theta_true {:.3}, Sigma_(i!=t) U_i^max {:.3}, \
             closest block misses by {:+.3}",
            d_theta_pre / d_n,
            d_theta_true / d_n,
            d_others / d_n.max(1.0),
            d_slack / d_n
        );
    }
}

fn term_max(b: &Blocks, avgdl: f32) -> f32 {
    if b.tabled {
        b.rows
            .iter()
            .map(|r| upper(b.idf, r, avgdl))
            .fold(0.0f32, f32::max)
    } else {
        // ⚠️ The loose bound, and it is loose on purpose: a term with no block table has no
        // per-block facts, and `idf*(k1+1)` is the supremum of its addend as tf -> infinity.
        b.idf * (K1 + 1.0)
    }
}

fn norm(len: u32, avgdl: f32) -> f32 {
    if avgdl > 0.0 {
        K1 * (1.0 - B + B * len as f32 / avgdl)
    } else {
        K1 * (1.0 - B)
    }
}

/// The largest contribution any document in the block can make: most `tf`, shortest document.
fn upper(idf: f32, r: &Blk, avgdl: f32) -> f32 {
    let tf = r.max_tf as f32;
    idf * (tf * (K1 + 1.0)) / (tf + norm(r.min_len, avgdl))
}

/// The smallest: least `tf`, longest document.
fn lower(idf: f32, r: &Blk, avgdl: f32) -> f32 {
    let tf = r.min_tf as f32;
    idf * (tf * (K1 + 1.0)) / (tf + norm(r.max_len, avgdl))
}

fn blocks_of(dict: &text::TermDict, built: &text::Built, term: &str, n: f32) -> Blocks {
    let e = dict.lookup(term).expect("entry");
    let df = e.df as f32;
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    let raw = &built.postings[e.offset as usize..(e.offset + u64::from(e.bytes)) as usize];
    let postings = dict.decode_list(&e, raw);
    let tabled = postings.len() >= BLOCK;
    let mut rows = Vec::new();
    if tabled {
        // Bytes are apportioned evenly across blocks. ⚠️ An approximation, and stated as one:
        // the encoding is varint, so a block's true size depends on its row gaps. It is
        // within a few percent and it does not change which blocks are prunable, which is
        // what this measures.
        let per = u64::from(e.bytes) / postings.len() as u64;
        for chunk in postings.chunks(BLOCK) {
            let max_tf = chunk.iter().map(|(_, t)| *t).max().unwrap_or(0);
            let min_tf = chunk.iter().map(|(_, t)| *t).min().unwrap_or(0);
            let lens: Vec<u32> = chunk
                .iter()
                .map(|(row, _)| built.fieldnorms.get(*row as usize).copied().unwrap_or(0))
                .collect();
            let min_len = lens.iter().copied().min().unwrap_or(0);
            let max_len = lens.iter().copied().max().unwrap_or(0);
            rows.push(Blk {
                max_tf,
                min_len,
                min_tf,
                max_len,
                bytes: per * chunk.len() as u64,
                full: chunk.len() == BLOCK,
            });
        }
    }
    Blocks {
        idf,
        rows,
        tabled,
        bytes: u64::from(e.bytes),
    }
}

/// θ from the real ranking: the k-th best score, which no pre-fetch decision can know.
fn true_theta(
    dict: &text::TermDict,
    built: &text::Built,
    terms: &[String],
    stats: &Stats,
    avgdl: f32,
    k: usize,
) -> f32 {
    let n = stats.doc_count as f32;
    let mut scores: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
    for t in terms {
        let Some(e) = dict.lookup(t) else { continue };
        let df = e.df as f32;
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        let raw = &built.postings[e.offset as usize..(e.offset + u64::from(e.bytes)) as usize];
        for (row, tf) in dict.decode_list(&e, raw) {
            let len = built.fieldnorms.get(row as usize).copied().unwrap_or(0);
            let tf = tf as f32;
            *scores.entry(row).or_insert(0.0) += idf * (tf * (K1 + 1.0)) / (tf + norm(len, avgdl));
        }
    }
    let mut v: Vec<f32> = scores.into_values().collect();
    v.sort_by(|a, b| b.total_cmp(a));
    v.get(k - 1).copied().unwrap_or(0.0)
}

fn corpus(rows: usize, vocab: usize, len: usize) -> Vec<Document> {
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
