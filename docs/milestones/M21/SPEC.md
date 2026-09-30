# M21 — `hint_cache_warm`: warm an index's metadata before a burst

**Serves:** D-44 (warm cache classes 1–4 only, never bulk), OQ-57's answer, and the warm-up
API of [`load-and-hotspots.md`](../../research/04-cluster/load-and-hotspots.md) item 4 and
[`affinity-and-coldstart.md`](../../research/07-caching/affinity-and-coldstart.md) § Warm-cache API.

## What is true today

- M20 gave the server a read cache that survives a restart (`PSTORE_CACHE_DIR`). It fills only
  on demand: the first query on each segment pays its open round in blob requests.
- A query's **open round** (`pstore_query::run::open`) reads, per segment and in one round: the
  footer and index section (`Class::Meta`), the delete vector, the centroid table, and the
  sparse and text dictionaries its legs need (`Class::Pinned`). Everything after it is bulk.
- [`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md) lists
  `hint_cache_warm` as a gap, assigned to M21.
- `pstore-server` has no placement routing: `pstore-cluster` is not wired into it. A request
  reaches whichever process the client or load balancer picked.

## Delta

**Engine.** `Engine::warm(index) -> Result<Warmed, EngineError>`:

1. Reads HEAD **fresh**, as a `strong` query does. There is no engine error for an unknown
   index today (the server decides from what a read found), so `Warmed` says whether HEAD
   names the index at all: a segment list or a schema for it.
2. Round 2, for every segment HEAD names, together: `Segment::open` (footer, and the index
   section's range read when it is too large for the suffix) and the delete vector, if HEAD
   names one.
3. Round 3, from what each footer says, together:
   - the centroid table, **only if** the segment has a dense field and at least
     `exact_scan_threshold` rows (this engine's own cluster parameter). Below it none was
     written (D-10); a query learns that from a 404 (BACKLOG row 46), and a warm must not
     pay one per small segment;
   - the sparse dictionary, only if a field's layout names `SparsePostings`;
   - the text dictionary, only if `has_text()`.
4. `Warmed { known, segments, fetched }`: whether HEAD names the index, the segments opened,
   and the sidecars fetched.

Every read goes through the engine's store with the class a query uses, so it lands in the
cache exactly as that query's would. **No bulk read, ever** (D-44): not the data section, not
postings, not `IndexRows`. The unfolded memtable and bundles are not touched: they are not
cached, and a query reads them fresh anyway.

**Server.** `POST /v1/indexes/{index}/warm`, empty body, the tenant header as every endpoint:

- `200 {"segments": n, "fetched": m, "meta": {"cost": {...}}}` with the same `cost` block a
  query reports, from the tenant's accounting. **It is billed**, as the corpus says to charge
  for it: a warm spends blob requests and cache space.
- `404 index_not_found` when HEAD does not name the index. An index that exists only in
  unfolded writes is `200` with `segments: 0`: it has nothing a cache holds.
- `409 no_read_cache` when the server was started without `PSTORE_CACHE_DIR`: a warm there
  would fetch and discard, spending the tenant's requests for nothing.
- **Synchronous.** It is metadata only, three rounds deep, so it returns when warm rather
  than accepting a job nothing tracks. turbopuffer's is asynchronous because it warms bulk.

**Docs.** `deploy.md`: a warm warms **the process that serves it**, and nothing routes a
tenant to that process, so a client warms each process it will query (or every one behind its
load balancer). `turbopuffer-api-parity.md`'s row points here.

**Not changed:** the query path, the cache, the format, D-50's scan handling, any other
endpoint. No `classes` parameter: D-44 fixes the classes, and `vectors` is refused by design,
not offered. No warming of a whole tenant in one call. No placement or forwarding.

## Acceptance criteria

1. **A warmed index's queries open without a request.** After `warm`, a dense, a sparse, a
   text, a filtered and a `rank_by` query on the index issue **0** `Meta` and **0** `Pinned`
   reads to the store beneath the cache, on a cache with room for them.
2. **A warm admits no bulk.** During `warm`, **0** `Bulk` or unclassed ranged reads reach the
   store, and the cache's bulk residency is unchanged.
3. **A warm reads only what exists.** On an index of segments below `exact_scan_threshold`,
   with no sparse and no text field, a warm issues exactly **1 + k** reads for k segments
   without delete vectors: HEAD and the k footers. No centroid, dictionary or 404.
4. **Each sidecar that exists is warmed.** An index with a clustered segment, a sparse field,
   a text field and a delete vector: `fetched` counts each, and criterion 1 holds for it.
5. **Depth is bounded.** A warm's sequential blob depth is **≤ 3**, asserted by the testkit's
   depth-counting store.
6. **A second warm is HEAD alone.** Warming a warm index costs exactly **1** read.
7. **It survives a restart.** Warm, close the cache, reopen it: criterion 1 still holds.
8. **The endpoint.** `200` with `segments`, `fetched` and a `cost` equal to the tenant's
   accounted reads during the call; `404 index_not_found`; `409 no_read_cache` on an `Api`
   without a cache, having issued **0** requests.
9. **Nothing else changes.** Every existing test of `pstore-server`, `pstore-engine` and
   `pstore-cache` passes unchanged.
10. **Gates.** `./scripts/mutants.sh` over the diff, and `./scripts/gates.sh`, green.

## Test plan

Engine tests in `crates/pstore-engine/tests/warm.rs`, over a store that records each read's
class (as `pstore-blob/tests/scanning.rs`'s `Classes`), beneath a `Caching` with a core.

| # | Test (fails first on a stub `warm` that reads HEAD only) | Mutation it catches |
|---|---|---|
| 1 | `a_warmed_index_opens_without_a_request`: each modality, 0 `Meta`/`Pinned` reads after | a sidecar kind skipped; footers not opened; `Pinned` fetched as `Bulk` |
| 2 | `a_warm_admits_no_bulk` | the data section or postings read; `Segment::scan` used to open |
| 3 | `a_warm_reads_only_what_exists`: exact count 1 + k | centroids fetched unconditionally; dictionaries fetched unconditionally; threshold compared the wrong way |
| 4 | `every_sidecar_that_exists_is_warmed`: `fetched` and zero-cost queries | the delete vector skipped; one dictionary kind skipped |
| 5 | `a_warm_is_three_rounds_deep` (`DepthCounting`) | dictionaries fetched one segment at a time; round 3 awaited per segment |
| 6 | `a_second_warm_costs_head_alone` | a warm that bypasses the cache (`get` instead of `get_immutable`) |
| 7 | `a_warm_survives_a_restart` | none new; pins the composition with M20 |

Server tests in `crates/pstore-server/tests/warm.rs`:

| # | Test | Mutation it catches |
|---|---|---|
| 8 | `warm_reports_and_bills`, `warm_of_a_missing_index_is_404`, `warm_without_a_cache_is_409_and_free` | route missing; cost not reported or from the wrong tenant; the 409 check after the HEAD read |
| 9 | the existing suites, unchanged | — |

## RA budget

| Operation | W | Rseq | Rpar | List |
|---|---|---|---|---|
| `warm`, cold | 0 | ≤ 3 | 1 + per segment: footer (+1 if its index section overflows the suffix), + dv, + centroids if clustered, + one per dictionary | 0 |
| `warm`, already warm | 0 | 1 | 1 (HEAD) | 0 |
| every other operation | unchanged | | | |

Requests scale with the segments of the one index named, per explicit call: the same shape
as that index's first query, never with records or elapsed time.

## Risks

- **A warm on one process, a query on another.** Without routing, a warm helps only if the
  client reaches the same process. Revealed by nothing automated; stated in `deploy.md`.
- **Parameters differ between writer and warmer.** A segment built with a lower
  `exact_scan_threshold` than this engine's has centroids the warm skips. The cost is a cold
  centroid read later, never a wrong answer. Revealed by criterion 1 failing if tests mix them.
- **A cache too small for the index's metadata** evicts what it warmed. Criterion 1 holds only
  "on a cache with room"; a warm larger than the Meta or Pinned quota is a sizing question,
  and `fetched` against residency is how an operator sees it.
- **Abuse.** A client can warm repeatedly; each warm after the first is one billed HEAD read.

## Tasks

- **M21.1** — `Engine::warm` and its engine tests (criteria 1–7).
- **M21.2** — `POST /v1/indexes/{index}/warm`, its server tests, `deploy.md`, the parity row
  (criteria 8–9).
- **M21.3** — ledger, sweep, gates, close (criterion 10).
