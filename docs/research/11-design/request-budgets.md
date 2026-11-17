# Request budgets

**The one place a test reads a request count from** (M278, lesson 2 of
[lessons-from-hsearch-opensearch.md](../../comparisons/lessons-from-hsearch-opensearch.md)).
A test calls `pstore_testkit::budget::spend("<name>", measured)`, or `spend_per` for a row
counted per tenant, and never repeats the number.

- **`exact`** fails above the count **and below it**. A change that saves a request fails
  until this table is lowered, so the saving is recorded and then held: a later change that
  spends it back fails too.
- **`max`** fails only above. Used where the measurement legitimately varies: a depth the
  counter merges, or a share that depends on timing.

`scripts/check-budgets.py` checks that every row is spent by at least one test and every name
a test spends is a row. Raising a row is a cost decision, made here in review where it shows,
not in the test. The service objectives these serve are in [slos.md](slos.md).

| Budget | Bound | Requests | What |
|---|---|---|---|
| `durable_write_first_puts` | exact | 2 | a tenant's first durable write on a process: the bundle and the lane's registration (per tenant) |
| `durable_write_first_reads` | exact | 11 | the same write's reads: the door's HEAD, then the flush's lane set, HEAD and one window of 8 tail probes |
| `durable_write_puts` | exact | 1 | every later durable write: its bundle (per tenant) |
| `durable_write_reads` | exact | 0 | every later durable write reads nothing |
| `batched_write_first_reads` | exact | 1 | a process's first batched write reads HEAD once, for the schema |
| `batched_write_requests` | exact | 0 | a batched write's writes and lists, and every request of a later one, until its flush bound |
| `fold_puts` | exact | 2 | one tenant's fold of one index: its segment and the HEAD CAS (per tenant) |
| `concurrent_durable_puts_one_tenant` | max | 3 | 8 concurrent durable writes to one tenant, every request taking 20 ms |
| `memtable_query_reads` | exact | 1 | a query answered from the memtable reads HEAD and nothing else |
| `engine_first_flush_requests` | exact | 12 | an engine's first flush: lane registration, HEAD, a probe window, the bundle |
| `engine_first_flush_depth` | max | 4 | its sequential depth |
| `engine_flush_requests` | exact | 1 | every later flush: the bundle |
| `engine_flush_depth` | max | 1 | its depth |
| `cold_query_depth` | exact | 3 | a cold vector query from HEAD: HEAD, footer and centroids, the probes |
| `api_query_depth` | exact | 4 | a query through the API, ranked and fetched, at 1, 4 or 8 segments (D-34 as M47 restated it) |
