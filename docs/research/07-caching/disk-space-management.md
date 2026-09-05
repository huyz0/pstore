# Disk Space Management: Keeping a Full Disk From Becoming an Outage

**Answers:** Q34
**Status:** Complete (v1)

## 1. The property we must protect

In `pstore`, local disk is **pure cache**. Nothing durable lives on it. Therefore:

> **Invariant I2.** Running out of local disk is a **performance** event, never an
> availability or correctness event. Every disk write path degrades to "serve from the blob
> store" rather than failing.

This is a genuine architectural advantage over local-disk engines — and it is easy to throw
away. Four common mistakes turn a full disk into an outage:

| Mistake | Consequence |
|---|---|
| Cache volume shared with the OS | Logs, metrics, journald, and temp all fail; the node dies without saying why |
| `ENOSPC` propagated as an error instead of a cache miss | Queries fail on a *cache* problem |
| Compaction that downloads inputs to disk | Inherits RocksDB's space-reservation problem for no reason |
| Spilling not-yet-durable buffers to the cache disk | Puts durable state on a device we promised was disposable |

The rest of this document is about not making those, plus the sizing question — where the
arithmetic produced a genuinely surprising answer.

## 2. The surprising finding: endurance binds before capacity

Flash caches have a **device-level write amplification** (DLWA) that rises sharply with
utilization, because mixing data of different lifetimes in the same flash blocks drives
garbage collection:

| Device utilization | DLWA |
|---|---|
| 50% | **1.3** |
| 100% | **3.5** |
| With lifetime segregation (FDP) | ~1.03 at any utilization |

And *"a DLWA of 2 causes the SSD to fail twice as fast compared to a DLWA of 1."*

Meta runs CacheLib in production with **50% host-level over-provisioning** — using only half
the device — specifically to hold DLWA near 1.3.

> **Finding K-1.** Filling the last third of a flash cache costs ~2.7× the device write
> amplification, and therefore ~2.7× the drive wear, in exchange for a few percent of hit
> rate. **Cache capacity is not device capacity.** The headroom is over-provisioning, not
> waste.

### The write budget

AWS does not publish endurance (DWPD/TBW) for instance-store NVMe — that is a real gap
(OQ-98). Planning at 2 DWPD and DLWA 1.3:

| DWPD | DLWA | Host writes/day | Sustained cache fill |
|---|---|---|---|
| 1 | 1.3 | 2.88 TB | **33 MB/s** |
| **2** | **1.3** | **5.77 TB** | **67 MB/s** |
| 2 | 3.5 (full device) | 2.14 TB | 25 MB/s |
| 3 | 1.3 | 8.65 TB | 100 MB/s |

**~67 MB/s of sustained cache admission per node** — perhaps 1–2% of what the NIC could
deliver. Nobody plans for this and it silently destroys drives around month 8.

> **Finding K-2.** Cache admission must be rate-limited by **device endurance**, not by
> network or capacity. A token bucket sized from the drive's write budget, not a "fill as
> fast as you can" loop.

### What that implies operationally

A 2.4 TB cache at 67 MB/s takes **~10 hours to fill from empty**. Three consequences, one of
which resolves an earlier open question:

1. **Cache persistence across restarts is not an optimization, it is mandatory.** A rolling
   deploy that flushes caches costs 10 hours of degraded hit rate per node. This upgrades
   D-23 from "nice" to "required".
2. **Bulk shadow-warming is impossible within budget** — and unnecessary. Classes 1–4
   (manifests, index sections, centroids, term dictionaries) are ~0.1–1% of bytes: **2.4–24 GB,
   or 36 seconds to 6 minutes** at budget. That is the whole warming story.
3. **→ Closes [OQ-57](../00-plan/open-questions.md)** ("is shadow warming worth its cost?"):
   **yes for metadata, never for bulk.** Warm the 1% that unblocks every query; let the other
   99% arrive on demand.

> **D-44.** Shadow warming and cold-start prefetch cover cache classes 1–4 only. Bulk vector
> and posting blocks are demand-filled, inside the endurance token bucket.

## 3. Capacity layout

For a 3.75 TB instance-store NVMe:

| Region | Size | Purpose |
|---|---|---|
| **Hard reserve** | 100 GB (2.7%) | OS, logs, metrics, scratch. **Never allocatable by the cache.** |
| **Cache maximum** | 2.40 TB (64%) | Everything the cache may use |
| **Unallocated** | 1.25 TB (33%) | Over-provisioning for DLWA control (Finding K-1) |

For scale: 2.4 TB holds the 1-bit scan tier for **~25 billion 768-dim vectors** on one node.
Capacity is rarely the problem; endurance and admission quality are.

> **D-45.** The cache gets its **own filesystem or raw device**, never a directory on the root
> volume. A full cache must be incapable of stopping the process from logging.

> **D-46.** The cache is a **small number of large files** (foyer's region/block model), never
> one file per cached block — otherwise inode exhaustion and directory-scan cost become their
> own failure modes.

## 4. Watermarks and progressive degradation

Adapting the Elasticsearch low → high → flood-stage pattern, whose key property is that
**reads keep working at every stage**:

| Utilization of *cache max* | State | Behaviour |
|---|---|---|
| < 70% | Normal | Admit all classes |
| 70–80% | Low | Stop admitting classes 6–9 (bulk vectors, postings, documents); background eviction rate up |
| 80–90% | High | Admit classes 1–4 only (metadata, centroids); aggressive eviction |
| > 90% | **Bypass** | **Zero disk writes.** All reads served from RAM cache or the blob store. Alert. |
| Hard reserve | — | Untouchable, always |

Bypass mode is the crucial one: the system gets *slower and more expensive* (more blob GETs),
and **stays correct and available**. That is exactly the degradation Invariant I2 promises.

> **D-47.** `ENOSPC` on any cache write is caught, counted, and treated as a cache miss. A
> test that runs the full query suite against a deliberately-full cache device is a CI gate —
> this is precisely the path that is never exercised by accident.

## 5. Four pstore-specific rules

### 5.1 Compaction needs zero local disk
RocksDB must call `EnoughRoomForCompaction()` and reserve `compaction_buffer_size` before
starting, because its inputs and outputs live on the same disk. **We have no such
constraint**: compaction reads ranged GETs, merges in bounded memory, and streams to a
multipart upload. Peak local footprint is a few buffers.

> **D-48.** Compaction, indexing, and GC are **streaming with bounded memory and zero disk
> scratch**. "Download inputs to `/tmp`, merge, upload" is a blocking review defect — it
> imports a disk-full outage mode we do not otherwise have.

Genuine exceptions (k-means over a large sample, sorting a big segment) get an explicitly
**reserved, accounted scratch budget**, and must degrade — sample less, merge in more passes —
rather than fail.

### 5.2 Never spill un-durable data to the cache disk
Under memory pressure a memtable is tempting to spill. But in `batched` mode that data is not
yet in the blob store, so spilling puts durable-ish state on a disposable device and breaks
Invariant I2.

> **D-49.** Under memory pressure, **force a flush (issue the PUT)** rather than spilling
> un-flushed data to disk. Spilling data that is *already* durable — a read-through cache of
> WAL bundles — is fine and is just cache.

### 5.3 Scans do not populate the cache
Compaction reads, bulk export, backfill, and one-off analytical scans would otherwise evict
the entire working set — the classic problem RocksDB solves with `fill_cache=false` and
Postgres with ring buffers.

> **D-50.** Sequential/scan access is **cache-bypassing by default**. Only access with a
> plausible re-read (query-path fetches) populates the cache. This single rule prevents most
> cache thrash and a large share of wasted device writes.

### 5.4 Segregate cache classes physically, not just logically
The DLWA research is explicit: mixing sequential/cold with random/hot data in the same flash
blocks is what drives garbage collection; segregating by lifetime takes DLWA from 1.3–3.5 to
~1.03.

Our cache classes (`cache-hierarchy.md`) already separate small hot metadata from large cold
vector blocks. That is a **lifetime** distinction, so it should be a **physical** one.

> **D-51.** Map cache classes onto separate foyer regions/devices so hot-small and cold-large
> data never share flash blocks. This is a free ~1.3× endurance win from a partitioning we
> were going to do anyway.

## 6. Multi-tenant fairness at 50M indexes

One tenant scanning a large index can evict everyone else's working set. Three layers:

1. **Class quotas** first — classes 1–4 are pinned, so no amount of bulk traffic can evict the
   metadata that every query needs.
2. **Per-tenant cache-byte accounting**, with weighted-fair eviction: a tenant over its share
   is evicted first. Ties into Design rule 13 (meter every tenant-consumable resource).
3. **Scan bypass** (D-50) removes the largest single source of pollution.

Note that D-43 (co-locating small tenants' indexes) helps here: a small tenant's ~50 indexes
share one node's cache, so their combined working set is one coherent, small unit rather than
50 fragments competing across 50 nodes.

## 7. Implementation: what foyer gives us

`foyer` covers most of this directly:

| Need | foyer |
|---|---|
| Append-only, region-based disk layout | ✅ block-based engine, configurable block size (16 MB default) |
| Admission control | ✅ pluggable admission + reinsertion filters |
| Eviction | ✅ LRU (with high-priority pool ratio), LFU, FIFO, **S3-FIFO** |
| **Endurance rate limiting** | ✅ **device throttling: IOPS and throughput limits, per direction** — this is D-45's enforcement point |
| Restart recovery | ✅ configurable recovery modes, concurrent recovery workers |
| Reserved space / capacity | ✅ configurable |

The "high-priority pool ratio" in its LRU maps onto our pinned classes, and the IO throttler
is exactly the token bucket K-2 requires. **This closes [OQ-54](../00-plan/open-questions.md)
favourably** — foyer supports quotas, priority, and throttling. What remains ours to build:
class→region mapping (D-51), per-tenant accounting, and the watermark state machine.

On policy: **S3-FIFO for eviction, TinyLFU-style admission** is the current best pairing —
`TinyUFO` (Cloudflare) combines exactly these, and W-TinyLFU wins hit rate on skewed
workloads while S3-FIFO wins throughput by cheaply discarding one-hit wonders. Our workload is
extremely skewed (a few hot tenants, 50M cold indexes), so the frequency filter matters.

> **D-52.** Use **direct I/O** for the disk cache. Buffered I/O double-caches every block in
> the kernel page cache, halving effective RAM for zero benefit.

## 8. The other kind of space: the blob store

S3 cannot fill up, so this is a **cost and hygiene** problem, not an overload one. But it is
unbounded if unmanaged. Sources of growth:

| Source | Control |
|---|---|
| LSM space amplification | Compaction policy (`compaction.md`); ~1.1–2× is fine at $0.023/GB |
| Orphans from lost CAS races and crashed compactions | Manifest-diff GC + weekly orphan sweep (one of the three sanctioned LISTs) |
| Epoch retention | Retention window ≫ max query duration; policy beyond that is a paid feature |
| Un-folded WAL bundles | TTL + forced fold on approach to expiry |
| Delete vectors | Folded away at ~20% garbage |
| **Branch retention** | A branch pins its parent's objects. **Attribute those bytes to the branch owner** or it becomes an invisible cost leak |

> **D-53.** Every durable byte is attributable to a tenant, including bytes retained only by a
> branch or an un-expired epoch. Un-attributed storage growth is how this class of system
> quietly loses money.

## 9. Metrics that must exist

- Cache bytes **by class and by tenant**; utilization vs. each watermark.
- **Admission bytes/s vs. the endurance budget** — the leading indicator of drive death.
- Observed write amplification: device bytes written ÷ bytes admitted (SMART vs. our counter).
- Hit rate **per class** (a 99% overall rate can hide a 40% centroid miss rate).
- `ENOSPC` count, bypass-mode duration, eviction rate, scan-bypass bytes avoided.
- Device wear indicators (SMART `percentage_used`), alerting on projected end-of-life.

## 10. Open questions raised

- **OQ-98 (Tier 2)** — Actual endurance of AWS/GCP/Azure instance-store NVMe. AWS does not
  publish DWPD/TBW. Without it the write budget is a guess. Measure `percentage_used` drift on
  a real fleet over weeks, or get the number from the vendor.
- OQ-99 — Optimal cache-max fraction. 64% is derived from the CacheLib/DLWA data; the true
  optimum depends on our access-size distribution and should be measured on the hit-rate vs.
  wear curve.
- OQ-100 — Does class→region segregation (D-51) actually reach the ~1.03 DLWA the FDP paper
  reports, without FDP hardware? Probably partially; measure.
- OQ-101 — S3-FIFO vs. W-TinyLFU vs. our class-aware policy on real traces (refines OQ-56).
- OQ-102 — Should very large tenants get a *dedicated* cache partition rather than a quota?
- OQ-103 — Behaviour when the cache device fails outright mid-flight (not full, but gone).
  Should be identical to bypass mode; verify it is, and that the node does not simply die.

## Sources

- [Towards Efficient Flash Caches with Emerging NVMe Flexible Data Placement SSDs — arXiv / EuroSys](https://arxiv.org/html/2503.11665)
- [CacheSack: Theory and Experience of Google's Admission Optimization for Datacenter Flash Caches — ACM](https://dl.acm.org/doi/fullHtml/10.1145/3582014)
- [TinyLFU: A Highly Efficient Cache Admission Policy — arXiv](https://arxiv.org/html/1512.00727v2)
- [TinyUFO (S3-FIFO eviction + TinyLFU admission) — lib.rs](https://lib.rs/crates/tinyufo)
- [foyer-rs/foyer — GitHub](https://github.com/foyer-rs/foyer)
- [foyer — Hybrid cache for Rust](https://foyer.rs/docs/overview)
- [Managing Disk Space Utilization — RocksDB wiki](https://github.com/facebook/rocksdb/wiki/Managing-Disk-Space-Utilization)
- [SST File Manager — RocksDB wiki](https://github.com/facebook/rocksdb/wiki/SST-File-Manager)
- [Optimizing Space Amplification in RocksDB — CIDR 2017](https://www.cidrdb.org/cidr2017/papers/p82-dong-cidr17.pdf)
- [Elasticsearch Disk Watermark: Causes, Fixes & Best Practices — groundcover](https://www.groundcover.com/learn/logging/elasticsearch-disk-watermark)
- [SSD instance store volumes for EC2 instances — AWS docs (TRIM)](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ssd-instance-store.html)
- [Amazon EC2 I7i instances — AWS](https://aws.amazon.com/ec2/instance-types/i7i/)
- Arithmetic reproducible in `07-caching/disk-budget.py`.
