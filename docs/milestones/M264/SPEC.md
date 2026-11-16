# M264: per-query timings and the cache hit ratio

## Serves
The [turbopuffer gap list](../../comparisons/turbopuffer-gaps-2026-11-15.md), "`performance`
per query and write": the vendor reports `server_total_ms`, `query_execution_ms` and
`cache_hit_ratio` on every query; pstore reports blob requests and bytes (`meta.cost`) and,
on request, the plan (M165), but no time and nothing about its cache. Workstream E/F of [the
delivery plan](../../comparisons/turbopuffer-next-plan.md). No D- or OQ- id covers it.

## Delta
1. **`meta.performance`** on every `POST /v1/indexes/{index}/query` response, single or
   multi-query (M9g), with:
   - `server_total_ms`: a float, milliseconds from the handler's first line to the response
     being built (JSON serialisation of the body not included).
   - `query_execution_ms`: a float, milliseconds spent running the planned queries (and
     facets): the part of `server_total_ms` after parsing, planning and admission.
   - `cache_hit_bytes` and `cache_miss_bytes`: bytes the tenant's read cache answered from
     memory or disk, and bytes it fetched from the store, over this request's cacheable
     reads (ranges, suffixes, immutable objects; never a whole-object `get`, which is never
     cached).
   - `cache_hit_ratio`: `hit / (hit + miss)`, a float in `[0, 1]`; `null` when the request
     made no cacheable read or the server keeps no cache.
2. **Attribution is `cost`'s**: differences of the tenant's monotone counters, taken before
   and after. Exact with one request in flight for the tenant, cross-attributed under
   concurrency, as `meta.cost` already says of itself.
3. The counters live on the tenant's `Caching` view (`pstore-cache`), two relaxed atomic
   adds per cacheable read; an uncached server counts nothing and reports `null`.

**No setting.** Nothing changes on disk or in what a query reads or returns; the additions
are a field in `meta`, two clock reads per request and two atomic adds per cacheable read.
The downside is measured negligible (AC5), so by the settings rule it ships without one.

**Not changed:** `meta.cost`, `explain`, results, the request count of any query, writes'
responses (the facade fills the vendor's write `performance` from what writes report).

## Acceptance criteria
1. **Shape.** `performance_is_reported`, in `crates/pstore-server/tests/it/performance.rs`:
   a query's and a multi-query's `meta.performance` carry the five fields; both times are
   finite, non-negative, and `query_execution_ms <= server_total_ms`.
2. **Cold then warm.** `cache_ratio_cold_then_warm`: over a server with a memory cache and a
   folded index, a second server's first query reports `cache_miss_bytes > 0`, no more than
   its `cost.bytes_read`, and a ratio of `hit / (hit + miss)` below 1; the same query again reports `cache_miss_bytes == 0`,
   `cache_hit_bytes` equal to the first's misses plus hits, and `cache_hit_ratio == 1.0`.
3. **No cache, no ratio.** `no_cache_reports_null_ratio`: a server without a cache reports
   `cache_hit_ratio: null` and both byte counts 0, over the same folded index.
4. **Counted where read.** `traffic_counts_each_cacheable_read`, in
   `crates/pstore-cache/tests/`: `get_range`, `get_ranges` (with a repeated and a coalesced
   range), `get_suffix` and `get_immutable`, cold then warm, move the counters by the exact
   bytes, and `get` moves neither.
5. **Negligible.** `cargo run --release -p pstore-server --example perf_overhead` (new)
   prints the median latency of 2,000 warm cached queries through the router, how many
   cacheable reads one makes, and the measured time of what M264 adds to one (two clock
   reads, two atomic adds per cacheable read, the five fields' serialisation), timed over a
   million repetitions. The added time is under 1% of the median, recorded in VERIFIED.md,
   `provisional` (container numbers).
6. **Gates:** fmt, clippy, the workspace tests, `scripts/gen-clients.py --check` (the
   OpenAPI `QueryMeta` gains `performance`), and the named mutations below observed red.

## Test plan
AC1 to AC4 are seen red against the parent, which has no `performance` and no counters.

| AC | Mutation |
|---|---|
| 1 | `query_execution_ms` reported as 0; the two times swapped |
| 2 | hits taken as asked bytes without subtracting the misses; the ratio over misses alone |
| 3 | an uncached view counting its reads |
| 4 | a batch's miss counted once for a range asked twice; `get` counted |

That `query_execution_ms` starts after admission, not at the handler's start, is not
checkable by a test: both orders give `query_execution_ms <= server_total_ms`. It is read in
review.

## RA budget
Unchanged: no blob request is added or removed.

## Risks
- **Concurrency** blurs both the byte counts and the ratio, exactly as it blurs `cost`.
- **A disk-tier hit** counts as a hit: it read no store, though it read the local disk.

## Tasks
- **M264.1** The spec.
- **M264.2** `Caching`'s counters, the engine's store accessor, `meta.performance`, the
  OpenAPI field and regenerated clients, tests, the overhead example, docs.
