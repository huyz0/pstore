# M213 verification

How to run the tests: `cargo test -p pstore-format --test it -- footprint`,
`cargo test -p pstore-engine --test it -- buffered::`,
`cargo test -p pstore-server --test it -- buffered::`,
`cargo test -p pstore-server --lib -- footprint` and
`cargo test -p pstore-server --test buffered_memory -- --nocapture`. All numbers here are
from a 4-core cloud container and are `provisional`.

Red runs: on the parent, `buffered_rows_are_charged_at_least_the_heap_they_hold` failed for
every shape (a tiny row was charged about 3% of its heap), `a_flushed_row_keeps_its_held_charge_until_folded`
failed (`unfolded` was the bundle's size), and `a_document_is_charged_the_memory_it_holds`
failed (the old sums). With the new rule but the server's estimate unchanged, six of M141's
twelve server `buffered::` tests failed (a write exactly at the bound was refused 413, and a
reservation was left over while its rows were pending), which is what criterion 6 fixes.
`each_operations_estimate_is_the_row_the_engine_buffers` did not compile before the
estimate functions existed.

Mutations were applied one at a time by a script and reverted, against the five commands
above. 13 were run; 11 killed, 2 survived and both are equivalent. No `cargo mutants` run is
claimed.
- Killed: the slot not counted; the attribute map's node not counted; `len` for `capacity`
  on dense vectors; the 16 bytes an allocation dropped; a large map counted as one leaf; an
  attribute value's heap not counted; a flushed batch charged nothing; the stamp's map leaf
  dropped; a tombstone estimated as its id and 8 bytes (M141's rule); parsed rows not
  shrunk; the patch-by-filter estimate dropped.
- Survived, equivalent: `held` not pruned at a fold (`buffered()` sums only batches at or
  past the pruned watermark, so the retain only keeps the map from growing; `stamps` has
  the same retain); and the patch-by-filter estimate keeping the patch's `$op` (overwritten
  by the by-filter kind, so the line was removed).

1. `buffered_rows_are_charged_at_least_the_heap_they_hold`: green. Charged per heap, in a
   debug build ([measurements/buffered-memory.csv](measurements/buffered-memory.csv)):
   tiny 1.18, 64 dimensions 1.14, 1,024 dimensions 1.03, 3 attributes 1.15, 128 dimensions
   1.11, 768 dimensions 1.03, 20 attributes 1.34, string and number arrays 1.16, sparse
   1.24. All inside 1.0 to 2.5. Killed the slot, map node, overhead, large-map and value
   heap mutations.
2. `a_flushed_row_keeps_its_held_charge_until_folded`: green; after a flush `unfolded`
   equals the former `pending`, and a fold returns it to 0. Killed "flushed batch charged
   nothing".
3. `a_document_is_charged_the_memory_it_holds`: green, computing the expected sum from
   `size_of` and capacities for dense, multi-vector, sparse, every value type and nested
   arrays. Killed "len for capacity".
4. M141's tests that use `footprint()` as their oracle pass unedited: engine
   `buffering_charges_each_row_and_a_flush_moves_exactly_its_rows`,
   `rows_buffered_during_a_flush_stay_charged`, `dropping_an_index_returns_its_pending_charge`,
   and server `a_running_write_is_charged_once`, `a_write_exactly_the_bound_fits`,
   `batched_rows_flush_at_the_tenant_flush_bound`, `buffered_rows_refuse_at_the_tenant_bound`
   and `one_tenant_at_its_bound_leaves_another_alone`. Changed because they wrote the old
   rule as a literal: `footprint.rs` (criterion 3 replaces it);
   `an_applied_rows_stamp_is_what_stamp_footprint_says` (the stamp is now a map leaf and
   three allocations); `a_reservation_converts_into_the_rows_its_scope_buffers` (a cap of
   1,000 bytes became ten rows' footprint, as one row is now about 830 bytes);
   `deletes_reserve_their_tombstones` (a tombstone is `delete_footprint`, not its id and 8);
   and the unit test `a_writes_footprint_counts_every_operation_once` (each part is now the
   engine's encoded row, still asserted distinct and summed once).
5. `buffered_footprint_against_heap` (release, rustc 1.90.0,
   [measurements/buffered-heap.csv](measurements/buffered-heap.csv)) now sums allocated less
   deallocated. Heap per charge: 0.84 (2 dimensions), 0.87 (with attributes), 0.90 and 0.91
   (128), 0.97 (768); so the charge is 1.03 to 1.19 times the heap. M141's VERIFIED carries
   a correction banner for its double-counted ratios. `provisional`.
6. `each_operations_estimate_is_the_row_the_engine_buffers`: green; a delete, a patch, a
   delete by filter and a patch by filter, each buffered alone, are charged exactly their
   `delete_footprint`, `patch_footprint` and `by_filter_footprint`, all four distinct, and a
   refused patch estimates 0. `a_write_exactly_the_bound_fits` covers a parsed row at the
   bound. Killed "tombstone as id and 8", "parsed rows not shrunk" and "patch-by-filter
   estimate dropped".
7. `flushed_rows_at_the_flush_bound_make_their_tenant_due_for_a_fold`: added after code
   review found held bytes could fill the cap while bundles stayed below the fold size.
   Seen red first (two flushed rows at the bound, folded 0), green after; the mutation `>`
   for `>=` at the bound was killed.

Code review: one round, an independent agent given the specs and the diff without the
author's reasoning. Pass, with two majors and two minors, all fixed: the migration note
advised raising a bound that can only be lowered; held bytes could fill the cap below the
fold trigger (criterion 7); `stamp_footprint` was documented as a bound and is not (now an
estimate, and a risk in the spec); and the external-source query reserved nothing (M212's
criterion 7).
