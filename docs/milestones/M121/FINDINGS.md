# M121 — the second review's findings, and what became of each

Three read-only reviewers, one each for `pstore-engine` with `pstore-cache`,
`pstore-index` with `pstore-format`, and `pstore-query` with `pstore-server` and
`pstore-meter`. Each was given M119–M120's diff (`e7ea269..HEAD`) and the paths it touches,
and asked for correctness defects and cost: requests, bytes, depth, and the copies left on a
hot path. Every finding below was traced in the code before it was acted on. None of the
three found a wrong answer.

| # | Crate | Finding | Outcome |
|---|---|---|---|
| 1 | format | a multi-field `scan` reads the first field's vectors twice, in two rounds | **Fixed** (Delta 1). Two reviewers found it independently. `a_multi_field_scan_reads_each_fields_vectors_once` was red at 480 bytes for 240; the round test was red at 2 rounds for 1 |
| 2 | format, blob | every pin check sends `head` and `get_tag`, two billed HEADs for one object's metadata | **Fixed** (Delta 2): `BlobStore::stat`. `a_stat_is_one_head` was red at 2 HEADs; `a_column_check_is_one_request_an_object` was red at 6 requests for 3 |
| 3 | format | an external read checks **every** object of a column, including those holding none of the rows asked | **Kept**. `a_changed_fragment_is_noticed` (M87) requires a read of the first file to notice that the last file changed: the column is pinned as a whole. Checking only the objects read would weaken that test, so it is the owner's call. The check's cost is halved by finding 2 |
| 4 | cache | every missed range is copied before the class and quota are judged, so a scan's copy is thrown away | **Fixed** (Delta 3). No test can see a copy dropped. `a_cached_range_owns_its_bytes` still passes on the copy kept |
| 5 | cache | concurrent batched misses each reach the store | **Fixed** (Delta 4). `concurrent_batched_misses_issue_one_request_a_range` was red at 24 requests for 3. Its first draft passed against the unfixed code: the solo call's spare permits let the first racer through alone. Draining them made the race run |
| 6 | format | `rows_at` builds every row of a block to return the few asked | **Fixed** (Delta 5). `rows_at_builds_only_the_rows_asked` went red with a skipped row's attributes not read through |
| 7 | format, index | the term dictionary, and a sparse dictionary's `dims`, are decoded again on every query | **Deferred**. Decoding lazily would lose the up-front refusal of a truncated or unsorted dictionary (`TermDict::decode`'s own warning). The fix is a decoded-dictionary cache per segment, with its own memory budget: a design of its own |
| 8 | index | a clustered search with replicated rows sorts every candidate to dedupe it, and allocates a row-sized `Vec<bool>` | **Deferred**. CPU only, on the replicated path, and correct as written |
| 9 | index | BM25's accumulator zero-fills `5 × rows` bytes a query | **Rejected**. `vec![0; n]` is a zeroed allocation: at this size the OS hands out zero pages lazily, and only the pages a posting touches are ever written |
| 10 | engine | a fresh-view hit clones the unfolded rows | **Deferred**. CPU only. Sharing them needs `Document` behind an `Arc` through the query path |
| 11 | engine | the per-index fresh map keeps one sealed segment per index until a miss | **Rejected**. Bounded by the memtable times a small constant, because every live entry shares one generation. Not a leak |
| 12 | server | a branch body is parsed into a tree to count its keys; a refused single query is parsed three times | **Deferred**. CPU only, on small or refused bodies |

## The deliberate breaks

Each fix was broken on purpose once and the test named above went red:

| Delta | Break | Red |
|---|---|---|
| 1 | the per-block slices asked again; the field reads awaited after the blocks | both `vector_fields` tests, before the fix |
| 2 | `ObjectStoreBackend::stat` left to the default; the column check as two calls | `a_stat_is_one_head`; `a_column_check_is_one_request_an_object` |
| 4 | no claim taken | `concurrent_batched_misses_issue_one_request_a_range`, before the fix |
| 5 | a skipped row's attributes not read through | `rows_at_builds_only_the_rows_asked` |
