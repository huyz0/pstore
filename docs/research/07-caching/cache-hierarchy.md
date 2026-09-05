# Cache Hierarchy: The Only Reason This Is Fast

**Answers:** Q24
**Status:** Complete (v1)

## The stakes

| Tier | Latency | Ratio vs. blob |
|---|---|---|
| RAM | ~100 ns | ~300,000× faster |
| NVMe | ~50–150 µs | ~300× faster |
| Object storage | ~30 ms | 1× |

The cache is not an optimization. **It is the difference between a 14 ms product and a 400 ms
product**, and turbopuffer's published numbers (cold p50 874 ms → warm p50 14 ms on 1M docs)
quantify exactly that gap.

## What we cache, in priority order

Not all bytes are equal. A byte of centroid table is worth thousands of bytes of raw vectors,
because *every* query needs it. Cache admission must be class-aware, not LRU-over-bytes.

| Class | Size | Value | Policy |
|---|---|---|---|
| **1. Manifest / HEAD** | KB | Every request | Always RAM, TTL-validated |
| **2. Segment index sections** ("hotcache") | <0.1% of segment (~10 MB per 15 GB) | Every query on that segment; converts all later access into exact ranges | **Always RAM if possible**, pinned |
| **3. Centroid tables** | ~100 MB per 1B vectors (quantized) | Every vector query; unblocks everything downstream | **Pinned, prefetched first on cold start** |
| **4. Term dictionaries / FSTs, block-max metadata** | small | Every FTS query; enables skipping *fetches* | RAM |
| **5. Filter columns / zone maps** | moderate | Plan selection + filtering | RAM then NVMe |
| **6. Quantized vector blocks (1-bit)** | 96 GB per 1B @768d | The scan tier | **NVMe**, this is the bulk |
| **7. Posting-list blocks** | large | FTS scan | NVMe |
| **8. Documents / stored fields** | largest | Only for the final top-k | NVMe, low priority |
| **9. Full-precision vectors** | 1.5 TB per 1B @768d | Exact rerank only | **Do not cache by default** |

> **D-21.** Cache admission is by **class**, with per-class quotas, not by a single global
> LRU. Classes 1–4 are tiny and must never be evicted by a burst of class 6–8 traffic. This
> mirrors turbopuffer's "prioritization of cache fills for more important files ... such as
> centroids."

## Implementation: `foyer`

**`foyer`** is a Rust hybrid (memory + disk) cache library, inspired by CacheLib (C++) and
Caffeine (Java). Benchmarks put it ahead of `moka`, second only to `quick-cache` (which is
memory-only). It is used by Percas, a distributed persistent cache service for NVMe.

> **D-22.** Use **`foyer` as the hybrid cache**, with our own class-aware admission policy on
> top. Fall back to `moka`/`quick-cache` for pure in-memory sub-caches (e.g. the manifest
> cache) where the hybrid machinery is overhead.

Requirements we must verify against `foyer` (OQ-54):
- Per-class quotas / multiple isolated instances.
- Pinning (classes 2–3 must be un-evictable while an index is active).
- Zero-copy `Bytes` handoff so a cached block goes to the SIMD scanner without a memcpy.
- Direct I/O and NVMe-friendly write patterns (avoid write amplification on the SSD).
- Per-tenant accounting for Design rule 13.

## Cache key

```
(segment_id, section, block_range)
```
Segment ids are globally unique and segments are immutable ⇒ **cache entries never need
invalidation**. This is the quiet superpower of an immutable-object design: no coherence
protocol, no invalidation messages, no stale-cache bugs. A cached block is correct forever.

Eviction is purely a capacity decision.

## Sizing

For a node with 128 GB RAM / 4 TB NVMe:
- RAM: ~16 GB for classes 1–5 (thousands of active indexes' metadata), ~16 GB page-cache-ish
  for hot class 6, rest for query execution and OS.
- NVMe: ~3.5 TB for classes 6–8.

3.5 TB of NVMe holds the 1-bit scan tier for **~36 billion 768-dim vectors** — on one node.
That is the number that makes the economics work: a modest fleet can hold the entire scan
tier of an enormous corpus warm.

## Write-through vs. write-around

Newly written segments: **write-through** to cache (we already have the bytes in memory, and
they are about to be queried). Compaction outputs: **write-around** (large, and not
necessarily hot). Cheap heuristic, meaningful hit-rate difference.

## Cache and cost

Every cache hit is a blob request not made. At 95% hit rate, blob read cost drops 20×. But
the *latency* benefit dominates the accounting: the cache is justified by p50, and the cost
saving is a bonus. State it that way to avoid over-tuning for hit rate at the expense of
tail latency.

## Open questions raised

- OQ-54: Verify `foyer` supports per-class quotas, pinning, and zero-copy handoff; if not,
  what do we build?
- OQ-55: Optimal RAM:NVMe ratio and instance type (i4i/i7ie vs. i8g etc.) — a cost-per-warm-
  byte optimization.
- OQ-56: Does class-aware admission actually beat a well-tuned S3-FIFO/W-TinyLFU on our
  traces? Must be measured, not assumed.

## Sources

- [foyer — Hybrid cache for Rust](https://foyer.rs/docs/overview)
- [foyer-rs/foyer — GitHub](https://github.com/foyer-rs/foyer)
- [Foyer: A Hybrid Cache in Rust — Past, Present, and Future — MrCroxx](https://blog.mrcroxx.com/posts/foyer-a-hybrid-cache-in-rust-past-present-and-future/)
- [HybridCache — foyer docs.rs](https://docs.rs/foyer/latest/foyer/struct.HybridCache.html)
- [turbopuffer — Architecture (cache tiers, cache fill prioritization)](https://turbopuffer.com/docs/architecture)
- [Quickwit 101 — hotcache is <0.1% of split size](https://quickwit.io/blog/quickwit-101)
