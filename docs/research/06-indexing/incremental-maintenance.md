# Incremental Index Maintenance: Never Rebuild a Billion Vectors

**Answers:** Q23
**Status:** Complete (v1)

## The problem

A clustered index is built by clustering. Clustering is expensive and global. If every write
required re-clustering, a 1B-vector index would be unusable. Meanwhile our LSM produces new
segments constantly.

## Two composing mechanisms

### 1. LSM-level: new segments get their own index
Each segment carries its own complete clustered index. A query probes every live segment's
index and merges results. Segment count is bounded by the merge policy (≈10–12), so this is
`1 Rpar` regardless.

This alone handles freshness: **a newly written segment is immediately searchable** without
touching any existing index. It is also why compaction policy and query latency are the same
conversation.

Cost: recall degrades slightly with segment count (each segment probes `p` lists
independently, so the effective candidate pool is fragmented). Mitigation: allocate `p` across
segments proportional to segment size, and slightly oversample.

### 2. Segment-level: LIRE (SPFresh) for in-place evolution
Within a segment (particularly a large L2 base segment), rebuilding on every compaction is
still too expensive. **LIRE** gives us local, incremental rebalancing:

| Operation | Rule |
|---|---|
| **Insert** | Append to the nearest posting list. |
| **Delete** | Mark in a version map; GC deferred. |
| **Split** | Posting list exceeds `max_posting_len` ⇒ divide into smaller partitions. |
| **Merge** | Posting list below `min_posting_len` ⇒ consolidate with neighbours. |
| **Reassign** | After a split/merge, re-check **only boundary vectors** that now violate the Nearest Partition Assignment rule — provably a small set in a good index, so **no full scan**. |

Published result: **superior query latency and accuracy versus global rebuild, using 1% of
the DRAM and <10% of the cores at peak**, on a billion-scale disk-based index at 1%/day
update rate.

That resource profile is what makes this affordable on shared, multi-tenant nodes doing
compaction opportunistically.

## Adapting LIRE to immutable object storage

SPFresh assumes **in-place** block updates on SSD. We cannot mutate objects. The adaptation:

- Posting lists are **blocks within a segment**. A "split" produces new blocks; the segment
  containing them is rewritten (or a delta segment is written) and published by CAS.
- **Batch the LIRE operations.** Instead of applying split/merge/reassign per insert, we
  accumulate the *decisions* during a compaction pass and apply them all in the single
  rewrite. LIRE's value is preserved — we still only touch boundary vectors — but the write
  granularity becomes a segment rewrite instead of a block update.
- **The version map becomes the delete vector** (`05-storage-engine/mutations-and-mvcc.md`),
  which we already have. Nice convergence.
- **Centroid drift** is tracked in segment stats; when a segment's centroids have drifted past
  a threshold (measured by average assignment distance vs. build time), it is scheduled for a
  re-cluster compaction.

> **D-19.** Vector-index maintenance is a *distinct work type* from data compaction, with its
> own trigger (centroid drift, posting-list length distribution, delete ratio) and its own,
> slower cadence. Data compaction is cheap and frequent; re-clustering is expensive and rare.
> Coupling them would force one of the two to the wrong frequency.

## The centroid hierarchy

At 1M+ centroids, selecting the top-`p` centroids is itself an ANN problem. SPANN keeps a
memory index over centroids. For us:

- **Small/medium indexes:** centroids fit comfortably in RAM; brute-force scan of the centroid
  table with SIMD is fastest and simplest.
- **Large indexes:** a two-level hierarchy — coarse centroids over fine centroids — or an HNSW
  over the centroid table. **This is the one place HNSW is appropriate**, because the centroid
  table is small, cached, and memory-resident, so its sequential hops cost nanoseconds.
- Centroids are quantized too (RaBitQ), keeping even a 1M-centroid table under ~100 MB.

## Handling drift and distribution shift

Embedding distributions shift (a tenant changes model, or ingests a new corpus). Symptoms:
posting lists become badly unbalanced, recall drops. Detection:
- Track the distribution of posting-list lengths per segment (in segment stats).
- Track average query-to-chosen-centroid distance over time.
- Track measured recall via **periodic shadow exact queries** on a small sample — the only
  ground truth we can get in production.

> **D-20.** Ship continuous recall measurement from day one: a small fraction of queries are
> also run exactly (in the background, off the critical path) and the recall delta is recorded
> per index. Without this, ANN quality regressions are invisible until a customer complains.
> This is cheap for us specifically because exact scan over quantized codes is fast and the
> data is already cached.

## Handling model changes / reindexing

Changing embedding model or dimension is a full reindex. Because branching is one PUT
(`mutations-and-mvcc.md`), the migration story is good:
1. Branch the index.
2. Reindex into the branch (writes go to both via dual-write, or replay).
3. Atomically swap HEAD to the branch's manifest.
4. Drop the old branch.
Zero downtime, one CAS to cut over, trivial rollback.

## Cost summary

| Operation | Cost |
|---|---|
| New data searchable | 1 segment build (already happening) |
| LIRE rebalance | Batched into a compaction; only boundary vectors touched |
| Full re-cluster | Rare; triggered by drift metrics |
| Recall monitoring | ~0.1% of queries, off critical path |

## Open questions raised

- OQ-50: How much recall is lost from segment fragmentation (probing `p` lists across 10
  segments vs. one)? Measure — this determines the merge policy's aggressiveness.
- OQ-51: LIRE on immutable storage — does batching decisions into compaction preserve its
  quality guarantees, or does deferral degrade the partition quality? **This is the biggest
  unvalidated assumption in the indexing design.**
- OQ-52: Drift thresholds for triggering re-cluster.
- OQ-53: Centroid hierarchy crossover point (when brute-force centroid scan stops winning).

## Sources

- [SPFresh: Incremental In-Place Update for Billion-Scale Vector Search — SOSP 2023](https://dl.acm.org/doi/10.1145/3600006.3613166)
- [SPFresh — Microsoft Research](https://www.microsoft.com/en-us/research/publication/spfresh-incremental-in-place-update-for-billion-scale-vector-search/)
- [SPFresh notes — Hrushikesh Dokala](https://hrushikesh.dev/notes/spfresh/)
- [SPANN: Highly-efficient Billion-scale ANN Search — arXiv](https://arxiv.org/pdf/2111.08566)
- [turbopuffer — Architecture (SPFresh, minimizing write amplification)](https://turbopuffer.com/docs/architecture)
