# M169 verification

Implemented in `crates/pstore-engine/src/index_limit.rs` (the count, the checks, the door's
cached names), the five call sites in `lib.rs` (create, branch, the write door), `replica.rs`
(replication and copy create) and `external.rs` (register, copy, and a register's lost-CAS
retry), and `crates/pstore-server/src/lib.rs` (`PSTORE_INDEXES_PER_TENANT`, `limit_indexes`,
`409 index_limit`). The default, 10,000, is the owner's choice (decision card, 2026-11-03). All
runs are in a 4-core cloud container, and every number is `provisional`.

How to run: `cargo test -p pstore-engine --test it index_limit::`,
`cargo test -p pstore-server --test it -- index_limit:: deploy::indexes_per_tenant index_scale::`
and `scripts/index-scale.sh`.

Red runs: the engine tests ran with both checks stubbed to admit (.harness/m169-red-engine.log):
all five failed. The server tests ran with the limit not applied, the error arm removed and a
default of 9,999 (.harness/m169-red-server.log): both failed.

Suite: 2,647 passed and 0 failed, with the two IPv6 tests skipped because the container has no
IPv6 (.harness/m169-suite.log). fmt and clippy pass (.harness/m169-clippy.log).
Coverage, `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` over the shipping
crates: lines 97.33%, regions 95.97%, functions 97.01%; `index_limit.rs` regions 98.52%
(.harness/m169-coverage.log).

Mutations were applied one at a time by script and reverted, each running the tests that claim
it (.harness/m169-mutations.log). The script reports a pattern that does not match once, or a
compile error, as invalid rather than as a kill. All 18 were killed: the check removed; `>=` as
`>`; unfolded names not counted; a counted name treated as new; a process's own unfolded name
treated as new; the door refusing from its cache alone; the door reading on every write; the
door unchecked; externals or replications left out of the count; create, branch, replication,
external register and external copy each unchecked; the server's error arm removed; the
server's limit not applied; the default changed. Not tested: the limit re-checked when an
external register loses its CAS, which no test makes lose. No `cargo mutants` run is claimed.

1. PASS: `a_create_past_the_limit_is_refused`: at three indexes a create is `IndexLimit` and
   HEAD's epoch and names are unchanged; a create whose first attempt read two indexes, and
   lost its CAS to a third's create, is refused on the retry; after a drop it succeeds.
2. PASS: `branch_replication_and_external_respect_the_limit`: at two (an index and an
   external index), branch, replication, copy, external register and external copy are each
   `IndexLimit` and the store's write count is unchanged; at six, the first four succeed and
   the external copy is refused as the sixth name.
3. PASS: `a_write_past_the_limit_is_refused`: two folded and one buffered name refuse a
   fourth; writes to the held names are accepted, folded or not.
4. PASS: `a_stale_door_rereads_before_refusing`: after another engine drops an index, a new
   name is admitted after exactly one read; a refusal costs exactly one read; a write to a
   known name reads nothing.
5. PASS: `the_fold_never_refuses_an_admitted_write`: two engines each admit a new name at one
   below the limit of two; both fold, HEAD names three, and a new name is then refused by a
   fresh engine and by a writer.
6. PASS: `indexes_per_tenant_parses` (10,000 unset, `0`, `-1` refused naming the variable) and
   `a_refusal_is_409` (a write and a create answer `409 index_limit` naming the limit; a held
   index and another tenant are written).
7. PASS: `thousands_of_indexes_in_one_tenant` by `scripts/index-scale.sh`
   (.harness/m169-index-scale.log), release, MemoryStore:

   | K | HEAD | per index | fresh query reads | one fold reads / writes | query all K |
   |---|---|---|---|---|---|
   | 50 | 11,793 B | 235 B | 4 | 13 / 4 | 14 ms |
   | 1,000 | 226,493 B | 226 B | 4 | 13 / 4 | 1.7 s |
   | 5,000 | 1,130,493 B | 226 B | 4 | 13 / 4 | 51.3 s |

   Every write was 200, every query answered only its own index's rows, lists were 0, and the
   limit at K refused a new name `409 index_limit` and admitted a held one. Requests do not
   grow with K; bytes do: at 5,000 every open reads a 1.1 MB HEAD, about 10 ms of a query in
   this container. ⚠️ 226 B per index is above the 106-195 B M6i measured, because these
   names and schemas differ; the 256 B bound holds with 12% to spare. The suite's
   `request_counts_do_not_grow_with_indexes` asserts the same at 2 and 40.
8. PASS for fmt, clippy, suite and the gate above; coverage as stated; mutation by the script
   above, not by `cargo mutants`.

Code review (`scripts/review.sh context m169`): round 1 passed with two minors. One was a false
sentence: `docs/deploy.md` said the overshoot was across servers only, but concurrent writes on
one server can overshoot too, because the door's check and the buffering are not one step. The
sentence is corrected. Left: a refusal's HEAD re-read does not prune this process's unfolded rows
to that HEAD, so a process holding durable rows that another process folded and then dropped can
count that name until its next prune.
