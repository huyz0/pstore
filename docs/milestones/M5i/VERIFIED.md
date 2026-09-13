# M5i — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **This milestone's artifact is a measurement, not a behaviour.** There is no red-green
cycle to report: the evidence is a number and the command that produced it,
`./scripts/msmarco.sh`, whose output is quoted here rather than paraphrased.

1. **The evaluation runs end to end** — `./scripts/msmarco.sh` over the full collection:
   **8,841,823 passages** in 23 shards of 400,000, **6,980** dev-small queries, every one of
   them judged. **MRR@10 = 0.1775, NDCG@10 = 0.2241.**
2. **Peak resident memory** — **4.8 GB** from the same `./scripts/msmarco.sh` run, printed
   per shard by the harness and rising
   monotonically to that ceiling at the last shard, against the 8 GB hard guard that aborts
   rather than swaps. 40% of headroom left, so the shard size is not on the edge.
3. ⚠️ **Against the published reference** — `./scripts/msmarco.sh` reports 0.1775 against the
   BM25 reference of **≈0.18**. Within about 1% of a baseline this project did not choose, and
   **below** rather than above it, which is the direction that rules out scoring against
   qrels we derived. ⚠️ The analyzer has no stemming and no stopwords, so the gap being this
   small is the surprise; the spec expected a wider one and said the gap would be a statement
   about the analyzer.
4. **Every query answered from the multi-segment path** — criterion 1's `./scripts/msmarco.sh`
   run is one call per query over all 23 targets, statistics merged across them, one fusion over the union. The harness holds no
   per-shard shortcut: criterion 5's arm had to be written separately to produce one.
5. ⚠️ **The statistics span the shards, and the effect is now measured on real text** — the
   same 6,980 queries with PSTORE_MSMARCO_PER_SHARD set — `./scripts/msmarco.sh` again, each
   shard scored against **only its own** corpus:
   **MRR@10 = 0.1692, NDCG@10 = 0.2143.** Merging the statistics is worth **+0.0083 MRR@10**
   (+4.9% relative) and +0.0098 NDCG@10. M5c showed global IDF changes the top-1; this is its
   size on 8.8M real passages — real, consistent at every 500-query checkpoint, and small.
   ⚠️ **A retracted first arm, kept because the number is still true of something.** That arm
   called the same query path per shard and sorted the results together, and measured
   **0.0038**. RRF is the default fusion, so every shard's rank-1 hit returns exactly 1/61 and
   the merge is a tie lottery. It is a fact about comparing independent *fusions*, not about
   statistics, and it is close to what M5f already tests. The shipped arm compares raw BM25
   through the text index with each shard's own summary, which is where statistics enter.
6. **`provisional`** — WSL2, one run, and `./scripts/msmarco.sh` prints that word in the
   header line above every result it reports.
   Only the ranking numbers are claimed; nothing here is a timing figure.
7. **Gates** — `./scripts/gates.sh` green. `scripts/ndcg.sh` and `scripts/recall.sh` are
   untouched: nothing outside the new example and its script changed, and no crate was edited.

## What this closes, and what it does not

- **M5's exit criterion is met.** It had stood as `NOT-RUN` since M5c, and
  [`roadmap.md`](../../research/11-design/roadmap.md) gave the reason as "no network and no
  dataset here". ⚠️ **Both halves were false when re-tested**, and had been for some time —
  one range request to the official corpus settled it. The blocker was never wrong when it was
  written; it was never re-checked. Nothing distinguishes a stale blocker from a live one
  except retrying it, and the backlog has no mechanism that does.
- ⚠️ **Not a CI gate**, deliberately. `./scripts/msmarco.sh` needs a 1.06 GB download and about
  two hours per arm on this machine, which is the argument that already keeps `recall.sh` and
  `depth.sh` outside `cargo test`. The roadmap's "NDCG/MRR as CI gates" line stays open, and it
  is a question about getting the corpus into CI rather than about measuring it.
- ⚠️ **The two arms are not a controlled experiment on fusion.** Criterion 1's arm ranks by
  RRF over a single leg, which is order-preserving, and criterion 5's ranks by raw BM25 — so
  the top-10 sets are comparable and only the statistics differ. That is the comparison
  intended, but it is an argument about the code and not something a test asserts.
- **No claim about the analyzer beyond the gap.** Adding a stemmer would move this number and
  make it incomparable with the run recorded here.
