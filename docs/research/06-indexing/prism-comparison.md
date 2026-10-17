# PRISM (pg_vectorsearch) Against Our Index: What Is New, and What Is Ours Already

**Answers:** Q49 — does PRISM, Tiger Data's PostgreSQL vector index, carry an idea that cuts
our memory, our bytes per query or our CPU without losing recall?
**Status:** Complete (v2). The three ideas that looked worth taking were specified as
[M97](../../milestones/M97/SPEC.md) and **measured short of their rules** (see
[Measured](#measured-m97-2026-10-17)). The numbers here are the corpus's, not PRISM's.

## What PRISM is

PRISM ("Partitioned Routing Index for Similarity Matching") is the index access method of
`pg_vectorsearch` (Tiger Data, open source, **alpha**, PostgreSQL 18+). It indexes pgvector
columns in place. From its architecture and tuning documents:

- **Partitioned, not a graph.** A hierarchical centroid tree (`fan_out` 32) over posting lists
  of about `target_pages` = 3 pages each. `nlist = max(rows / target_entries, sqrt(rows))`.
- **1-bit RaBitQ codes** in the lists, packed 32 to a group for a lookup-table SIMD kernel
  ("fastscan", `prism.fastscan_bits` 16 or 8).
- **Two stages.** Estimate, then use RaBitQ's error bound (`f_error` stored, `g_error` per
  query) to drop candidates that cannot be in the top `k`, then rerank the survivors against
  the **full-precision vectors in the heap**. The docs claim stage 1 drops 80–95% and that
  1–5% of vectors are reranked.
- **Build-time replication:** SPANN-style boundary replication (`boundary_epsilon` 0.35) and
  a SOAR secondary assignment (`soar_lambda` 1.0).
- **Compressed centroids** (`centroid_compression`, RaBitQ for L2 and cosine) with the best
  `probe_expand` × nprobe (2.0) re-sorted by exact centroid distance.
- **`nprobe` auto** = 0.5 × sqrt(nlist), clamped to 10–2048, aiming at about 0.95 recall@10.

The headline claim is 6 ms p50 at 95% recall on 100M vectors, faster than HNSW on a machine
with a quarter of the memory. ⚠️ **It is the vendor's number on an alpha release**, and the
docs do not cover filtered search.

## Most of it is already our design

| PRISM | Ours | Where |
|---|---|---|
| Partitioned posting lists, not a graph | SPANN-family balanced lists | `vector-index-survey.md`, D-8 |
| 1-bit RaBitQ codes with an error bound | The same, ε derived (3.90) | `quantization.md`, D-11 |
| `boundary_epsilon` | `Params::replicas`, `boundary` | `cluster.rs` |
| Estimate, then rerank | int8 rung in the same round trip, `exact` in a fourth | C-3, D-12 |
| `nprobe`, `prism.rerank` | `p`, `rerank: none \| fast \| exact` | `vec_index.rs` |

**PRISM's memory advantage over HNSW is the one this design was built on** (lists plus
32× codes; `storage-to-cache-ratio.md`). There is no "adopt PRISM" step. What is left are
details, and the deciding difference between the two systems decides which details transfer:

> ⚠️ **PRISM's second stage reads the heap from a buffer cache it may page in; ours must
> choose its ranges before rung 0 has ranked, or pay a round trip** (D-34). So everything
> PRISM does *after* the estimate costs it CPU and memory, and costs us **bytes we already
> chose to move** — or depth we are not allowed.

## The three ideas that looked transferable (specified as M97)

### 1. The rerank tier sets our bytes, not the 1-bit tier — and it could be narrower

> ⚠️ **Measured by M97: not at 384d.** A 4-bit tier gives 0.856 against int8's 0.981 on the
> clustered corpus; see [Measured](#measured-m97-2026-10-17). The banner wins over this section.

`rerank: fast` fetches the int8 record of **every probed row** beside its 1-bit code, in one
call (`vec_index.rs`; C-3). At 384d a probed row moves 72 bytes of 1-bit code and 392 bytes of
int8: **the int8 tier is 84% of the bytes a default query moves** and the larger part of the
resident working set. `cost-model.md` already records it: the scan is the int8 tier's,
about 6× a 1-bit-only estimate.

Measured again for M97 (`./scripts/recall.sh`, provisional: container), clustered 20,000 × 384d,
p=8, over=32: `none` 0.3090 recall@10 at 0.13 MB a query, `fast` **0.9810 at 0.84 MB**, `exact`
0.9990 at **0.62 MB**. ⚠️ So at the defaults the four-round `exact` mode moves **fewer** bytes
than `fast`. The 320 survivors' `f32` rows cost less than every candidate's int8 record. `fast`
buys a round trip with bytes, and the rate is 0.22 MB a query.

PRISM does not narrow this tier (it reranks from the heap). The idea beside it is
**extended RaBitQ**: `B` bits a dimension on the rotated vector, with a per-vector rescale
chosen to minimise error. LanceDB describes 2–4 bits as lower-error than PQ at the same
budget; we found **no published recall figure** at our operating point, so it is measured,
not assumed:

| Rerank tier (384d) | Bytes a probed row | vs. int8 today |
|---|---|---|
| int8 (`Sq8`) | 72 + 392 = 464 | 1.00× |
| 4-bit (3 extra bits) | 72 + 200 = 272 | 0.59× |
| 2-bit (1 extra bit) | 72 + 72 = 144 | 0.31× |

⚠️ **Extra bits beside the 1-bit code, over the *padded* dimension.** An extended-RaBitQ code's
sign bit is the 1-bit code's bit, so only `B − 1` bits a coordinate are new. The rotation codes
`next_power_of_two(D)` coordinates (512 at 384d; `rabitq.rs`). A standalone `B·D/8` record would
be both wrong and dearer: the first draft of M97 priced it so, and its review caught it.

If a 4-bit tier holds int8's recall, it is offered **as an index option**, because the width is
written at build time and a customer who wants the last 0.5 point of recall can keep int8.

### 2. The error bound can size the survivor set; a constant does today

> ⚠️ **Measured by M97: it keeps recall and keeps more rows**, 500 and 1,024 against 320; see
> [Measured](#measured-m97-2026-10-17).

`Query::default()` records that oversample 8 gives 0.801 recall@10 and 32 gives 0.981, and
OQ-40 asks for the factor per rung. PRISM's answer is not a factor: **keep every candidate
whose upper bound reaches the `k`-th best lower bound.** `Quantizer::error_bound` exists and
is tested; no search path calls it.

⚠️ **What it can and cannot buy us.** It cannot cut the int8 bytes: those are fetched before
rung 0 ranks. It can cut the **fourth round's** float32 fetch (`rerank: exact` reads 320
survivors × 1,536 bytes = 0.49 MB at the defaults), and it can replace a tuned constant with a
derived one. And C-3 predicts a risk PRISM's claim does not mention: neighbours inside one list
differ by less than the 1-bit error, so the bound may keep **more** than 320. That outcome
is itself the finding — the 80–95% PRISM reports is not ours until measured.

The uniform corpus says where to look. At p=8, `fast` and `exact` both give **0.5150** (10,000 ×
384d, 50 lists). Rerank precision is not what loses recall there; rung 0's cut or the probe set is.
A survivor set sized by the bound rather than by a constant is a test of the first of those.

### 3. The centroid table can be stored at int8

> ⚠️ **Measured by M97: no recall lost at any p**, but the probe overlap at clustered p=4 is
> 0.9875, under the rule's 0.99; see [Measured](#measured-m97-2026-10-17).

The centroid object is `f32` (`vec_index.rs`), sized at about 768 MB in production
(`vector-index-survey.md`), and it is the one object that must stay resident (D-9). At int8
it is a quarter of that. PRISM goes further (1-bit plus an exact re-sort), and **the exact
re-sort is the part we cannot copy**: re-reading `f32` centroids after choosing is a
data-dependent fetch. An int8 table needs no re-sort if its probe order matches `f32`'s,
and the only other consumer, the residual estimate's `<c, q>`, adds the same error to every
row of a list — a ranking shift between lists, measured rather than argued.

## Measured (M97, 2026-10-17)

`./scripts/recall.sh --prism`, **provisional**: a Linux cloud container, synthetic corpora, 384d,
k=10, p=8 and over=32 unless stated. Hit counts are out of 1,000 (100 queries × 10). Each decision
rule was written in the spec before the harness existed.
[VERIFIED.md](../../milestones/M97/VERIFIED.md) has the full output.

**1. The rerank tier: no narrower tier holds int8's recall.**

| Tier | Bytes a probed row | Clustered 20,000 (100 lists) | Uniform 10,000 (50 lists) | MB/query clustered |
|---|---|---|---|---|
| int8 (control) | 464 | **0.9810** (981) | 0.5150 (515) | 0.841 |
| exq B=4 | 272 (0.59×) | 0.8560 (856) | 0.5110 (511) | 0.493 |
| sq4 (unrotated) | 272 (0.59×) | 0.6990 (699) | 0.5040 (504) | 0.493 |
| exq B=2 | 144 (0.31×) | 0.5540 (554) | 0.4110 (411) | 0.261 |

⚠️ **The clustered corpus decides it, and it is the one the module docs warned about.** Rows in one
list differ by about the third decimal place of `<o, q>`, and 4 bits on the residual does not
resolve that where 8 bits on the row does. The rotation and the rescale search are
worth having: exq beats the simple 4-bit code by **15.7 points** at the same bytes. But they do
not make up for four bits. On uniform data, where neighbours are far apart, B=4 is within 0.004.
OQ-170 is **answered no** at 384d.

⚠️ A wider exq does not rescue the bytes. At 384d the padding to 512 coordinates costs B=6 a
400-byte record, 0.86× int8, so the saving is gone before recall is measured (not measured).

**2. Bound-sized survivors keep recall, and keep more rows than the constant.**

| Survivors (exact rerank) | Clustered recall | Mean survivors | Uniform recall | Mean survivors |
|---|---|---|---|---|
| over=32 (today) | 0.9990 (999) | 320 | 0.5150 (515) | 320 |
| bound, cap 1,024 | 1.0000 (1000) | **499.8** | 0.5170 (517) | **1,024** (every query at the cap) |

C-3's prediction holds: the 1-bit error is wider than the gaps it must separate, so every
interval overlaps the k-th lower bound. PRISM's "drops 80–95%" is a claim about lists whose
members differ by more than ours do. The recall half of the rule passes on both corpora. The
survivor half fails on both. OQ-40 stays open: the bound cannot replace the constant at 1 bit.
The bound itself is sound: `the_residual_bound_holds_and_is_tight` measures 6 breaches in 40,000
pairs.

**3. An int8 centroid table loses no recall, and misses the overlap rule at one point.**

| p | Clustered f32 / int8 recall | Overlap | Uniform f32 / int8 recall | Overlap |
|---|---|---|---|---|
| 4 | 0.9800 / 0.9800 | **395/400 = 0.9875** | 0.3020 / 0.3050 | 396/400 = 0.9900 |
| 8 | 0.9810 / 0.9810 | 794/800 = 0.9925 | 0.5150 / 0.5170 | 795/800 = 0.9938 |
| 16 | 0.9810 / 0.9810 | 1592/1600 = 0.9950 | 0.7380 / 0.7390 | 1592/1600 = 0.9950 |

The table is a quarter of the size: 39,200 bytes against 153,600 for 100 lists. Recall is equal
or higher in every cell. But the rule asked for an overlap of at least 0.99 everywhere, and
clustered p=4 has 0.9875, five lists in 400. **The rule says not yet**, and the ledger may not
move it. What the number says is that the differing lists cost no recall here. A rule led by
recall, or real embeddings, is what would close OQ-171.

## Not taken, and why

| Idea | Why not now |
|---|---|
| **fastscan** layout | H-1 / D-90: the scan is memory-bound by about 7× per core; a faster kernel buys nothing until a profile shows 1-bit scoring is compute-bound (OQ-173). |
| **Centroid tree** | A tree level not in the first parallel round is a data-dependent chain (D-34). Matters above ~1M centroids per index (OQ-37, OQ-53). |
| **SOAR** | Plausible: more recall a probe than boundary replication for the same stored copies. But replication is clamped to 0 in the engine (M5g, item 14), so it is untestable end to end today (OQ-172). |
| **Auto `nprobe`** = 0.5·sqrt(nlist) | `p` = 8 is already measured as a 2× margin on our corpus; a formula fitted to PRISM's lists is not evidence about ours. |

## Open questions raised

- OQ-170 — Can a narrower rerank tier (extended RaBitQ, 2–4 bits) replace int8 at its recall?
  **Answered no (M97)**: 0.856 against 0.981 on the clustered corpus.
- OQ-40 — Optimal oversample per rung. **The bound is not the answer (M97)**: 500 and 1,024
  survivors against 320.
- OQ-171 — Does an int8 centroid table rank probes as `f32` does, at every `p` we use? **Recall yes,
  overlap 0.9875 at clustered p=4 (M97)**: open under the rule as written.
- OQ-172 — SOAR against boundary replication, once a replicated row is written once.
- OQ-173 — Is 1-bit scoring ever compute-bound on our hardware, so that fastscan would pay?

## Sources

- [pg_vectorsearch — GitHub (README)](https://github.com/timescale/pg_vectorsearch)
- [pg_vectorsearch — architecture](https://github.com/timescale/pg_vectorsearch/blob/main/docs/architecture.md)
- [pg_vectorsearch — tuning](https://github.com/timescale/pg_vectorsearch/blob/main/docs/tuning.md)
- [Announcing pg_vectorsearch — Tiger Data](https://www.tigerdata.com/blog/announcing-pg_vectorsearch-scalable-open-source-vector-search-postgresql)
- [pg_vectorsearch claims 6 ms at 100M vectors; filtered queries can come back short — DEV](https://dev.to/gabrielanhaia/pgvectorsearch-claims-6-ms-at-100m-vectors-filtered-queries-can-come-back-short-5cm2)
- [SOAR: new algorithms for even faster vector search with ScaNN — Google Research](https://research.google/blog/soar-new-algorithms-for-even-faster-vector-search-with-scann/)
- [RaBitQ quantization (extended RaBitQ, 2–4 bits) — LanceDB](https://www.lancedb.com/blog/feature-rabitq-quantization)
