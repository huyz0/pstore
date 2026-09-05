# The 1M × 50 Tenancy Model, and the Three Things It Forces

**Answers:** Q33 — *What does 1M tenants × up to 50 indexes, 10% active per second, do to the
design?*  Also **closes [OQ-84](../00-plan/open-questions.md)**, which asked for exactly these
numbers.
**Status:** Complete (v1)

## 1. The parameters

| Parameter | Value |
|---|---|
| Tenants | **1,000,000** |
| Indexes per tenant | up to **50** ⇒ **~50,000,000 indexes** |
| Tenants with indexing activity in any 1 s window | **10%** ⇒ 100,000 active tenants/s |
| Active **indexes** per second (`A`) | 100k – 5M, depending on how many of a tenant's 50 a burst touches |
| Idle at any instant | **90% of tenants, and ≳98% of indexes** |

> **Reading of "10% in 1s".** I have taken this as *activity*: one tenant in ten has write /
> indexing traffic in a given second. If it instead means *freshness SLA* — one tenant in ten
> needs sub-second visibility — that is a second, compatible lever, handled in §7. Both
> readings point the same way, so nothing below depends on resolving it.

Constants used: 2,592,000 s/month; S3 PUT $5×10⁻⁶; bundle target `B` = 8 MiB.

## 2. What the naive design costs (per-index flushing)

`PUTs/month = A × 2,592,000` at a 1 s flush:

| Active indexes/s `A` | $/month in PUTs alone |
|---|---|
| 100,000 | **$1,296,000** |
| 300,000 | **$3,888,000** |
| 1,000,000 | **$12,960,000** |
| 5,000,000 | **$64,800,000** |

At 1M tenants paying, say, $20/month, gross revenue is ~$20M/month. **The write path alone
would consume 6%–320% of revenue before storage, compute, or reads.** This confirms
[Finding B-1](../05-storage-engine/batching-and-visibility.md) at your actual scale rather
than at a hypothetical one.

## 3. Change 1 — Write cohorts: fan writes *in*, not out

With cross-index bundling the floor becomes `W / T`, where `W` is the number of nodes
*simultaneously buffering writes*. My earlier assumption was `W = N` (all 10,000 nodes),
because write placement followed read placement.

**Your 90%-idle figure says that is wrong.** If only 100k tenants are active per second, there
is no reason to smear their writes across the whole fleet — doing so maximizes the floor.

| `W` (writer nodes) | `T` = 1 s | `T` = 5 s |
|---|---|---|
| 10,000 (write-follows-read) | $129,600 | $25,920 |
| 1,000 | $12,960 | $2,592 |
| 600 | $7,776 | **$1,555** |
| 256 | $3,318 | **$664** |

Against the data-driven term, which is irreducible:

| Fleet write volume | Data-driven $/month |
|---|---|
| 0.1 GB/s | $154 |
| 1 GB/s | $1,545 |
| 10 GB/s | $15,450 |

**Size `W` so the two terms balance.** Setting `W × 2,592,000 / T = bytes_per_s × 2,592,000 / B`
gives:

```
W* = write_bytes_per_second × T / B
```

| Fleet writes | `T`=1 s | `T`=5 s |
|---|---|---|
| 0.1 GB/s | 12 | 60 |
| 1 GB/s | 119 | **596** |
| 10 GB/s | 1,192 | 5,960 |

At 1 GB/s and `T` = 5 s: `W ≈ 600`, and total WAL PUT cost ≈ **$3,100/month** — versus
$3.9M for per-index flushing. Below `W*`, bundles are already full and you gain nothing;
above it, you are paying the time-driven floor for half-empty objects.

`W` is **self-tuning**: each node measures fleet write throughput from gossip and derives `W`;
disagreement merely shifts a few tenants. Clamp `W ≥ 32` so the write path always has
redundancy and parallelism.

### Discovery still works
The cohort node for a tenant is `LRH(tenant_id)` over a **cohort ring of size `W`** — a subset
of the full ring, derived, not discovered. A reader or recovering node computes it from
`tenant_id`. **Still zero LIST**, still no ownership: if the cohort node is unreachable, any
node writes to its own lane and registers in the per-shard lane bitmap
([`write-path-and-wal.md`](../05-storage-engine/write-path-and-wal.md) §3).

### This does not hurt visibility
Concentrating writes for *cost* and replicating them for *visibility* are separate fan-outs of
the same in-memory record:

```
client write
   ├─▶ cohort node for hash(tenant_id)    → buffers, bundles, issues the ONE PUT   (cost)
   └─▶ R read placements of each index    → in-memory memtable                     (visibility ~1 ms)
```

Both are intra-AZ in-memory sends. The PUT path and the freshness path are now independently
tunable, which is the whole point.

> **D-41.** Writes fan **in** to a derivable cohort of `W = write_bytes/s × T / B` nodes
> (clamped ≥32), not out to every read placement. Visibility is served by a separate in-memory
> fan-out to the read placements.

## 4. Change 2 — The tenant is the CAS unit; the index is the API unit

After bundling, the WAL is cheap and **the fold rate becomes the dominant per-index cost** —
each structural commit is a segment PUT + manifest PUT + HEAD CAS, and HEAD was per-index.

Hourly folds:

| CAS unit | PUTs per fold | $/month |
|---|---|---|
| Per **index** (50M) | 3 | **$540,000** |
| Per **index** (50M), inlined | 1 | **$180,000** |
| Per **tenant** (1M) | 3 | **$10,800** |
| Per **tenant** (1M), inlined | 1 | **$3,600** |

**A 50× reduction, exactly matching indexes-per-tenant.** The 50-indexes-per-tenant structure
is not an inconvenience; it is the thing that makes this work, because a tenant's 50 indexes
are written by one application, buffered on one cohort node, and can be committed together.

```
{h}/tnt/{tenant_id}/HEAD        ← CAS'd. Lists all 50 indexes: epoch, manifest_ref
                                   or (for small indexes) their contents INLINE.
{h}/tnt/{tenant_id}/idx/{index_id}/HEAD   ← only for promoted hot indexes
```

- One fold commits **all** of a tenant's dirty indexes in one CAS. Natural, since one node
  holds them all.
- A tenant whose 50 indexes are all small is **one object**. Creating, deleting, backing up,
  branching, or migrating that tenant is a single-object operation.
- **Adaptive promotion:** an index above a commit-rate threshold gets its own HEAD, referenced
  by pointer from the tenant HEAD. Two levels, so a hot index cannot contend with its 49
  quiet siblings on one register. The tenant HEAD then changes only on promotion or schema
  change.

Contention check: a tenant folding hourly is ~0.0003 CAS/s against a ~5 CAS/s ceiling — five
orders of magnitude of headroom. Even a tenant folding every second is fine.

> **D-42.** The **tenant** is the unit of physical grouping, commit, and CAS. The **index**
> remains the unit of API, schema, query, and isolation. These were conflated; separating them
> is worth 50× on commit cost and makes tenant lifecycle operations atomic.

This is a genuine revision to [`key-layout.md`](../11-design/key-layout.md) and
[`manifest-and-cas.md`](../03-metadata-consistency/manifest-and-cas.md), both of which assumed
one CAS register per index.

## 5. Change 3 — Co-locate small tenants' reads

Read placement was `LRH(index_id)`, which scatters a tenant's 50 indexes across 50 different
node sets. For a small tenant that means **50 independent cold starts** — 50 × ~400 ms of
first-query latency spread over their workload, and 50 nodes each caching a few hundred KB.

Since ~98% of indexes here are small and idle, that is the common case, not the tail.

> **D-43.** Read placement keys on **`tenant_id`** below a size threshold and on
> **`index_id`** above it. A small tenant's 50 indexes then share one node set: one cold start
> warms all 50, and their metadata shares cache lines. Large indexes keep independent
> placement so a single big tenant still spreads across the fleet.

The threshold is the same one that governs inlining. Placement changes as an index crosses it,
which is fine — placement is a hint, and crossing is rare.

## 6. Where the money actually goes, after all three changes

At 1 GB/s fleet writes, `W` ≈ 600, `T` = 5 s, hourly tenant folds:

| Component | $/month |
|---|---|
| WAL bundles (time floor + data term) | ~$3,100 |
| Tenant folds / structural commits | ~$10,800 |
| **Write path total, 50M indexes** | **~$14,000** |
| *Naive per-index flushing (A = 300k)* | *~$3,900,000* |

**~280×.** Storage for 1M tenant HEADs at ~10 KB is 10 GB ≈ $0.23/month. Memtable memory at
`T` = 5 s, R = 3, 1 GB/s is 15 GB fleet-wide — **1.5 MB per node**. None of the secondary
resources bind.

The conclusion from [`cost-model.md`](cost-model.md) survives and strengthens: once the write
path is structured correctly it is a rounding error, and **this remains a compute-efficiency
business.**

## 7. If "10% in 1s" meant the freshness SLA instead

Then it is a second lever, and a cheap one: only the 10% that need sub-second visibility pay
for R-way memtable replication and short `T`. The other 90% get a **relaxed class** — no
replication fan-out (visibility from the cohort node plus the WAL, seconds of staleness), and
a longer `T`. That cuts memtable memory and intra-AZ network by ~10× and lowers the WAL floor
further.

This slots straight into the existing per-request consistency modes
([`consistency-model.md`](../03-metadata-consistency/consistency-model.md)) as a per-index
*default*, so it needs no new mechanism — just a policy field in the tenant HEAD.

## 8. What else 50M indexes touches (checked, all fine)

| Subsystem | At 50M indexes |
|---|---|
| **Catalog** | Make it tenant-scoped: 1M records × ~10 KB = 10 GB. At 16,384 fixed-width buckets that is 640 KB each — one parallel round, ~$0.007. Scales. ✅ |
| **Key entropy** | `{hash4}` gives ~1M prefixes. Hash on `tenant_id` ⇒ ~1 tenant per prefix. Consider 5 chars for headroom (OQ-92). ✅ |
| **Placement compute** | O(log N + C) per request regardless of index count. ✅ |
| **Gossip** | Carries node state, not index state. Unaffected by 50M. ✅ |
| **Object count** | ~1M tenant HEADs + segments for active indexes. Buckets have no object limit. ✅ |
| **Memtables** | 1.5–18 MB/node. ✅ |
| **Idle cost** | An idle tenant is one object at rest. ~$0.0000002/month. ✅ |

## 9. Hazards introduced by these changes

| Hazard | Mitigation |
|---|---|
| Write cohort concentrates blast radius: a cohort node's failure affects all its tenants' un-flushed writes | Un-flushed data is already R-way replicated at read placements; the cohort node is a *flusher*, not the only holder. Clamp `W ≥ 32`. |
| `W` oscillation as measured throughput moves | Hysteresis + slow adjustment; a wrong `W` costs money, never correctness. |
| Tenant HEAD becomes a contention point for a hot tenant | Adaptive promotion of hot indexes to their own HEAD (§4). |
| Tenant-grouped placement concentrates a tenant that later grows | Threshold-based promotion back to per-index placement. |
| Cross-tenant bundles still mix tenants in one object | Unchanged from [OQ-87](../00-plan/open-questions.md) — per-byte-range encryption; **validate with customers**. Note that per-*tenant* grouping means one bundle now holds fewer, larger tenant slices, which makes range-level isolation cleaner. |
| Tenant deletion must reap 50 indexes' objects | Tenant HEAD lists them all; deletion is one CAS + a derived object set. Easier than before, not harder. |

## 10. Open questions raised

- **OQ-92** — How many of a tenant's 50 indexes does a typical write burst touch? This sets
  `A` (100k vs 5M) and therefore how much §3 is worth. **The single most valuable remaining
  number.**
- **OQ-93** — Fleet write throughput in bytes/s. Sets `W*` directly.
- **OQ-94** — Size threshold for tenant-grouped vs index-grouped placement, and for inlining.
  Probably the same number; verify.
- **OQ-95** — Does `W`-node write concentration interact badly with S3 per-prefix rate limits?
  600 nodes × 0.2 PUT/s is trivial, but verify the read-side load on cohort lanes during
  recovery.
- **OQ-96** — Prefix entropy width at 1M tenants (4 vs 5 chars) — refines OQ-78.
- **OQ-97** — Does adaptive promotion of hot indexes out of the tenant HEAD need to be
  reversible, and what is the demotion hysteresis?

## Sources

- [S3 Pricing — AWS](https://aws.amazon.com/s3/pricing/)
- [Write Path — WarpStream docs](https://docs.warpstream.com/warpstream/overview/architecture/write-path)
- [Minimizing S3 API Costs with Distributed mmap — WarpStream](https://www.warpstream.com/blog/minimizing-s3-api-costs-with-distributed-mmap)
- [Reimagining the vector database — Pinecone (memtable / freshness layer)](https://www.pinecone.io/blog/serverless-architecture/)
- [Best practices design patterns: optimizing Amazon S3 performance — AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html)
- Model and arithmetic: reproducible in `10-benchmarks-cost/model.py` (committed alongside).
