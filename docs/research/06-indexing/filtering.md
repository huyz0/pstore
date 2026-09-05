# Filtering: Where a Clustered Index Quietly Wins

**Answers:** Q22
**Status:** Complete (v1)

## Why this is a headline feature, not a checkbox

Real workloads are almost never pure vector search. They are
`WHERE tenant_id = X AND created_at > T AND status IN (...)` **plus** semantic similarity.
And this is exactly where the market leaders are weakest:

- Graph indexes (HNSW) suffer **islands and dead ends** under selective filters: hiding
  non-matching nodes disconnects the graph and traversal terminates early, so **recall
  collapses precisely when the filter is most selective**.
- High-cardinality attributes (`user_id`, timestamps) force a flat scan in most engines.
- Filter-index fusion approaches (ACORN, Filtered-DiskANN) **tightly couple metadata to the
  vector index**, which breaks for ad-hoc queries over arbitrary schemas — fatal for a
  general-purpose multi-tenant product.

Meanwhile the published finding is blunt: **at low selectivity, IVF-family beats HNSW
decisively**, because pre-filtering composes with cluster pruning — you prune distance
computations early. At high selectivity, HNSW holds and post-filtering becomes competitive.

We chose a clustered index for round-trip reasons. **It hands us the better filtered-search
architecture for free.**

## The three strategies, and when each wins

| Strategy | Mechanism | Wins when |
|---|---|---|
| **Exact scan** | Build the bitmap, brute-force score only matching vectors | Filtered set is small (≲50k–200k) |
| **Pre-filter + clustered ANN** | Build bitmap, probe posting lists, AND during the scan | Filter is selective but the set is still large |
| **Post-filter** | ANN first, filter results, oversample to compensate | Filter is permissive (high selectivity) |

> **D-16.** The planner chooses adaptively per query, using an estimated selectivity, and
> **always** has an exact fallback. Milvus's result is the guide: *system-level behaviour —
> dual-pool traversal plus an adaptive brute-force fallback — beats cleverer algorithms.*
> Meanwhile pgvector's cost model regularly picks a worse plan than a sequential scan. **The
> fallback matters more than the clever path.**

## Selectivity estimation without a round trip

Choosing a plan requires an estimate *before* fetching anything. Sources, all already in the
cached index section:

1. **Zone maps** per block (min/max/null/count per column) — bound the matching blocks.
2. **Count-distinct sketches (HLL)** per column per segment.
3. **Histograms** for numeric/date columns.
4. **Bloom / ribbon filters** for equality on high-cardinality columns.
5. **Cardinality of the index itself** from the manifest.

All of this lives in the index section, which is cached, so **plan selection costs zero blob
requests**. This is why the index section deserves its own cache priority class.

The survey's **GLS (global–local selectivity) correlation** — local selectivity (fraction of
the true k-NN that pass the filter) vs. global selectivity (fraction of the whole dataset that
passes) — is a better index-selection signal than distance-based heuristics, spanning nearly
the full [−1, 1] range versus [−0.3, 0.3] for prior metrics.

> **D-17.** Track per-index, per-predicate-shape **observed GLS** as a feedback signal from
> executed queries, stored in segment stats. Correlated filters (e.g. `tenant_id`, where
> matching docs are also semantically clustered) behave completely differently from
> anti-correlated ones, and only measurement reveals which you have.

## Bitmap machinery

- Filter evaluation produces a **roaring bitmap** of matching row ids per segment.
- It composes by AND with the **delete vector** (also roaring) — deletes and filters are the
  same operation, which is a nice simplification.
- Bitmaps are computed from columnar blocks; zone maps let us skip whole blocks without
  fetching them, which on object storage means skipping *network fetches*.
- For very common filter shapes (e.g. a single `tenant_id` in a shared index), **materialize**
  the bitmap in the segment as a precomputed index. This is Pinecone's "pre-computed filter
  representations" vs. "ad-hoc application" distinction. Materialize only above an observed
  usage threshold.

## The multi-tenant special case

The most common filter in practice is `tenant_id = X` inside a shared index. Do not solve this
with a filter at all:

> **D-18.** Support **partitioned indexes**: an index may declare a partition key, and
> documents are physically segregated by it into separate segment sets. `tenant_id = X` then
> becomes *segment selection* (zero scan cost, exact) rather than filtering.

This is a large practical win and is only possible because our unit of tenancy is cheap. Most
"filtered vector search is slow" complaints in the industry are actually this problem
mis-modelled.

## Pre-filter integration with clustered ANN

The important detail: with a pre-filter bitmap, the number of posting lists to probe must
**increase**, because each probed list yields fewer surviving candidates. So:

```
p_effective = p_base / estimated_selectivity, clamped to [p_base, p_max]
```

And since `p` costs **bytes, not round trips** (all lists fetched in parallel), we can raise
it aggressively. **This is the structural advantage over graph indexes**: they cannot "search
harder" without more sequential hops; we can, for free in latency.

If `p_effective` would exceed the total posting list count, fall through to exact scan.

## Filter expression support

Target parity with the leaders: equality, comparison, `IN`, `NOT`, nested `AND`/`OR`, range,
null checks, array containment, glob and regex (trigram-accelerated), and full-text predicates
as filters. All pushed down to bitmap evaluation over columnar blocks.

## Cost summary

| Query | RA |
|---|---|
| Plan selection | **0** (cached stats) |
| Exact scan path | 1 Rpar (matching blocks only, zone-map pruned) |
| Pre-filter + ANN | 1 Rpar (filter columns) + 1 Rpar (posting lists) → 2 Rseq depth |
| Partitioned index | same as unfiltered, on a smaller segment set |

## Open questions raised

- OQ-46: Selectivity thresholds for plan switching — measure on YFCC (the standard filtered
  benchmark) plus a synthetic correlated-filter dataset.
- OQ-47: When is materializing a filter bitmap worth its storage and build cost?
- OQ-48: Should partitioned indexes be a distinct concept or just "many indexes + a fan-out
  query"? The latter is simpler and reuses machinery. **Leaning: many indexes.**
- OQ-49: How do we keep `p_effective` from exploding on anti-correlated filters where the
  matching docs are semantically far from the query?

## Sources

- [Filtered Approximate Nearest Neighbor Search in Vector Databases: System Design and Performance Analysis — arXiv](https://arxiv.org/html/2602.11443)
- [Accurate and Efficient Metadata Filtering in Pinecone's Serverless Vector Database](https://www.pinecone.io/research/accurate-and-efficient-metadata-filtering-in-pinecones-serverless-vector-database/)
- [JAG: Joint Attribute Graphs for Filtered Nearest Neighbor Search — arXiv](https://arxiv.org/pdf/2602.10258)
- [GateANN: I/O-Efficient Filtered Vector Search on SSDs — arXiv](https://arxiv.org/pdf/2603.21466)
- [Metadata Filtering and Hybrid Search for Vector Databases — Dataquest](https://www.dataquest.io/blog/metadata-filtering-and-hybrid-search-for-vector-databases/)
- [Pre and Post Filtering in Vector Search — DEV](https://dev.to/volland/pre-and-post-filtering-in-vector-search-with-metadata-and-rag-pipelines-2hji)
