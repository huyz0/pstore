# PRISM (pg_vectorsearch) Against Our Index: What Is New, and What Is Ours Already

**Answers:** Q49 — does PRISM, Tiger Data's PostgreSQL vector index, carry an idea that cuts
our memory, our bytes per query or our CPU without losing recall?
**Status:** Complete (v1). The three ideas worth taking are specified as
[M97](../../milestones/M97/SPEC.md); the numbers here are the corpus's, not PRISM's.

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

## The three ideas that transfer (specified as M97)

### 1. The rerank tier sets our bytes, not the 1-bit tier — and it could be narrower

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
| 4-bit | 72 + 200 = 272 | 0.59× |
| 2-bit | 72 + 104 = 176 | 0.38× |

If a 4-bit tier holds int8's recall, it is offered **as an index option**, because the width is
written at build time and a customer who wants the last 0.5 point of recall can keep int8.

### 2. The error bound can size the survivor set; a constant does today

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

The centroid object is `f32` (`vec_index.rs`), sized at about 768 MB in production
(`vector-index-survey.md`), and it is the one object that must stay resident (D-9). At int8
it is a quarter of that. PRISM goes further (1-bit plus an exact re-sort), and **the exact
re-sort is the part we cannot copy**: re-reading `f32` centroids after choosing is a
data-dependent fetch. An int8 table needs no re-sort if its probe order matches `f32`'s,
and the only other consumer, the residual estimate's `<c, q>`, adds the same error to every
row of a list — a ranking shift between lists, measured rather than argued.

## Not taken, and why

| Idea | Why not now |
|---|---|
| **fastscan** layout | H-1 / D-90: the scan is memory-bound by about 7× per core; a faster kernel buys nothing until a profile shows 1-bit scoring is compute-bound (OQ-173). |
| **Centroid tree** | A tree level not in the first parallel round is a data-dependent chain (D-34). Matters above ~1M centroids per index (OQ-37, OQ-53). |
| **SOAR** | Plausible: more recall a probe than boundary replication for the same stored copies. But replication is clamped to 0 in the engine (M5g, item 14), so it is untestable end to end today (OQ-172). |
| **Auto `nprobe`** = 0.5·sqrt(nlist) | `p` = 8 is already measured as a 2× margin on our corpus; a formula fitted to PRISM's lists is not evidence about ours. |

## Open questions raised

- OQ-170 — Can a narrower rerank tier (extended RaBitQ, 2–4 bits) replace int8 at its recall?
- OQ-40 — Optimal oversample per rung — **now with a candidate answer: no constant at all**.
- OQ-171 — Does an int8 centroid table rank probes as `f32` does, at every `p` we use?
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
