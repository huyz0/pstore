# Load, Hotspots, and Cold-Start Stampedes

**Answers:** Q14
**Status:** Complete (v1)

Hash-uniform placement balances *keys*, not *load*. Real workloads are heavy-tailed in three
independent dimensions, and each needs its own answer.

## Hotspot 1: One index is enormous (size skew)

*A single tenant with 50 TB while the median tenant has 5 MB.*

- **Sharding.** Above a threshold, an index is split into shards by `hash(doc_id)`, and
  placement is on `(index, shard)`. A 50 TB index with 4,000 shards spreads across the whole
  fleet.
- Hash-by-id sharding (turbopuffer does the same) gives uniform shard sizes and no
  range-split hotspots; the cost is that every query fans out to every shard.
- **Fan-out cost is the real limit.** A 4,000-shard query is 4,000 RPCs. So shard count is
  bounded (~256–1,024) and beyond that we scale by making shards bigger, not more numerous.
  Above that, use **two-level fan-out** (a node aggregates a sub-tree of shards).
- Blob-request cost is unaffected: reading 1 TB as 1,000 shards or 100 shards is the same
  bytes; only RPC count changes.

## Hotspot 2: One index is queried enormously (rate skew)

*One index at 50,000 QPS while the median is 0.001 QPS.*

- **Replication factor is dynamic.** `R` is a function of observed QPS:
  `R = clamp(ceil(qps / per_node_qps_budget), 1, R_max)`. Because nodes own nothing,
  **increasing R costs nothing but a cache fill** — no data movement, no coordination.
  This is the single biggest advantage of ownership-free design and should be highlighted in
  the architecture doc.
- R is computed independently by each node from gossiped load hints. Disagreement is harmless
  (it only shifts routing slightly).
- CHBL-style overload skipping (see `routing-and-placement.md`) provides the fast local
  reaction; R adjustment is the slower structural one.

## Hotspot 3: Cold-start stampede

*An index goes from 0 to 10,000 QPS instantly (a deploy, a batch job, a viral event). All R
nodes miss cache and each issues the same blob reads.*

This is the classic cache stampede, amplified by fan-out. Mitigations, in order:

1. **Request coalescing (singleflight).** Concurrent misses for the same block on one node
   collapse into one blob GET. Mandatory, not optional.
2. **Cache-fill prioritization.** Fill centroids and filter indexes first — the files needed
   by *every* query — before cluster data. turbopuffer does exactly this ("prioritization of
   cache fills for more important files ... such as centroids"). A 10 MB centroid file
   fetched once unblocks thousands of queries.
3. **Admission control on cold work.** Bound concurrent cold-fill operations per node; queue
   the rest. A stampede should produce *slow* queries, not a blob-store 503 storm.
4. **Warm-up API.** Let clients declare intent ahead of a known burst (turbopuffer offers
   this; it costs us nothing and prevents the worst case).
5. **Adaptive R ramp.** Detect the ramp and raise R *before* the fleet saturates.

## Hotspot 4: Prefix hotspot in the blob store

Covered by Design rule 5 — hash entropy at the front of the key. Additionally: S3 partition
splits are *reactive* and take minutes, so a brand-new index's first burst is rate-limited.
Mitigation: the shard prefix is drawn from a **large, pre-existing space** (~1M prefixes), so
new indexes land in partitions that are already split from other tenants' traffic. Multi-
tenancy is thus a *performance advantage* here, not just a cost one.

## The tail-latency problem at 10K nodes

With 1,000-way fan-out, p99 of the *query* is roughly p99.9 of the *slowest shard*. Standard
remedies, all of which we can afford because nodes own nothing:

- **Hedged requests.** After p95 of the expected shard latency, re-issue to placement #1.
  Ownership-free means the hedge is always legal — no leader to check with. **Hedging is
  health-aware, not a global switch**: hedge *away from* suspect targets even under load, but
  never hedge blindly under overload (see [`gray-failure.md`](gray-failure.md) §7).
- **Tied requests** for the most expensive shards.
- **Partial results with a completeness flag** — for search, returning 998/1000 shards at
  20 ms often beats 1000/1000 at 400 ms. Expose it: the response states which shards were
  included and the client picks the policy.
- **Bounded fan-out** as above.

## Isolation between tenants

Because one process serves many indexes, a noisy tenant can starve others. Required controls:
- Per-index token buckets for QPS, bytes scanned, and **blob requests issued** (the last is
  the one that maps to cost).
- Per-index concurrency caps and CPU accounting for SIMD distance work.
- Cache quotas so one huge index cannot evict everyone else's working set (see
  `07-caching/cache-hierarchy.md`).

> **Design rule 13.** Every resource that can be consumed on behalf of a tenant must be
> metered per index — CPU, cache bytes, and especially **blob requests**, because that is the
> one that shows up on the bill.

## Open questions raised

- OQ-19: Max practical shard fan-out before two-level aggregation is needed. Guess: ~256.
- OQ-20: Does dynamic R oscillate? Needs a damping/hysteresis design.
- OQ-21: Cache quota policy — hard per-index caps vs. weighted-fair eviction.

## Sources

- [turbopuffer — Architecture (cache fill prioritization, warm cache API, sharding)](https://turbopuffer.com/docs/architecture)
- [turbopuffer — Concepts (hash-by-id sharding, shared WAL)](https://turbopuffer.com/docs/concepts)
- [Consistent Hashing with Bounded Loads — arXiv](https://arxiv.org/pdf/1608.01350)
- [Best practices design patterns: optimizing Amazon S3 performance — AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html)
