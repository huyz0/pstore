# M234 verification

How to run the tests: `cargo test -p pstore-engine --test it mask_cache`,
`cargo test -p pstore-query --lib masks`, `cargo test -p pstore-engine --lib storage` and
`cargo test -p pstore-server --test it storage`. The benchmark: `./scripts/columns.sh` and
`./scripts/columns.sh 100000`.

Red runs: the tests name `pstore_query::masks`, `Engine::with_mask_cache`,
`Storage::filter_cache` and `Config::filter_cache_bytes`, which the parent does not have, so
they were red by not compiling. Two were then red on their own mistakes before going green:
the eviction test first sized its budget for three entries, which the one-eighth rule never
holds; the opt-out check first compared bytes across two query vectors, whose hits resolve
different blocks. The mutations below show what each tests.

Mutations were applied one at a time by a script and reverted. 7 of 7 were killed: the key
without the segment; the key without the filter; a hit ignored; the index override ignored;
eviction of the newest entry; the one-eighth size check dropped; the fourth HEAD setting not
decoded. No `cargo mutants` run is claimed.

1. `a_cached_mask_answers_as_a_computed_one`: green. 200 random filters (`Cmp` of every op on
   an int, a string and the id, under `And`, `Or`, `Not`), each asked twice with two vectors
   of a cached engine and an uncached one over three segments with deletes and unfolded rows:
   every answer equal, the second never more bytes, and fewer in total; a filter keeping every
   block reads less the second time; with the index's `filter_cache` overridden off, the same
   query twice reads the same bytes and the cache does not grow.
2. `a_hit_returns_the_mask_held_and_its_key_alone`, `the_least_recently_used_goes_first`,
   `a_large_mask_and_a_zero_budget_keep_nothing`, `bytes_held_never_pass_the_budget`: green.
3. `storage_overrides_round_trip_last` (the fourth setting round-trips each way, code 3 in it
   refused), `the_cluster_default_is_read_from_the_environment` (the budget, `0`, and two
   refusals by name), `an_index_overrides_the_cluster_default_and_says_so` (the override set,
   reported and cleared; a server with no cache reports `false` whatever the override):
   green. ⚠️ `storage_overrides_round_trip_last` refused `0x40` and `0x80` as high bits in
   M231; M234 gives those bits to `filter_cache`, so the test now decodes them and refuses
   `0xc0` (a code of 3 in the new setting) instead.
4. `./scripts/columns.sh`, `provisional` (in-memory store, 1 MiB coalescing gap, one 4-core
   container, medians of 31 runs). A filtered top-10 dense query over one segment: masks
   uncached, built and kept (a miss, a fresh cache each run), and read from the cache (a hit).
   Columnar blocks with statistics (M232's arm):

   | filter | 20,000 rows: uncached / miss / hit (ms) | 100,000 rows (ms) | bytes uncached / hit, 100,000 |
   |---|---|---|---|
   | `category =` (uniform) | 3.18 / 3.17 / 0.57 | 15.08 / 15.00 / 2.92 | 7,502,322 / 942,500 |
   | `customer =` one batch | 0.71 / 0.72 / 0.56 | 3.97 / 3.95 / 2.97 | 2,286,110 / 942,471 |
   | `customer in` 3 batches | 0.77 / 0.77 / 0.56 | 8.63 / 8.53 / 2.92 | 6,988,077 / 942,500 |
   | `sku =` one row | 2.97 / 2.97 / 0.56 | 14.84 / 14.83 / 3.01 | 7,502,352 / 942,530 |
   | `customer =` absent | 0.53 / 0.53 / 0.53 | 4.39 / 4.38 / 2.91 | 2,285,508 / 808,192 |
   | `archived = true` | 0.76 / 0.78 / 0.56 | 3.87 / 4.92 / 2.88 | 1,614,128 / 942,500 |
   | `promo = true` | 2.42 / 2.41 / 0.59 | 11.33 / 11.36 / 2.87 | 7,502,322 / 942,500 |
   | `price < 100` | 2.48 / 2.50 / 0.59 | 11.74 / 11.63 / 2.88 | 7,502,322 / 942,500 |
   | `customer =` AND `price <` | 0.80 / 0.78 / 0.59 | 4.41 / 4.41 / 2.94 | 2,286,110 / 942,471 |

   Row blocks (M230's arm) at 100,000 rows: `category =` 26.8 / 26.8 / 3.04 ms. A miss's
   bytes always equal uncached; a hit's are the vectors and the hits' blocks alone. One miss
   above 10% (`archived = true`, 100,000 rows, 4.92 against 3.87 ms, same bytes) was run
   again: 3.83 / 3.83 ms, noise. Every other miss is within 3% of uncached. Adopted, default
   on (64 MiB), by criterion 4's rule.

Downsides, measured: (a) **memory**: a mask is a byte a row, so a 100,000-row segment's costs
about 100 KB; the 64 MiB default holds about 650 such (segment, filter) pairs, shared by every
tenant on the server. (b) **A miss** formats the filter once a query and inserts once a
segment: within noise above. (c) **Only vector and text queries** use masks; order-bys and
aggregations filter rows inline and gain nothing. (d) **Remote parts** are not cached (a part
reads no HEAD, so cannot see the override). (e) **Rollback** below M234 with a `filter_cache`
override set is refused by the older binary's HEAD decoder (docs/deploy.md). Nothing durable.

Gates, in a cloud container: GATES-PENDING
