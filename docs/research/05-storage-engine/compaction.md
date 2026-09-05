# Compaction: Choosing Amplification When Storage Is $0.023/GB

**Answers:** Q17
**Status:** Complete (v1)

## The classical tradeoff, re-priced

| Strategy | Write amp | Read amp | Space amp |
|---|---|---|---|
| **Leveled** | **10×–30×** in practice | O(L) | O((T+1)/T) — low |
| **Tiered** | ~1× | O(T·L) | **O(T)** — high |

RocksDB's own history is instructive: they *shifted focus from write amplification to space
amplification* because space is what costs money on real hardware.

**On object storage the calculus changes again:**

| Cost | Local NVMe | Object storage |
|---|---|---|
| Write amplification | wears the device, consumes IOPS | **PUT requests + CPU + bandwidth** |
| Space amplification | expensive ($0.10+/GB) | **cheap ($0.023/GB)** |
| Read amplification | ~100 µs per extra read | **~30 ms per extra round trip** |

So on object storage:
- **Space amplification is the cheapest of the three.** A 2× space overhead costs
  $0.023/GB-mo extra — trivial.
- **Read amplification is the most expensive**, because it is measured in 30 ms round trips
  on a user-facing query, and because our whole latency budget is ~3 round trips.
- Write amplification is a moderate, *dollar-denominated* cost (PUTs + compute), not a
  device-lifetime cost.

> **D-7.** Bias toward **leveled-ish compaction with a small number of levels**, accepting
> higher write amplification to keep the number of segments a query must open low. Space
> amplification is the currency we spend most freely; round trips the least.

## The policy

**Three tiers, not many levels:**

| Tier | Contents | Target segment size | Trigger |
|---|---|---|---|
| **L0** | Direct fold of WAL lanes. Overlapping. | 64–256 MiB | ≥8 lanes-worth or 128 MiB unindexed |
| **L1** | Non-overlapping within a shard, sorted | 512 MiB–1 GiB | ≥6 L0 segments |
| **L2** | The stable base | 1–4 GiB | ≥8 L1 segments or ≥20% garbage |

A query therefore opens at most `|L0| + |L1| + 1` segments, bounded by policy at ~10–12.
That directly bounds the query's `Rpar` fan-out, which is the number we actually care about.

**Vector-index compaction is different and is the hard part.** Rebuilding a clustered ANN
index over a merged segment is expensive (clustering is superlinear-ish). This is why
**SPFresh/LIRE matters**: it lets us *incrementally rebalance* clusters at compaction time
instead of re-clustering from scratch. See `06-indexing/incremental-maintenance.md`.

## Who runs it: nobody in particular

Per `04-cluster/ownership-and-leases.md`:
- `needed_compaction(manifest)` is a **pure function** every node evaluates for the shards it
  is a placement for.
- Deterministic assignment (LRH placement #0) plus jittered backup timers suppress duplicates.
- A soft claim is written for jobs above a cost threshold.
- The compactor writes new immutable segments, then CASes HEAD. Losers discard.

**There is no compaction queue, no scheduler, and no state to recover.** If every node
restarts, the work list regenerates from the manifest.

## Compaction economics per job

Merging `n` input segments of total size `S` into one output:
- Reads: `S` bytes, `~S / block_size` GETs, but **coalesced into large sequential ranges** —
  a compactor should read whole segments with a small number of big ranged GETs, since it
  needs everything. Cost ≈ `S / 8 MiB` GETs.
- Writes: **1 PUT** if the output is ≤5 GiB (the single-`PutObject` limit); otherwise a
  multipart upload costing `S / part_size` PUT-class requests plus initiate and complete,
  since **every `UploadPart` is separately billable**.
- Commit: 1 R + 2 W.

For S = 4 GiB: 512 GETs (~$0.0002) + **1 PUT** (~$0.000005) + commit.
**Well under a millidollar per compaction.**

> **⚠️ Corrected (C-3, see [`batching-and-visibility.md`](batching-and-visibility.md) §13).**
> An earlier version costed this output as 64 multipart parts. At 4 GiB it is under the 5 GiB
> single-PUT limit and is **1 request, not 64** — so sizing segments just below 5 GiB is
> meaningfully cheaper than chunking them. The conclusion is unchanged and strengthened:
> compaction is CPU-bound, not request-bound. Compaction cost is dominated by CPU (decode, re-encode,
re-cluster), not by blob requests. That is a useful conclusion: **we should compact more
aggressively than a local-disk LSM would**, and the constraint is our own compute budget.

## Rate limiting and fairness

Compaction competes with queries for CPU and NIC on the *same* nodes. Required:
- A global (per-node) compaction budget as a fraction of CPU, dynamically reduced under query
  load.
- Per-index fairness so one tenant's compaction backlog doesn't starve others.
- Blob-request budget accounting — compaction requests are attributed to the tenant.

Alternative: **dedicated compaction nodes**. Tempting, but it reintroduces node roles and
therefore placement/scheduling of roles. Since any node can do any work, the better answer is
a **role bias** in gossip: nodes may advertise `prefers_background_work`, and the assignment
function weights them. Same mechanism, no new concept.

## GC

Compaction dereferences input segments. They cannot be deleted immediately — a query holding
an older epoch may still read them.

- **Epoch retention window**: keep dereferenced objects for `max(query_timeout, snapshot_ttl)
  + safety` (default ~1 hour), then delete.
- Deletion targets are **derived from manifest diffs**, not from LIST: `objects(E_old) −
  objects(E_new)`.
- Capability-adaptive: on S3, DELETE is free, so reap eagerly in batches of 1,000. On
  GCS/Azure, deletes are billed, so batch and defer.
- A periodic **orphan sweep** (the sanctioned LIST) catches objects lost to crashed
  compactions. Weekly, offline, per prefix-group.

## Open questions raised

- OQ-29: The right level count and fan-out under our actual segment-count-vs-latency curve.
- OQ-30: Should vector re-clustering be decoupled from data compaction (different triggers,
  different cadence)? Probably yes — data compaction is cheap and frequent, re-clustering is
  expensive and rare.
- OQ-31: Compaction under a heavy-tailed index-size distribution — does a 50 TB index starve
  a million small ones? Needs a fairness simulation.

## Sources

- [Compaction — RocksDB wiki](https://github.com/facebook/rocksdb/wiki/Compaction)
- [LSM-based Storage Techniques: A Survey — arXiv](https://arxiv.org/pdf/1812.07527)
- [How to Grow an LSM-tree? Towards Bridging the Gap Between Theory and Practice — arXiv](https://arxiv.org/pdf/2504.17178)
- [Quickwit 101 — merge policy](https://quickwit.io/blog/quickwit-101)
- [SlateDB: An Object-Native LSM for Online Systems](https://slatedb.io/blog/introducing-slatedb/)
- [S3 Pricing (DELETE is free) — AWS](https://aws.amazon.com/s3/pricing/)
