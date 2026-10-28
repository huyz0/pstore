# M147 verification

Implemented in `crates/pstore-engine/src/lib.rs` (`create_index` and its interference hook,
`EngineError::IndexExists`, `Answer::named`, the `named` field of a query scope, and
existence at `warm`) and `crates/pstore-server/src/lib.rs` (the `PUT /v1/indexes/{index}`
route, `CreateRequest`, the 409 mapping, and the relevance query's 404 now read from
`named`). All runs are in a 4-core cloud container, and every number is `provisional`.

How to run: `cargo test -p pstore-server --test it create::` and
`cargo test -p pstore-engine --test it create::`.

Red run (.harness/m147-red.log): the six API tests were written first and all failed
against the tree before the change (the route answered 405). The engine tests were written
after the engine API they call existed. Their red is the mutations below.

Mutations, applied one at a time by script and reverted, each running both `create::`
modules (.harness/m147-mutations.log, .harness/m147-mutations-r2.log). 20 were killed: each
of the four existence sites left at their old rule; the as_of empty answer naming the index;
the create without `remember_schemas` or without `record_commit`; each existence guard
removed (indexes, rejects, quarantine, external, held rows, replica, fencing); the schema
committed with the default metric; the `dims` bounds moved or removed; unknown fields
admitted; and `index_exists` answered as 400. The first sweep left five guards alive, and
`a_create_refuses_each_kind_of_name_head_holds` and `a_bounded_read_after_a_create_sees_it`
were added for them. One survivor was equivalent: past the empty return in the `as_of`
answer, the targets are that epoch's segments, so the index is named. It is now the
literal `true`. No `cargo mutants` run is claimed.

1. PASS: `created_index_survives_restart`, `a_created_index_exists_on_every_read`,
   `an_index_emptied_by_deletes_and_compaction_exists_on_every_read`. List, summary (0
   segments, 0 documents, the declared dims and metric), vector query, `rank_by`,
   aggregate (a count of 0) and `warm` all treat the index as existing and empty, from the creating
   `Api` and from a second one over the same store. An index emptied by deletes and a
   compaction of two segments does the same on the engine. A query `as_of` the creation
   epoch is 404.
2. PASS: `create_refusals_touch_nothing`, `a_create_refuses_what_exists_and_writes_nothing`,
   `a_create_refuses_each_kind_of_name_head_holds`, `a_backend_that_cannot_fence_refuses_a_create`.
   - Through the API, each of these is refused with its status and code and the PUT
     counter is unchanged: a folded name, a name held unfolded, a created name, `dims`
     0, `dims` past `array_elements`, `dims` negative, `dims` missing, an unknown metric,
     an unknown field, a non-object body, and the name `..`.
   - On the engine, rejects, quarantine and external names are `IndexExists`, a replica's
     name is `ReadOnly`, and an invalid name, a zero width or an overflowing width is
     `Refused`.
   - A store that cannot fence is `BackendCannotFence` with no request.
3. PASS: `concurrent_creates_have_one_winner`. The first create's commit loses to a create
   that lands in between. Its retry is `IndexExists`, and HEAD holds the winner's
   euclidean schema with 4 stored components.
4. PASS: `first_write_follows_the_declared_schema`. In the same `Api` with no read between,
   a write of another width, a write omitting the cosine metric and one declaring the
   french analyzer are each `400 schema_conflict`, and the store's PUT count is unchanged. A cosine write then folds into
   one segment, and its query's `$dist` is cosine's 0 and 1.
5. PASS: `a_fold_keeps_an_empty_index`.
6. PASS: `an_empty_index_drops_and_its_name_is_reused`.
7. PASS: `create_costs_one_read_and_one_cas`. A HEAD GET and a CAS PUT, no LIST, and
   `cost` reports both.
8. PASS, with the IPv6 exception M127 records.
   - `cargo clippy --all-targets --all-features -- -D warnings` passed.
   - The workspace suite passed 2,339 tests with none failing. It ran with all features
     and both `[::1]` tests skipped: the container has no IPv6, and those tests run in CI.
   - `./scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed at
     97.20% lines and 95.78% regions (.harness/m147-coverage.log).
   - `scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed
     (.harness/m147-quality.log).

Code review: see the commit message.
