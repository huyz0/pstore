# The Query Path: A Three-Round-Trip Budget

**Answers:** Q26
**Status:** Complete (v1)

## The budget

Every user-facing query gets **≤3 sequential blob round trips**. Fan-out within a round is
wide (bandwidth is free, latency is not) — but **bounded by a memory reservation, not
unlimited**: see [`../09-rust-stack/memory-management.md`](../09-rust-stack/memory-management.md)
§3. Where residency binds, shrink block size before narrowing width — trading memory for blob
requests ($0.0004/1000) beats trading memory for round trips. This is a hard constraint that every feature
must fit inside; features that need a 4th data-dependent hop must be redesigned or made
opt-in.

turbopuffer states the same discipline: cold queries take 3–4 round trips of ~100 ms each,
≈400 ms for 1M documents.

## Anatomy of a cold vector query

```
                                      ┌ round trip ┐
 t=0    parse, authenticate, plan                     (0 RT — all inputs cached)
 t=0    RT-A  ┌─ HEAD (conditional, usually 304)      ┐  merged: these are
              ├─ segment index sections (uncached)    │  independent, so
              ├─ centroid tables (uncached)           │  ONE round trip
              └─ filter columns / zone maps           ┘
 t=30ms compute: evaluate filter → bitmap; estimate selectivity; choose plan;
                 score centroids; select p posting lists
 t=32ms RT-B  ┌─ p posting-list / cluster blocks, all segments, all shards ┐ one RT
 t=62ms compute: SIMD scan 1-bit codes against the bitmap; top-(k·oversample)
 t=70ms RT-C  ┌─ rerank codes + document payloads for the survivors ┐ one RT
 t=100ms compute: rerank, merge across shards, format
```

**Three round trips. ~100–150 ms cold, dominated by network latency, not compute.**

The critical structural property: **RT-A merges everything that can be known without looking
at data.** A naive implementation would fetch HEAD, *then* the manifest, *then* the index
section, *then* the centroids — four sequential hops before any real work. Avoiding that is
the single highest-leverage optimization in the engine, and it is why the manifest carries
enough metadata to issue RT-A's fetches speculatively.

> **D-25.** The planner issues **speculative parallel fetches** in RT-A for everything the
> query *might* need, accepting some waste. Bytes are free; hops are not. (Pattern 5.)
> Speculation is charged against the query's memory reservation, so a plan that would
> speculate beyond its budget speculates less rather than risking the process (D-55).

## Warm query

All of RT-A and RT-B hit cache; RT-C often does too. Latency becomes compute-bound:
SIMD scanning, bitmap ops, merge. **Target ≤10 ms p50**, which is achievable because scanning
`p × 4,000` 1-bit vectors at 768 dims is a few million popcounts — microseconds to low
milliseconds.

At that point the bottleneck is *our code*, so the query engine must be genuinely fast:
vectorized execution, no per-row virtual dispatch, arena allocation, no lock contention.

## Fan-out across shards

`fan_out = shards × segments_per_shard`. Bounded by:
- shard count policy (≈256 max before two-level aggregation),
- merge policy (≈10–12 live segments per shard).

Each shard's work happens on its placement node in parallel; the coordinator merges.
Tail-latency controls (hedging, tied requests, partial results) from
`04-cluster/load-and-hotspots.md` apply here.

## Planning inputs, all free

| Input | Source |
|---|---|
| Schema, shard map, segment list | Manifest (cached) |
| Zone maps, histograms, HLL, blooms | Segment index sections (cached) |
| Observed GLS / selectivity history | Segment stats (cached) |
| Cache residency | Local — **the planner knows what is already cached** |

That last one is unusual and valuable: **plan choice should depend on what is cached.** If
the exact-scan path's data is warm and the ANN path's is cold, exact scan may be faster
*and* more accurate. A conventional cost model that ignores cache state will pick wrong.

> **D-26.** Cache residency is a first-class term in the cost model.

## Vectorized execution

Columnar, batch-at-a-time (e.g. 1,024 rows), Arrow-compatible. Operators: scan, filter,
bitmap-and, distance-score, top-k, rerank, aggregate, merge. This is what turbopuffer means
by a "vectorized execution model" and it is table stakes for the warm path.

## Pagination

Offset-based pagination over ANN results is incoherent (results are approximate and shift
between calls). Provide instead:
- **Cursor pagination** pinned to an epoch + a score/id watermark. The cursor encodes the
  epoch, so page 2 reads the *same snapshot* as page 1 — correct by construction thanks to
  MVCC, and impossible in systems without immutable snapshots.
- Document `max_page_depth`; deep pagination over ANN is discouraged and expensive.

## Aggregations

Counts, sums, min/max, cardinality (HLL), top-N facets — computed over the filter bitmap
using columnar blocks, with zone maps and precomputed per-segment partial aggregates for
common cases. Many aggregations can be answered **entirely from the cached index section**
with zero data fetches; that should be an explicit fast path.

## Timeouts and partial results

Every query carries a deadline. On expiry, return what completed with an explicit
`partial: true` and the list of shards included. For search, a 98%-complete answer in 20 ms
usually beats a complete one in 400 ms — but the client must be told, never silently.

## Cost summary

| Query | Rseq depth | Typical Rpar width |
|---|---|---|
| Cold vector | 3 | 10–200 |
| Cold BM25 | 2–3 | 10–100 |
| Cold hybrid | 3 (shared RT-A) | more |
| Warm anything | 0 | 0 |
| Small index exact | 1 | 1–10 |
| Aggregation from stats | 0–1 | small |

## Open questions raised

- OQ-60: How much waste does speculative RT-A fetching actually incur? Measure bytes fetched
  vs. bytes used.
- OQ-61: Cache-aware cost model — needs a real design, not just a term.
- OQ-62: Two-level fan-out threshold and aggregation-tree shape.

## Sources

- [turbopuffer — Architecture (3–4 roundtrips, ~100 ms each)](https://turbopuffer.com/docs/architecture)
- [turbopuffer — Database of Databases (vectorized execution)](https://dbdb.io/db/turbopuffer)
- [Quickwit 101 — byte-range query execution over object storage](https://quickwit.io/blog/quickwit-101)
- [Filtered ANN Search in Vector Databases — arXiv](https://arxiv.org/html/2602.11443)
