# Runtime and I/O Architecture

**Answers:** Q29
**Status:** Complete (v1)

## The workload's actual shape

Two very different phases, on the same machine:

| Phase | Character |
|---|---|
| **Fetch** | Hundreds of concurrent HTTPS requests to the blob store, ~30 ms each, plus NVMe reads. Latency-bound, I/O-concurrency-heavy. |
| **Scan** | SIMD distance computation, bitmap ops, decompression. **CPU-bound, cache-sensitive, embarrassingly parallel.** |

A warm query is ~100% phase 2. A cold query is dominated by phase 1's latency but still ends
in phase 2. **Both must be fast, and they interfere with each other** — a long SIMD scan
blocking an async executor delays every in-flight fetch on that thread.

## Runtime choice

The io_uring thread-per-core runtimes are genuinely faster on raw I/O: at 4 threads,
`tokio-uring` 1.39M req/s, `monoio` 1.36M, `compio` 1.27M, `glommio` 1.14M — **27–64% faster
than standard Tokio**. Thread-per-core means tasks are pinned, never migrate, no other thread
touches your data, and `Send` is not required.

Tempting. But:

- Our I/O is **HTTPS to a remote service at ~30 ms**, not millions of tiny local syscalls.
  Syscall efficiency is not our bottleneck; we will never be near 1M req/s of blob I/O per
  node (that would be $1,440/hour in GET charges).
- Thread-per-core hurts when work is **unevenly sized** — and ours is wildly uneven (one query
  scans 1,000 vectors, another scans 10 million). Work-stealing is exactly the right response
  to that skew; pinning is exactly the wrong one.
- Ecosystem: `object_store`, `hyper`, `tonic`, `axum`, `tantivy` integrations, and the entire
  tracing/metrics stack are Tokio-shaped.
- io_uring runtimes are Linux-only and kernel-version-sensitive.

> **D-33.** Use **Tokio (multi-threaded, work-stealing)** for the network and orchestration
> layer, with a **separate `rayon` pool (or dedicated blocking pool) for CPU-heavy scan
> work**. Never run a SIMD scan on the async runtime's worker threads.

The split is the important part. It gives us the ecosystem, the right scheduler for skewed
work, and isolation between phases. Revisit io_uring only for the **NVMe cache tier**, where
`tokio-uring`/`compio` for local file I/O could be a targeted win without changing the whole
architecture (OQ-69).

## Concurrency control — the thing that actually matters

More important than the runtime choice:

1. **Adaptive concurrency limiter per prefix-group** (Design rule 6). AIMD or Gradient-style,
   reacting to `503 SlowDown` and observed latency. Without this, a burst produces a 503 storm
   and a latency cliff.
2. **Singleflight** on every cache miss key. Mandatory (`load-and-hotspots.md`).
3. **Per-tenant concurrency and request budgets** (Design rule 13).
4. **Bounded queues everywhere**, with explicit shed policies. An unbounded channel is a
   latent outage.
5. **Deadline propagation.** Every task carries the query deadline; expired work is cancelled,
   not completed. At 1,000-way fan-out, uncancelled work is a large hidden load.

## Memory discipline

> Full treatment in [`memory-management.md`](memory-management.md). The essentials below; the
> load-bearing point is that **Rust allocation failure aborts and is not catchable**, so every
> data-proportional allocation must be accounted above the allocator (Invariant I3).

- **Zero-copy from cache to scanner.** `Bytes` all the way through: blob response → cache →
  decompressed block → SIMD kernel. Every copy of a 4 MB block at 10k QPS is real bandwidth.
- **Arena/bump allocation per query** for intermediate results; drop the arena at the end
  rather than freeing millions of small objects.
- **Alignment**: 64-byte-aligned quantized code buffers for AVX-512.
- **`jemalloc` with `background_thread:true`** — not mimalloc: mimalloc reclaims on allocation
  activity, which suits a busy uniform service, while ours has worker threads idle between
  bursts across 50M mostly-idle indexes. jemalloc purges on decay timers and exposes the
  `stats.allocated` vs `stats.resident` surface we need. (D-59)
- Explicit memory budget per query with admission control; OOM at 10,000 nodes is a fleet
  event, not a node event.

## Compute

- **Runtime SIMD dispatch** — one binary must run on AVX2, AVX-512, and NEON. `simsimd` does
  this; detect features **once at startup**, never per batch. Note that **most `std::arch`
  intrinsics are safe to call as of Rust 1.87**, so hand-written kernels no longer imply
  scattered `unsafe`. See [`hot-loop-performance.md`](hot-loop-performance.md) §3.
- **Batch sizes** tuned to L2 cache, not to "looks nice". The unit is a **block/morsel**
  (64 KiB–4 MiB), the same unit used for fetch, decode, and cache — one unit of work throughout
  the system ([`hot-loop-performance.md`](hot-loop-performance.md) §6).
- **Prefetching** and **non-temporal loads** in scan loops: we score-and-drop, so scanned blocks
  are never reused and should not evict the centroids that are.
- **Huge pages.** A 96 MB scan touches 23,438 4 KiB page entries against an L2 TLB of ~2,000 —
  guaranteed thrashing. 2 MiB pages reduce that to 46 (D-91).
- **The scan is memory-bound by ~7× per core**, so the payoff is in *fewer bytes*, not faster
  instructions. Budget kernel effort accordingly (D-90).
- Consider **GPU** for very large exact scans much later; it breaks the homogeneous-fleet
  property, so the bar is high.

## Observability

Non-negotiable, from the first commit:
- Blob requests by class and tenant (this is the bill).
- Bytes fetched vs. bytes used (measures speculative-fetch waste, D-25).
- Round-trip depth per query — **assert ≤3 in tests**, alert in production.
- Cache hit rate per class.
- CAS attempts, losses, and 409s per index.
- Cold-query ratio per index.
- **RSS vs `stats.allocated`** (fragmentation ratio), memory-pool reservations by consumer and
  tenant, `memory.events.high`, and PSI memory `full avg60`. See
  [`memory-management.md`](memory-management.md) §9.

> **D-34.** Round-trip depth is a **tested invariant**, not a metric to look at later. A test
> that runs a cold query against the fault-injecting blob store and asserts the sequential
> depth is ≤3 will catch the single most likely class of performance regression.

## Open questions raised

- OQ-69: `compio`/`tokio-uring` for the NVMe cache tier specifically — measure.
- OQ-70: Optimal split of cores between Tokio workers and the scan pool under mixed load.
- OQ-71: Does `simsimd`'s dispatch overhead matter at our batch sizes, or do we need
  monomorphized kernels?

## Sources

- [Apache Iggy's migration to thread-per-core powered by io_uring](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)
- [Thread-Per-Core Async in Rust — Chrysostomos Nanakos](https://www.include.gr/writing/rust-thread-per-core-async.html)
- [Comparing with Tokio and Glommio — bytedance/monoio](https://zread.ai/bytedance/monoio/30-comparing-with-tokio-and-glommio)
- [simsimd — crates.io](https://crates.io/crates/simsimd/4.3.0)
- [Best practices design patterns: optimizing Amazon S3 performance — AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html)
