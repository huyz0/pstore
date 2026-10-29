# M141 verification

Implemented in `pstore-format` (`Document::footprint`, `Value::footprint`), `pstore-engine`
(the memtable's *pending* count, reservations, `Engine::reserve_buffer`,
`Engine::buffered`, `stamp_footprint`) and `pstore-server` (`write_footprint`,
`reserve_buffer` in `write_documents`, and the two new `RequestLimits` fields). All runs
below are in a 4-core cloud container and every number is `provisional`.

How to run the tests: `cargo test -p pstore-engine --test it buffered::`,
`cargo test -p pstore-server --test it buffered::`,
`cargo test -p pstore-format --test it footprint::`, and the limit unit tests with
`cargo test -p pstore-server --lib ingress::`.

A reservation is the request's estimated footprint, charged under the memtable lock when
it is checked. The request's buffer calls run inside `BufferReservation::scope`, and each
moves its rows' footprint from the reservation into *pending*, so a running request is
counted once; rows past the estimate are charged as rows (trued up), and the guard returns
only what was not converted, on any exit path. The estimate counts a delete's tombstone
and, for an applied write, the `$op` and `$req` marks on every row. The cap is checked
before the flush bound, so a write that cannot fit is refused before any flush is spent.

Red runs. The first suite was written against stubs and failed (.harness/m141-mutations.log
starts from it). After code review round 1, the five new tests below were written against
a stub `scope` that converts nothing, a `stamp_footprint` of 0, the old estimate and the
old check order, and all five failed (.harness/m141-r2-red.log):
`a_reservation_converts_into_the_rows_its_scope_buffers`,
`an_applied_rows_stamp_is_what_stamp_footprint_says`, `a_running_write_is_charged_once`,
`a_full_batched_write_is_refused_before_any_flush` and `deletes_reserve_their_tombstones`.
`concurrent_reservations_admit_one` passed against that stub too, because check and charge
were already one step; its red is the "reservation checked but not charged" mutation.

Mutations, one at a time by script and reverted (.harness/m141-mutations.log), each
running the `buffered::` engine and server tests, the `footprint::` tests and the
`ingress::` unit tests. All ten were killed: a `>=` flush comparison, unfolded not
counted, a reservation checked but not charged, a flush resetting *pending*, an index drop
returning no charge, no pre-flush, no fold request, a string value not counted, a buffer
bound below the flush bound accepted, and an oversized write answered 429. No
`cargo mutants` run is claimed; CI's sweep is the full check.

1. PASS: `batched_rows_flush_at_the_tenant_flush_bound`. On a registered lane a batched
   write that brings *pending* exactly to the flush bound costs no blob request; one more
   row first flushes the earlier rows in exactly one PUT, and every row of both writes is
   visible to queries.
2. PASS: `buffered_rows_refuse_at_the_tenant_bound`,
   `unfolded_bundles_count_against_the_cap_until_folded`. Exactly at the bound succeeds;
   one more is `429 overloaded` with `Retry-After: 1`, costs no request, buffers nothing
   and requests one fold; once that fold commits, the same write succeeds.
3. PASS: `one_tenant_at_its_bound_leaves_another_alone`,
   `concurrent_reservations_admit_one`, `rows_buffered_during_a_flush_stay_charged`,
   `dropping_an_index_returns_its_pending_charge`,
   `dropping_an_index_returns_its_buffered_charge`,
   `a_write_larger_than_the_bound_is_too_large_not_overloaded`,
   `reservations_are_checked_and_charged_in_one_step`,
   `a_reservation_converts_into_the_rows_its_scope_buffers`,
   `a_running_write_is_charged_once`, `a_full_batched_write_is_refused_before_any_flush`,
   `deletes_reserve_their_tombstones`, `an_applied_rows_stamp_is_what_stamp_footprint_says`.
   A second tenant is unaffected while the first is at its bound. Two durable writes that
   each fit alone, started together behind a barrier with the winner's bundle PUT held,
   admit exactly one: the other is a 429 and only the winner's row is visible. Rows
   buffered during a flush's PUT stay charged; dropping an index returns its charge; a
   request larger than the bound is a 413. A write whose PUT is in flight is counted once,
   so a second write that fits beside it is admitted.
4. PASS: `a_failed_preflush_changes_nothing`. With the fault-injecting store refusing the
   PUT, the earlier pending rows, the counts and the write's own rows are as before.
5. PASS: `unsafe_or_malformed_configuration_is_refused`,
   `configuration_lowers_each_limit`, `version_one_defaults_are_the_documented_contract`,
   with `every_value_kind_is_counted` and `a_document_counts_its_id_vectors_and_attributes`
   for the footprint. Zero, above the default, `tenant_buffered_bytes` below
   `tenant_flush_bytes`, and unknown fields are refused.
6. PASS, with the IPv6 exception M127 records. Clippy `--all-targets --all-features -D
   warnings` passed. The workspace suite (all features, both `[::1]` tests skipped: the
   container has no IPv6; they run in CI) passed 2,317 tests with none failing.
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed at 97.19%
   lines / 95.78% regions (.harness/m141-coverage.log). `scripts/recall.sh`,
   `scripts/ndcg.sh` and `scripts/depth.sh` passed (.harness/m141-quality.log). The
   measurement below is `buffered_footprint_against_heap`.

Measurement (the spec's risk: `footprint()` is not RSS). `buffered_footprint_against_heap`
(release, rustc 1.90.0, [measurements/buffered-heap.csv](measurements/buffered-heap.csv))
buffers batched rows below the flush bound and compares the heap the process gained with
the footprint *pending* gained. The heap held 58x the footprint for 2-dimension rows with
no attributes, 34x with two attributes, 5.1x and 6.0x at 128 dimensions, and 4.3x at 768.
The gap is per-row overhead (maps, strings and allocator rounding, about 1 KB a row),
so `tenant_buffered_bytes` bounds rows, not memory: at the 64 MiB default a tenant of tiny
rows can hold several GB. That is the honest reading of D-56 for this slice, and a
process-wide memory bound remains later work.

Code review: round 1 (an independent agent, read-only) failed with one major: the
reservation was never converted into rows, so a running request was counted twice, and
the estimate missed tombstones and applied stamps. Its three minors (the cap checked after
a flush, no concurrent test, vector field names counted against the spec) were fixed with
it. Round 2 passed; its one minor (the measurement files untracked) is fixed by this commit.
