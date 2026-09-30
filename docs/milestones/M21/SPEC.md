# M21 — `hint_cache_warm`: warm an index's metadata before a burst

**Serves:** D-44 (warm cache classes 1–4 only, never bulk), OQ-57's answer, and the warm-up
API of [`load-and-hotspots.md`](../../research/04-cluster/load-and-hotspots.md) item 4 and
[`affinity-and-coldstart.md`](../../research/07-caching/affinity-and-coldstart.md) § Warm-cache API.

## What is true today

- M20's read cache (`PSTORE_CACHE_DIR`) fills only on demand: the first query on each segment
  pays that segment's open round in blob requests.
- A query's open round (`pstore_query::run::open`) reads the footer and index section (`Meta`),
  the delete vector, and, **chosen by its legs and not by the segment**, the centroid table
  and the sparse and text dictionaries (`Pinned`). An absent one is a 404, which no cache keeps
  ([BACKLOG](../BACKLOG.md) row 46).
- `VecIndex::warm` exists and fetches centroids unconditionally, paying that 404 per small
  segment. It is not reused.
- `pstore-server` routes nothing: a request reaches whichever process the client picked.

## Delta

**Engine.** `Engine::warm(index) -> Result<Warmed, EngineError>`, three rounds:

1. One fresh HEAD read (`head::read`, 1 request).
2. Per segment HEAD names, together: `Segment::open`; the delete vector if HEAD names one; the
   centroid table if its `SegmentRef.rows` ≥ this engine's `exact_scan_threshold` and the
   schema's `dims` > 0, the fold's own predicate (`vec_index.rs:357`).
3. Per segment, from its index section: the sparse dictionary if a field names
   `SparsePostings`, the text dictionary if `has_text()`.

`Warmed { exists, segments, fetched }`. `exists` is decided as a query decides it: HEAD names
segments of the index, or this process holds unfolded rows for it. `fetched` counts sidecars
(delete vectors, centroid tables, dictionaries), not footers. Every read uses the class a query
uses, so it lands in the cache as that query's would. **No bulk read** (D-44): no data section,
postings or `IndexRows`. Delete vectors are outside D-44's classes 1–4 but `Pinned` in the code,
and every query reads them; they are warmed.

**Server.** `POST /v1/indexes/{index}/warm`, empty body, the usual tenant header:

- `200 {"segments", "fetched", "meta": {"cost"}}`, cost from the tenant's accounting: a warm is
  **billed**, as the corpus says to charge for it. Unfolded-only: `200`, `segments: 0`.
- `404 index_not_found` when `exists` is false.
- `409 no_read_cache`, with **no request issued**, when the `Api` has no cache: a warm there
  fetches and discards.
- Synchronous: metadata only, three rounds, so it returns warm. turbopuffer's is asynchronous
  because it warms bulk.

**Docs.** `deploy.md`: a warm warms the process that serves it, so a client warms each process
it will query. The parity row points here.

**Not changed:** the query path (row 46's 404s stay), the cache, the format, any other endpoint.
No `classes` parameter: D-44 fixes them. No whole-tenant warm. No routing.

## Acceptance criteria

1. **A warmed index's queries open with no Meta read.** After `warm`, a dense, sparse, text,
   filtered and `rank_by` query each issue **0** `Meta` reads beneath the cache, and every
   `Pinned` read that reaches the store is a 404 for a key the index lacks (row 46). Each costs
   exactly what it costs on a second run with no warm (M20's `queries_and_warm_cost` baseline).
2. **A warm admits no bulk.** During `warm`, **0** `Bulk` or unclassed ranged reads reach the
   store, and bulk residency is unchanged.
3. **A warm reads only what exists.** Index of k segments below the threshold, no dv, no sparse
   or text field: exactly **1 + k** reads, `fetched == 0`. No 404.
4. **Each sidecar that exists is warmed.** Two segments, one of exactly `exact_scan_threshold`
   rows, both with sparse and text fields, one with a delete vector: `fetched == 6`, and
   criterion 1 holds.
5. **Depth ≤ 3**, by `DepthCounting`, for k ≥ 2 segments within `INDEX_BUDGET`. Engine-written
   segments always are (`try_finish` refuses more); an overflowing index section would add one.
6. **A second warm costs 1 read** (HEAD).
7. **It survives a restart.** With a disk tier: warm, close, reopen; criterion 1 holds.
8. **The endpoint.** `200` with `segments`, `fetched`, and `cost.blob_reads` equal to the
   tenant's accounted reads in the call; unfolded-only `200` with `segments: 0`;
   `404 index_not_found`; `409 no_read_cache` after 0 requests.
9. **Nothing else changes.** Existing `pstore-server`, `pstore-engine`, `pstore-cache` tests pass.
10. **Gates.** `./scripts/mutants.sh` over the diff, and `./scripts/gates.sh`, green.

## Test plan

`crates/pstore-engine/tests/warm.rs`, over a store recording each read's class and outcome
(as `pstore-blob/tests/scanning.rs`'s `Classes`) beneath a `Caching` with a core. Each fails
first on a stub `warm` that reads HEAD only.

| # | Test | Mutation it catches |
|---|---|---|
| 1 | `a_warmed_index_opens_without_a_meta_read` | footers not opened; a sidecar kind skipped; wrong class |
| 2 | `a_warm_admits_no_bulk` | the data section or postings read; opening by `scan` |
| 3 | `a_warm_reads_only_what_exists` | centroids or dictionaries fetched unconditionally; `dims > 0` dropped |
| 4 | `every_sidecar_that_exists_is_warmed` | `>=` as `>` at the threshold; the dv skipped; a dictionary kind skipped |
| 5 | `a_warm_is_three_rounds_deep` (k = 3) | segments warmed in a serial loop; dictionaries awaited before round 2 ends |
| 6 | `a_second_warm_costs_head_alone` | a read that bypasses the cache (`get` for `get_immutable`) |
| 7 | `a_warm_survives_a_restart` | the warm filling a memory-only path, so the disk tier misses on reopen |

`crates/pstore-server/tests/warm.rs`: `warm_reports_and_bills`, `warm_of_an_unfolded_index_is_empty`,
`warm_of_a_missing_index_is_404`, `warm_without_a_cache_is_409_and_free`. Mutations: route
missing; cost absent or another tenant's; existence from HEAD alone; the 409 after HEAD.

## RA budget

| Operation | W | Rseq | Rpar | List |
|---|---|---|---|---|
| `warm`, cold | 0 | ≤ 3 | 1 + per segment: footer, + dv, + centroids if clustered, + one per dictionary | 0 |
| `warm`, warm | 0 | 1 | 1 | 0 |

Per explicit call, scaling with one index's segments: the shape of its first query. Other ops unchanged.

## Risks

- **A warm on one process, a query on another.** Nothing routes a tenant; stated in `deploy.md`.
  Nothing automated reveals it.
- **Writer and warmer parameters differ.** A segment clustered below this engine's threshold has
  centroids the warm skips: a cold read later, never a wrong answer. No test mixes them, so
  nothing reveals it.
- **A cache too small for the metadata** evicts what it warmed. Criterion 1 assumes room;
  `fetched` against class residency is how an operator sees it.
- **Abuse.** Repeated warms cost one billed HEAD read each.

## Tasks

- **M21.1** — `Engine::warm` and its engine tests (criteria 1–7).
- **M21.2** — the endpoint, its server tests, `deploy.md`, the parity row (criteria 8–9).
- **M21.3** — ledger, sweep, gates, close (criterion 10).
