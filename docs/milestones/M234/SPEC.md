# M234: a per-segment filter-mask cache

## Serves
Item 5 of [the column-storage adoption list](../../comparisons/column-storage-adoption.md).
M9b's filter masks. D-34 unchanged.

## Delta
Today a filtered vector or text query builds, for every segment a leg runs over, the
segment's mask (`run.rs` `mask`): it fetches every block the zone maps and the equality
section keep, decodes the filter's columns and tests each row. A dashboard or a paginated
query that repeats its filter with another vector pays it again every time.

After, a server keeps a **filter-mask cache** in memory: `(segment key, the filter's
canonical text)` to the mask. A segment is immutable and its key is never reused, and a mask
holds no deletes (they are applied after it), so an entry can never be stale: no
invalidation, nothing durable, and an answer never depends on it.
- Read on the query's own task, as M165's plan is: `pstore_query::masks::scope` runs a query
  with a cache, and `candidates` looks each segment's mask up before spawning its task. A hit
  costs no block GET and no decode; a miss computes and inserts.
- Bounded in bytes (a mask is a byte a row, plus its key), least recently used evicted
  first; a mask larger than an eighth of the budget is not kept. Shared by every tenant on the
  server, each key naming its tenant's segment.
- Remote parts (M54's peers) are not cached: a part reads no HEAD, so its server cannot
  see the index's override.

**A setting**, by the "a feature with a measured downside is a setting" non-negotiable
(memory is the downside): the cluster default `PSTORE_FILTER_CACHE_BYTES` (64 MiB if unset;
`0` turns it off), and the index override `filter_cache` in M231's storage settings (HEAD's
per-index byte gains a fourth 2-bit setting; `PATCH /v1/indexes/{index}`
`{"storage": {"filter_cache": false}}`). Default on only if criterion 4 holds.

Unchanged: order-bys and aggregations (they filter rows inline, no mask); a query's depth;
an answer.

## Acceptance criteria
1. Equal answers: over seeded segments with deletes, with the cache on and off, 200 random
   filtered queries (each run twice, the second with another vector) answer the same; the
   second run of a filter reads fewer bytes when on, and the same when off or overridden off.
2. The cache: a hit returns the mask inserted; least recently used goes first past the
   budget; a mask over an eighth of the budget is not kept; a budget of 0 keeps nothing;
   bytes held never exceed the budget.
3. The setting: the environment's value (and its refusals by name); the override wins over
   the default for the index, survives a restart, and a HEAD without it keeps its bytes; an
   unknown 2-bit code is refused.
4. `./scripts/columns.sh` (`provisional`): each filter's mask time on a miss (computed and
   inserted) and a hit, against M233's uncached mask. Default on only if no miss costs more
   than 10% over uncached, and hits are faster.

## Test plan
- 1: `a_cached_mask_answers_as_a_computed_one` (engine `tests/it/mask_cache.rs`). Kills: a key
  without the segment; a key without the filter; a hit ignored; the override ignored.
- 2: unit tests in `masks.rs`. Kills: eviction of the newest; the size check dropped.
- 3: `storage_overrides_round_trip_last` extended; server `tests/it/storage.rs` extended.
- 4: the script.

## RA budget
A hit removes a segment's block plan from the mask's round; that round runs beside the
legs', so depth is unchanged. Requests only fall.

## Risks
- Memory: bounded by the budget, never by tenants or indexes.
- A filter repeated rarely only costs a key's formatting and an insert per segment.
- Rollback below M234: a pre-M234 server refuses a HEAD whose override byte uses the fourth
  setting (as M231's own rollback note says of any override): clear `filter_cache` overrides
  first.

## Tasks
- M234.1: `masks.rs` cache, the `candidates` lookup, engine and server plumbing, setting,
  OpenAPI and clients, tests, bench arm, VERIFIED.md.
