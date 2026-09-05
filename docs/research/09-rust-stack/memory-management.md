# Memory Management: Avoiding OOM

**Answers:** Q35
**Status:** Complete (v1)
**Companion to:** [`../07-caching/disk-space-management.md`](../07-caching/disk-space-management.md)

## 1. Why memory is harder than disk

The disk document rests on Invariant I2: local disk is pure cache, so exhausting it degrades.
**Memory does not have that property.**

| | Disk | Memory |
|---|---|---|
| Contents | pure cache | cache **+ un-durable memtables + in-flight query state** |
| On exhaustion | serve from blob store | **process aborts** |
| Failure blast radius | slower node | **node dies, in-flight work lost, `batched` writes lost** |
| Recovery | immediate | restart + ~10 h cache refill (endurance-bound) |

And Rust removes the one escape hatch other runtimes have:

> **Rust allocation failure is not catchable.** `Vec` and friends call
> `alloc::alloc::handle_alloc_error` on failure, which **aborts**. There is no `catch_oom`. The
> fallible path (`try_reserve`, stable since 1.57 for `Vec`, `String`, `HashMap`, `HashSet`,
> `VecDeque`) must be used deliberately, everywhere it matters.

The evidence that this is worth doing: one production service went from **thousands of
`rust_oom` coredumps per day to 5–10 per day** by adopting `try_reserve` plus proactive
low-memory detection.

> **Invariant I3.** The allocator is never the limit. Every byte that scales with request
> size, data size, or concurrency is accounted **above** the allocator and refused there.
> Reaching `handle_alloc_error` is a bug, not a capacity signal.

## 2. Where the memory actually goes

| Source | Scales with | Bounded by |
|---|---|---|
| RAM cache (classes 1–5) | config | config ✅ |
| **Memtables** (freshness layer) | active tenants × `T` × R | per-tenant caps + **flush valve** |
| **In-flight query fetch bytes** | **fan-out × block size × concurrency** | ⚠️ **unbounded by default — the real OOM source** |
| Query state (top-k, bitmaps, merge) | k, oversample, rows | reservation |
| Compaction / index build | sample size, merge width | job admission |
| Write/bundle buffers | `W` × bundle size × in-flight | config |
| Per-open-index resident state | indexes touched | ⚠️ needs an LRU — 1M open × 4 KB = **4.1 GB** |
| RPC/connection buffers | connections | bounded conns |
| Tokio task futures | spawned tasks | box large futures |

Two of these deserve their own sections.

## 3. The dominant term: in-flight fetch bytes

Our design says fan-out is free because bandwidth is free and latency isn't. **That is true for
cost and latency and false for memory.**

A cold vector query probing `p`=32 posting lists of ~3 MB, on a node handling 4 shards:

| Strategy | Peak per query |
|---|---|
| Accumulate all fetched bytes | **384 MB** |
| Score-and-drop, 32 in flight | 96 MB |
| Score-and-drop, 16 in flight | 48 MB |
| Score-and-drop, **8 in flight** | **24 MB** |
| Score-and-drop, 4 in flight | 12 MB |

> **D-54.** A query's memory is **O(k + bytes resident in flight)**, never O(bytes scanned).
> Fetched blocks are decoded, scored into the top-k heap, and **dropped immediately**. Peak is
> the number of blocks simultaneously resident, not the number fetched.

This is a 16× reduction and, more importantly, it makes per-query memory **independent of
index size** — a 10 KB index and a 50 TB index cost the same peak. That property is what makes
50M indexes on shared nodes safe.

### The residency-vs-round-trips tension
Bounding resident bytes bounds fetch concurrency, which can add pipelined waves and cost
latency — in direct tension with the 3-round-trip budget
([`../08-query-engine/query-path.md`](../08-query-engine/query-path.md)).

Resolution, in order:
1. **Pipeline, don't serialize.** Issue the fetches concurrently and apply backpressure at the
   *decode* stage. If scoring keeps pace with arrival, residency stays far below fan-out width
   and the round trip is preserved.
2. **Shrink blocks before shrinking width.** Fetching 32 × 750 KB sub-ranges instead of
   8 × 3 MB holds the same round trip at a quarter of the residency, and costs only more GETs —
   which are $0.0004/1000. **Trading memory for blob requests is a far better trade than
   trading memory for latency.**
3. Only then reduce width, accepting extra waves.

> **Correction M-1.** [`request-efficiency-patterns.md`](../02-object-storage/request-efficiency-patterns.md)
> Pattern 5 ("when in doubt, fetch the superset") and `query-path.md` ("fan-out within a round
> is unlimited") need a memory bound. Fan-out is limited by an **in-flight byte reservation**,
> not by cost. Bandwidth is free; buffers are not.

## 4. Memory admission control, not concurrency limits

The standard mistake is capping *concurrent queries*. Our query footprints vary by ~1000×
(a small-index exact scan vs. a wide cold fan-out), so any single concurrency limit either
wastes the box or kills it.

Follow the DataFusion / Trino model: **reserve bytes from a pool.**

- A `MemoryPool` tracks reservations; a `MemoryReservation` is an accounted allocation freed on
  drop; a named `MemoryConsumer` owns it. `GreedyMemoryPool` is first-come-first-serve;
  `FairSpillPool` caps each spillable reservation at an even fraction of what's left.
- Trino kills queries exceeding `query_max_memory_per_node` — explicitly *"to ensure fairness
  and prevent deadlock caused by memory allocation."*
- DataFusion's documented limitation is instructive: it only tracks operators whose memory is
  proportional to input rows, **not the RecordBatches flowing between operators**. Untracked
  memory is where OOM hides. We must track the buffers too, since ours *are* the dominant term.

> **D-55.** Every query acquires a byte reservation from a global pool before executing, sized
> from its plan (fan-out width × block size + k × oversample + bitmap estimate). Exceeding the
> reservation degrades or fails **that query**; it never touches the process.

> **D-56.** Per-tenant memory quotas, alongside cache bytes and blob requests (Design rule 13).
> At 50M indexes on shared nodes, one tenant must not be able to price everyone else out of
> RAM.

### Sizing sanity check (128 GB node, 40 GB query pool)

| Scenario | Concurrency | Memory |
|---|---|---|
| Warm: 200 QPS × 10 ms | 2 | 0.05 GB |
| Cold: 200 QPS × 400 ms | 80 | 1.9 GB |
| Pool capacity at 24 MB/query | **1,667** | 40 GB |

Steady state uses ~5% of the pool. **The pool is not sized for the average; it is sized for the
pathological** — a burst of wide cold queries, a scan, a hostile request. That is the case that
kills nodes, and the reservation is what makes it a 429 instead.

## 5. Node budget (128 GB)

| Pool | Size | Notes |
|---|---|---|
| RAM cache (classes 1–5) | 24 GB | manifests, index sections, centroids, FSTs |
| Memtables (freshness) | 8 GB | vastly over-provisioned vs. the 1.5–18 MB/node steady state; sized for a bursting tenant |
| **Query execution pool** | **40 GB** | reservations (§4) |
| Background (compaction, index build) | 12 GB | job admission control |
| Write/bundle buffers | 4 GB | `W` in-flight bundles |
| RPC / connection buffers | 4 GB | bounded connections |
| **Emergency reserve** | **4 GB** | §6 — never allocatable except by the flush path |
| *Accounted* | *96 GB* | |
| Allocator overhead / fragmentation (~15%) | 14 GB | RSS ≠ live bytes |
| OS, page cache, headroom | 10 GB | small because the cache uses direct I/O (D-52) |
| **`memory.max`** | **120 GB** | **`memory.high` = 102 GB** |

## 6. The "need memory to free memory" deadlock

Under pressure, the release valve is to **flush memtables** (D-49: force the PUT rather than
spilling un-durable data to disk). But serializing and uploading a bundle *requires memory*. If
the pressure response starts when nothing is left, it cannot run.

> **D-57.** A hard **emergency reserve** is excluded from every other pool and allocatable only
> by the flush path. Without it, memory pressure is an unrecoverable deadlock rather than a
> recoverable event.

The same reasoning applies to the metrics and logging paths — a node that cannot report its own
distress is much harder to operate.

## 7. Seeing it coming: cgroups v2 and PSI

| Control | Behaviour |
|---|---|
| `memory.max` | Hard ceiling. Reclaim fails ⇒ **OOM kill**. |
| `memory.high` | Soft ceiling. Kernel throttles and forces direct reclaim — **the process stalls but does not die**. |
| `memory.events` | `high` counts throttle events; `max` counts limit hits. |
| PSI `/proc/pressure/memory` | Time spent stalled on memory. *"The best early indicator of a container approaching its limits — well before OOM kills."* Guidance: **full avg60 > 5% ⇒ under-provisioned or leaking.** |

> **D-58.** Always set `memory.high` strictly below `memory.max` (≈85%), so there is a throttled
> warning zone rather than a cliff. Treat crossing `memory.high` as a **load-shedding trigger**,
> not merely a metric.

Caveat worth knowing: `memory.high` throttling shows up as **p99 latency spikes with no OOM
kills at all** — a genuinely confusing signature. Our own accounting (§4) should trip *before*
the kernel's, so `memory.events.high` incrementing means our limits are set wrong.

## 8. Allocator choice and the fragmentation trap

**RSS is what the OOM killer sees, and RSS ≠ live bytes.** Fragmentation and un-returned dirty
pages inflate it.

> **D-59.** Use **jemalloc** with `background_thread:true`. Reason: jemalloc purges on **decay
> timers**, whereas *"mimalloc's reclamation is tied to allocation activity, making it a poor
> fit for workloads where worker threads go idle between bursts"* — which describes a node
> serving 50M mostly-idle indexes exactly. jemalloc's stats surface (`stats.allocated` vs
> `stats.resident`) is also what §9's monitoring needs.

Known trap: `dirty_decay_ms` does not always behave as documented. Reports show RSS exceeding
expectations even with `retain:true, dirty_decay_ms:10000`, with peak RSS only responding at
`dirty_decay_ms:0`. Tune empirically against RSS, never from the documentation alone (OQ-107).

## 9. Rules for the codebase

| Rule | Why |
|---|---|
| **Never `with_capacity(n)` where `n` comes from a request, header, or file field** | The classic single-allocation OOM. Validate against a limit, then `try_reserve`. |
| **`try_reserve` on every data-proportional allocation** | The only fallible path Rust offers |
| **Bounded channels and queues everywhere** | An unbounded channel is a latent outage (already a rule in `runtime-and-io.md`) |
| **Box large futures before `tokio::spawn`** | Each task's future is heap-allocated; large futures × many tasks is invisible growth |
| **Streaming decode, never decode-all** | Peak = resident, not total (D-54) |
| **Bound the open-index LRU** | 1M open indexes × 4 KB = 4.1 GB of pure bookkeeping |
| **Arena per query, dropped wholesale** | Avoids fragmentation from millions of small transient allocations |
| **No unpooled allocation in any data path** | If it isn't reserved, it isn't bounded |

## 10. The degradation ladder

Ordered least- to most-disruptive. Every step must be exercised in tests; **OOM is not on the
list**:

1. Shrink the RAM cache (classes 5→1 in reverse priority). Pure performance cost.
2. Force memtable flush (uses the emergency reserve).
3. Narrow fetch width / shrink blocks for in-flight queries (§3).
4. Reduce oversample factor — costs recall, reported in `meta`.
5. Return **partial results** with `partial: true` and the shard list.
6. Reject new queries with `429` + `Retry-After`.
7. Cancel the single largest-reservation query.
8. *(never)* abort.

> **D-60.** The ladder is driven by **our own accounting**, not by kernel signals. PSI and
> `memory.events.high` are alarms that our thresholds are wrong, not the primary trigger.

## 11. Correlated OOM: the 10,000-node failure mode

A single node dying is routine — it owns nothing, and placement routes around it. **The real
risk is correlation:** one expensive query shape, spread by the load balancer, drives every node
to the same pressure simultaneously. That is a fleet outage.

Mitigations:
- Memory admission (§4) converts the event into 429s, which are survivable and visible.
- **Per-node jitter on thresholds** so nodes shed at slightly different points, turning a cliff
  into a ramp.
- Circuit-breaking per query *shape* (fan-out width, oversample, filter selectivity), not just
  per tenant.
- Shed load before pressure, not after: admission control that considers the *estimated* cost
  of a plan, which we already compute for free (`query-path.md` — planning inputs are cached).

## 12. Testing

- **Memory-constrained CI job**: full query suite under a tight `memory.max`; assert 429s and
  degradation, and **zero aborts**.
- Fault injection: reservation failures at every acquisition point.
- Adversarial requests: huge `top_k`, huge oversample, enormous filter expressions, maximum
  fan-out, pathological vector dimensions.
- Long-running RSS-vs-`stats.allocated` soak to catch fragmentation drift and leaks (no GC
  means a leak is permanent).
- Deterministic simulation: 10K logical nodes given the same expensive query shape at once
  (§11).

## 13. Open questions raised

- **OQ-104 (Tier 2)** — Real per-query memory profile by plan type. §5's 24 MB is derived, not
  measured; it sets the pool size and the admission model.
- OQ-105 — Can plan-time memory estimates be accurate enough for admission, or do we need
  mid-flight re-reservation with the ability to downgrade a running query?
- OQ-106 — Right split between the query pool and the RAM cache. Both convert memory into
  latency, at different exchange rates; there is a computable optimum.
- **OQ-107** — jemalloc tuning (`dirty_decay_ms`, `muzzy_decay_ms`, `retain`) against measured
  RSS, given the documented gap between the settings and observed behaviour.
- OQ-108 — Should query intermediates ever spill to local disk? Our intermediates are small
  (top-k) and the bulk is re-fetchable from the blob store, so *"narrow the fetch width"* is
  probably strictly better than spilling — and it avoids competing with the cache for endurance.
  Confirm.
- OQ-109 — Per-open-index resident state size; drives the LRU bound.
- OQ-110 — Does `memory.high` throttling ever hurt us more than shedding would? If our
  accounting is right we should never reach it.

## Sources

- [How to Deal with Out-of-memory Conditions in Rust — CrowdStrike](https://www.crowdstrike.com/en-us/blog/dealing-with-out-of-memory-conditions-in-rust/)
- [RFC 2116: alloc-me-maybe (fallible allocation) — Rust RFC Book](https://rust-lang.github.io/rfcs/2116-alloc-me-maybe.html)
- [Feedback from adoption of fallible allocations — Rust Internals](https://internals.rust-lang.org/t/feedback-from-adoption-of-fallible-allocations/14502)
- [MemoryPool — DataFusion docs.rs](https://docs.rs/datafusion/latest/datafusion/execution/memory_pool/index.html)
- [FairSpillPool — DataFusion docs.rs](https://docs.rs/datafusion/latest/datafusion/execution/memory_pool/struct.FairSpillPool.html)
- [Spill to disk — Trino documentation](https://trino.io/docs/current/admin/spill.html)
- [Control Group v2 — kernel.org](https://www.kernel.org/doc/Documentation/cgroup-v2.txt)
- [Kubernetes p99 Spikes Without OOM: Diagnosing cgroup v2 memory.high with PSI — Michal Drozd](https://www.michal-drozd.com/en/blog/cgroup-v2-memory-high-psi-kubernetes/)
- [Linux Cgroups V2 Memory Throttling & OOM Fix — Netdata](https://www.netdata.cloud/academy/diagnosing-linux-cgroups/)
- [Your Rust Service Isn't Leaking — It Could Be the Allocator — pranitha.dev](https://pranitha.dev/posts/rust-and-memory-allocators/)
- [Rust allocator jemalloc vs mimalloc vs tcmalloc — Kunal Ganglani](https://www.kunalganglani.com/blog/rust-allocator-jemalloc-mimalloc-tcmalloc)
- [jemalloc TUNING.md — docs.rs](https://docs.rs/crate/jemalloc-sys/latest/source/jemalloc/TUNING.md)
- [jemalloc #2688: dirty_decay_ms doesn't take effect](https://github.com/jemalloc/jemalloc/issues/2688)
- [On the Impact of Memory Allocation on High-Performance Query Processing — arXiv](https://arxiv.org/pdf/1905.01135)
- Arithmetic reproducible in `09-rust-stack/memory-budget.py`.
