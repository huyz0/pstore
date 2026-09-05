# CPU Management: Avoiding Overload

**Answers:** Q36
**Status:** Complete (v1)
**Third in the resource series:** [disk](../07-caching/disk-space-management.md) ·
[memory](memory-management.md) · **cpu**

## 1. The headline result: QPS is not a unit of capacity

Warm query CPU is dominated by SIMD scanning of quantized codes, which is
**memory-bandwidth-bound**, not instruction-bound. So cost scales with **bytes scanned**, and
"queries per second" varies by ~75× depending on how many vectors a query touches:

| Vectors scanned/query | Bytes @96 B | Scan time (16 cores, ~60 GB/s) | Scan-bound QPS | ~QPS at 4× overhead |
|---|---|---|---|---|
| 128,000 (32 lists × 4k) | 12.3 MB | 0.20 ms | 4,883 | **~1,221** |
| 1,000,000 (1% of 100M) | 96 MB | 1.60 ms | 625 | **~156** |
| 10,000,000 (1% of 1B) | 960 MB | 16.0 ms | 62 | **~16** |

> **Finding C-1.** The unit of capacity is **vectors-scanned-per-second**, not
> queries-per-second. A "QPS per node" figure is meaningless without stating the scan size.
> This refines [OQ-75](../00-plan/open-questions.md): the cost model's 200 QPS/node is only
> valid at ~1M vectors scanned per query, and is off by an order of magnitude in either
> direction for other workloads.

Per core the scan is **memory-bound by ~7×** (750 M vectors/s of compute against 104 M/s of
bandwidth), which is why 1-bit quantization moves this number 32× and a faster kernel does not.
See [`hot-loop-performance.md`](hot-loop-performance.md) §1.

**Corollary:** every capacity, pricing, and admission decision should be denominated in bytes
scanned. That is also the number the query planner already knows before executing (`p` ×
posting-list size), so it is available for admission control at zero cost.

### The tier matters more than the core count

| Source of the scanned bytes | Effective bandwidth | 96 MB scan |
|---|---|---|
| RAM | ~10 GB/s per core, ~60 GB/s per node | **1.6 ms** |
| NVMe cache | ~6 GB/s per node | **16 ms** |
| Blob store | ~1 GB/s per node practical | ~100 ms + RTT |

**A scan served from NVMe is ~10× slower than the same scan from RAM.** So the RAM/NVMe split
in [`cache-hierarchy.md`](../07-caching/cache-hierarchy.md) is not only a hit-rate decision, it
is a *throughput* decision: promoting the hottest quantized blocks into RAM buys 10× scan
throughput on exactly the queries that matter most.

> **D-61.** The RAM cache holds not just metadata (classes 1–5) but the **hottest slice of the
> quantized scan tier (class 6)**, sized to the highest-QPS tenants. This is the cheapest
> available 10× on warm throughput.

## 2. The two workloads that fight

| | Foreground | Background |
|---|---|---|
| Work | query scan, decode, rerank, merge | compaction, index build, re-clustering, cache fill, GC |
| Character | latency-critical, bursty, small | throughput-oriented, long, large |
| If starved | user-visible p99 | backlog grows, then read amplification grows, *then* p99 |

Background work cannot simply be deprioritized to zero: starving compaction raises the segment
count, which raises query fan-out, which raises foreground cost. **The feedback loop is
positive**, so a naive "background at nice 19" policy is a slow-motion outage.

> **D-62.** Background work gets a **guaranteed floor** (e.g. ≥10% of cores) and an
> **elastic ceiling** that shrinks under foreground load. Never zero, never unbounded.

Recall from [`ownership-and-leases.md`](../04-cluster/ownership-and-leases.md) that any node can
do any work, so a node under query pressure can simply decline to claim background items and
let a quieter placement take them. **Load balancing for background work is free**; only the
floor needs protecting locally.

## 3. Scheduling architecture

Confirmed from [`runtime-and-io.md`](runtime-and-io.md): Tokio (work-stealing) for network and
orchestration, a **separate pool for CPU-heavy scan work**. Refinements:

1. **Never run a SIMD scan on a Tokio worker.** A 16 ms scan on an async worker delays every
   in-flight fetch on that thread. Already a rule; it is the single most important one.
2. **No preemption exists.** Rust has no way to interrupt a running scan. Long scans must be
   **chunked** (e.g. per 4 MiB block) with a deadline check and cancellation point between
   chunks. A query past its deadline must stop *computing*, not just stop being awaited —
   otherwise cancelled work still consumes the CPU that live queries need.
3. **Priority in the scan pool.** Plain rayon work-stealing has no priority; a large background
   merge can occupy every worker. Use **separate pools** (foreground / background) with
   distinct thread counts rather than one pool with priorities, which is simpler and actually
   enforceable.
4. **Chunk size** trades scheduling granularity against per-chunk overhead. Block-sized chunks
   (64 KiB–4 MiB, matching the format) are the natural unit and give ~0.01–0.3 ms of
   granularity.

### NUMA and SMT
- On a dual-socket node, cross-socket access roughly halves effective bandwidth for a
  bandwidth-bound scan. **Pin scan threads and allocate their buffers on the same node**, or
  prefer single-socket instance types and treat the box as two independent units.
- SMT gives little on bandwidth-bound SIMD (both siblings contend for the same load/store
  ports and the same memory controller). Count *physical* cores when sizing the scan pool
  (OQ-113).
- **AVX-512 downclocking is largely a historical concern**: Ice Lake showed a ~175 MHz average
  drop and +24% power, but on Sapphire Rapids peak frequency is *"similar with and without
  AVX-512"* and power is basically unchanged; AMD Genoa does not downclock either. Sustained
  all-core AVX-512 still settles at ~53% (SPR) and ~84% (Genoa) of single-core turbo, which is
  a capacity-planning input, not a reason to avoid AVX-512. **Use 512-bit vectors on modern
  server CPUs.**

## 4. CPU admission control

Same shape as memory ([`memory-management.md`](memory-management.md) §4), and easier, because
the cost is *predictable*: the planner knows `p` and the posting-list sizes before executing.

> **D-63.** Every query estimates **bytes to scan** at plan time and acquires a scan-budget
> reservation. Over-budget queries degrade (lower `p` ⇒ lower recall, reported in `meta`) or
> are shed with `429`. A query whose actual scan exceeds its estimate by a wide margin is
> cancelled and logged — that is a planner bug.

> **D-64.** Per-tenant CPU accounting in **bytes scanned**, alongside cache bytes, memory, and
> blob requests (Design rule 13). This is also the natural billing unit and matches what S3
> Vectors does by tiering query price on index size.

## 5. Overload: the metastable failure risk

This is the failure mode that turns a bad minute into a bad afternoon.

> A **metastable failure** is a *self-sustaining congestive collapse in which a system degrades
> in response to a transient stressor but fails to recover after the stressor is removed.*
> **Retry policy is the sustaining effect in >50% of studied incidents**, and **direct load
> shedding was used in >55% of recoveries.**

Our architecture has three built-in amplifiers that must be controlled:

| Amplifier | Why it is dangerous | Control |
|---|---|---|
| **Hedged requests** ([`load-and-hotspots.md`](../04-cluster/load-and-hotspots.md)) | A hedge doubles CPU exactly when CPU is scarce — textbook positive feedback | Disable **blind** hedging above a load threshold. **Refined:** the decision is per-target-health, not global — hedging *away from a suspect target* is the best per-request gray-failure mitigation and should survive the load threshold. See [`../04-cluster/gray-failure.md`](../04-cluster/gray-failure.md) §7. |
| **Client retries** | The single most common sustaining effect | **Retry budgets** (retries capped as a fraction of requests), server-advertised `Retry-After`, and exponential backoff *with jitter* |
| **Cold-start stampede** → more cold work → slower → more cold | Positive feedback via cache misses | Singleflight + admission control on cold fills (already specified) |

Standard controls, all applicable:
- **Adaptive concurrency limits** (Netflix-style, TCP-congestion-inspired): a fixed limit either
  under-utilizes or collapses; the limit must track observed latency.
- **CoDel queue discipline**: FIFO under normal load, **switch to LIFO under pressure** so fresh
  requests with a chance of meeting their deadline are served and stale ones are dropped.
  Cheap, and a large win on tail behaviour.
- **Circuit breakers per query shape** (fan-out width, scan size, selectivity), not just per
  tenant.
- **Shed early.** Rejecting 5% at the door beats degrading 100%.

> **D-65.** Overload behaviour is **tested, not assumed**: a load test that ramps past capacity
> and then removes the stressor must show the system *recovering*. A system that does not
> return to baseline after the load is removed has a metastable failure, and that is a release
> blocker.

## 6. cgroups: do not set a CPU limit

This is counterintuitive enough to be worth stating plainly.

| Control | Effect |
|---|---|
| `cpu.weight` (from the request) | Proportional share **under contention**. Harmless when idle. |
| `cpu.max` (from the limit) | CFS bandwidth control: a **hard per-100 ms budget**. |

The pathology: *"CPU limits enforce a peak budget, not an average, and a workload that bursts
above the limit in any 100 ms window gets throttled, even if average usage is nowhere near the
limit."* This produces **high throttling with low average CPU** — a genuinely confusing
signature — and adds latency with no workload benefit.

Our workload is *exactly* the pathological shape: bursty millisecond-scale SIMD scans against a
mostly-idle baseline.

> **D-66.** Set **CPU requests, not CPU limits**. Size requests near steady-state p95. Rely on
> `cpu.weight` for fairness under contention and on our own admission control (D-63) for
> protection. Monitor `throttled_time`; any non-zero value means a limit is set somewhere it
> should not be.

## 7. Metrics

- **Bytes scanned/s** per node and per tenant — the real capacity metric (C-1).
- Scan-pool utilization, queue depth, and queueing delay, foreground vs background separately.
- `throttled_time` (should be zero), run-queue latency, PSI CPU `some avg10`.
- Hedge rate and hedge-win rate; retry rate vs retry budget.
- Shed rate (`429`s), and **recovery time after a load spike** — the metastability indicator.
- Effective bandwidth achieved by the scan loop vs. theoretical, to catch NUMA misplacement.

## 8. Open questions raised

- **OQ-111 (Tier 1, refines OQ-75)** — Measured bytes-scanned/s per node on target instance
  types, from RAM and from NVMe separately. This is the cost model's dominant input and C-1
  shows it varies 75× with workload shape.
- OQ-112 — Foreground/background core split: what floor does compaction actually need to keep
  segment counts bounded at our write rates?
- OQ-113 — Does SMT help our bandwidth-bound scan at all? Measure; size the pool in physical
  cores if not.
- OQ-114 — Chunk size for cancellation granularity vs. per-chunk overhead.
- OQ-115 — Do we need NUMA-aware buffer allocation, or should we simply prefer single-socket
  instances and treat a 2-socket box as two nodes?
- OQ-116 — Adaptive concurrency algorithm choice (Vegas/Gradient) and how it interacts with the
  blob-store congestion controller, which is a *second* adaptive limiter in the same request
  path. Two interacting controllers can oscillate.

## Sources

- [Metastable Failures in Distributed Systems — HotOS 2021 (Bronson et al.)](https://sigops.org/s/conferences/hotos/2021/papers/hotos21-s11-bronson.pdf)
- [Metastable Failures in the Wild — USENIX](https://www.usenix.org/publications/loginonline/metastable-failures-wild)
- [Netflix/concurrency-limits — GitHub](https://github.com/Netflix/concurrency-limits)
- [How Uber Conquered Database Overload: From Static Rate-Limiting to Intelligent Load Management](https://www.uber.com/us/en/blog/from-static-rate-limiting-to-intelligent-load-management/)
- [Kubernetes CPU Throttling: CFS Quotas and Latency Fixes — CloudOptimo](https://www.cloudoptimo.com/blog/kubernetes-cpu-throttling-cfs-quotas-and-latency-fixes/)
- [How Kubernetes CPU Requests and Limits Actually Work — CloudBolt](https://www.cloudbolt.io/how-kubernetes-requests-limits-work/cpu/)
- [AVX-512 Performance Comparison: AMD Genoa vs. Intel Sapphire Rapids & Ice Lake — Phoronix](https://www.phoronix.com/review/intel-sapphirerapids-avx512/8)
- [Microarchitectural comparison of Grace, Sapphire Rapids, and Genoa — ScienceDirect](https://www.sciencedirect.com/science/article/pii/S0167819126000013)
- Arithmetic reproducible in `09-rust-stack/cpu-budget.py`.
