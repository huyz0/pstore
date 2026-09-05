# Build Roadmap: De-Risking Order

**Synthesizes:** D35
**Status:** v1 proposal

## The principle

Build in **descending order of "if this is wrong, the architecture is wrong."** Not in order
of user-visible value. Three things could invalidate the design, and all three are cheap to
test before writing a search engine:

1. Does blob CAS actually behave as assumed under contention? (M0)
2. Does the lane design actually remove the write bottleneck? (M1)
3. Does LIRE survive being batched into immutable-segment rewrites? (M4 — **prototype early,
   out of order, because it is the biggest unknown**)

## Milestones

### M0 — Substrate truth (2–3 weeks)
**Goal: replace every guessed number in the research with a measured one.**

- `pstore-blob`: the `BlobStore` trait over `object_store`, with congestion control and
  per-tenant request accounting.
- The **fault-injecting store**: latency distributions, 412/409/503, delayed visibility,
  deterministic seeds.
- A **microbenchmark suite** run inside AWS/GCP/Azure answering: OQ-3 (real TTFB percentiles),
  OQ-5 (**CAS throughput and loss curves under 2/8/64/512 contenders — the single most
  important measurement in the project**), OQ-2 (range-coalescing break-even `G*`), OQ-6 (ABA
  under multipart ETags), OQ-24 (Express One Zone semantics and cost).

**Exit:** a document of measured numbers replacing §OQ-2/3/5/6/24, and a go/no-go on the CAS
premise.

### M1 — Single-node storage engine (4–6 weeks)
- `pstore-format`: segment encode/decode, footer, blocks, zone maps.
- `pstore-manifest`: HEAD, epochs, commit protocol, delta chains.
- `pstore-wal`: lanes, group commit, lane bitmap, tail probing, **cross-index bundles**
  (footer index, sort by `(index_id, shard)`) and the **memtable/freshness layer** — these are
  not a later optimization; a per-index write path would have to be rewritten (OQ-84/85).
- `pstore-cache`: `foyer` + class-aware admission.
- **Exact brute-force vector search only.** No ANN yet — it already serves the majority of
  real indexes (D-10).

**Exit:** write, read, filter, exact-search a single index on real S3. Measured
`RA(write) = 1 W` and `RA(cold read) ≤ 3 Rseq`, **asserted in tests** (D-34).

### M2 — Multi-writer correctness (3–4 weeks)
- `pstore-sim`: deterministic simulation harness (**built before the distributed features**,
  D-32).
- N concurrent writers on N lanes, one index; property tests over commit interleavings;
  injected pauses, partitions, and CAS storms; **assert Invariant I1**.
- Compaction as optimistic work + CAS-on-publish, with duplicate-work suppression.
- GC with epoch retention.
- **Bundle-recovery proof (OQ-91):** under node death, placement change, and fallback writes,
  `HEAD.lane_watermarks` + forward probing of placement lanes must find *every* un-folded
  record. If this cannot be shown, cross-index bundling is unsafe and the cost model collapses.

**Exit:** linearizability of the epoch sequence under adversarial scheduling, at 100+ logical
writers, plus the bundle-recovery proof. This is where the architecture is either proven or
disproven.

### M3 — Vector index (5–7 weeks)
- `pstore-quant`: RaBitQ + int8 SQ + SIMD kernels (`simsimd`), rerank ladder.
- `pstore-index-vec`: SPANN-family build — hierarchical balanced clustering, boundary
  augmentation, query-aware pruning.
- Recall harness against SIFT1M/Deep1B subsets; **recall as a CI gate** (D-35).

**Exit:** 90–95% recall@10 at ≤3 round trips cold, measured, on 100M vectors.

### M3.5 — LIRE spike (run in parallel from M1, 2 weeks of effort)
**Out of order deliberately.** Prototype LIRE-style split/merge/reassign batched into an
immutable segment rewrite, and measure partition quality against a global rebuild (OQ-51).

**Exit:** either "batched LIRE preserves quality" or "we need a different maintenance
strategy" — known *before* M3 hardens around it.

### M4 — Cluster (4–6 weeks)
- `pstore-cluster`: gossip (SWIM+Lifeguard via `chitchat`/`foca`), LRH+CHBL placement,
  work assignment.
- Roster seeding from the blob store; routing hop; hedged requests.
- Simulated 10,000-node runs in `pstore-sim`; measure convergence and post-scale-out cold
  ratio.
- **Per-AZ cells** ([`../04-cluster/az-topology.md`](../04-cluster/az-topology.md)) and
  **gray-failure detection** ([`../04-cluster/gray-failure.md`](../04-cluster/gray-failure.md)):
  cross-AZ probe mesh, blob health bulletin, peer-relative outlier detection, self-eviction
  draining. Gray fault injection in the simulator is part of this milestone, not a follow-up.

**Exit:** 1,000 real nodes, 10,000 simulated; add/remove 50% of the fleet with zero data
movement and a measured, bounded cache dip.

### M5a — Sparse vectors and hybrid (2 weeks)
- Sparse/learned-sparse retrieval over the generic-impact posting lists built in M3.
- RRF fusion; exercise the `prefetch[]` + `fusion` path.
- **Before BM25 deliberately:** sparse search is *exact*, so it is a much smaller subsystem and
  gives a real hybrid story for a fraction of the work
  ([`../06-indexing/modalities-and-sequencing.md`](../06-indexing/modalities-and-sequencing.md) §7).

### M5b — Full-text (4–5 weeks)
- Tantivy behind a `BlobStore`-backed `Directory`; block-max metadata in the index section.
- BM25, two-pass IDF, trigram regex.
- MS MARCO quality evaluation; NDCG/MRR as CI gates.

### M6 — Multi-tenancy at scale (3–4 weeks)
- Sharded catalog; enumeration in one parallel round.
- Per-tenant quotas and metering, especially **blob requests** (Design rule 13).
- Synthetic 1M-index workload (OQ-72).

**Exit:** 1M indexes, open latency unaffected by index count, zero LISTs on any hot path.

### M7 — Production hardening (ongoing)
- GCS + Azure backends and the `Capabilities` matrix.
- Time travel, branching, warm API, streaming responses.
- Observability, SLOs, chaos testing, Jepsen-style verification.
- BYOC packaging.

## Cross-cutting, from day one

| Discipline | Enforcement |
|---|---|
| Round-trip depth ≤3 | Test assertion against the fault-injecting store (D-34) |
| Zero LIST on hot paths | Lint + the awkward `list_unrestricted` API name (D-2) |
| Blob requests per op | Counter assertions in tests; per-tenant metrics in prod |
| Recall / NDCG | CI gates (D-35) |
| Invariant I1 | Deterministic simulation (D-32) |

## Rough sequencing

```
M0 ──▶ M1 ──▶ M2 ──▶ M3 ──▶ M4 ──▶ M5 ──▶ M6 ──▶ M7
        └─────── M3.5 LIRE spike (parallel) ──┘
```
~7–9 months to a credible single-cloud beta with one engineer-equivalent of focus; faster with
parallelism on M3/M5, which are largely independent of M2/M4.

## The three questions that decide whether to continue

0. **Before M1:** what is the real tenancy distribution (OQ-84)? If most indexes turn out to
   be large and continuously written, cross-index bundling is over-engineering and the simpler
   per-index path is fine. This is a customer-discovery question, not an engineering one, and
   it is cheap to answer first.
1. **After M0:** does CAS behave well enough under contention? If a single key can't sustain
   even a few writes/second reliably, the partitioned-register plan needs rethinking.
2. **After M2:** does the simulation find correctness bugs we cannot close? A masterless design
   that needs a lock service is just a worse design with extra steps.
3. **After M3.5:** does incremental clustering work on immutable storage? If not, the fallback
   is periodic full re-clustering during compaction — more expensive, still viable, but it
   changes the cost model.
