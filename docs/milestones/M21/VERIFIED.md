# M21 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0). No
number here is a latency; every count is a request count, exact on any machine.

Engine tests: `cargo test -p pstore-engine --test warm`. Server tests: `cargo test -p
pstore-server --test warm`.

**Observed red.**
- Six of eight engine tests failed on a stub `warm` that read HEAD only.
- Four of five server tests failed with no route.
- The code-review tests each failed before their fix.
- Tests that pass on a stub guard a mutation instead. Each mutation named below was applied
  by hand and fails the test named with it.

1. **A warmed index opens with no Meta read.** `a_warmed_index_opens_without_a_meta_read`
   covers a small and a full index, all five query modalities, and an unwarmed twin's second
   run. It fails with the footers, the delete vector, centroids or either dictionary skipped,
   and with sidecars fetched as `Bulk` or `Meta`. Criterion 1's wording was amended, as the
   spec says.
2. **A warm admits no bulk.** `a_warm_admits_no_bulk` fails with sidecars fetched as `Bulk`.
   `sidecars_are_cached_as_pinned_and_footers_as_meta` came from code review.
3. **A warm reads only what exists.** `a_warm_reads_only_what_exists` fails with centroids or
   dictionaries fetched unconditionally, and with existence taken from HEAD alone.
   - `an_index_without_dense_vectors_has_no_centroids_to_ask_for` fails with `dims > 0`
     dropped.
   - `a_sidecar_the_writer_did_not_write_is_no_error` fails with a 404 treated as an error.
4. **Each sidecar that exists is warmed.** `every_sidecar_that_exists_is_warmed`:
   `fetched == 6` and 9 reads. It fails with `>=` as `>`, the delete vector skipped, or
   either dictionary skipped.
   - `an_unreadable_delete_vector_fails_the_warm` came from code review. It fails with the
     delete vector not required.
5. **Depth ≤ 3.** `a_warm_is_three_rounds_deep` (3 segments): a serial loop over segments,
   applied by hand, is 7 rounds deep.
6. **A second warm costs 1 read.** `a_second_warm_costs_head_alone` fails with `get` for
   `get_immutable`.
7. **It survives a restart.** `a_warm_survives_a_restart`, on a disk tier. It fails with the
   sidecars skipped or fetched as `Bulk`.
8. **The endpoint.** `warm_reports_and_bills`, `warm_of_an_unfolded_index_is_empty`,
   `warm_of_a_missing_index_is_404`, `warm_without_a_cache_is_409_and_free`, and
   `a_warm_is_billed_to_its_own_tenant` (redundant; code review, minor).
   - Eight hand mutations each fail one of these: the route, each refusal, the 409 after a
     read, the cost's tenant and window, and the two counts.
   - Existence follows a query's: `an_index_that_does_not_exist_is_not_known`, and
     `an_index_another_process_folded_and_dropped_is_not_known` from code review. The latter
     fails without the prune.
9. **Nothing else changes.** `cargo test -p pstore-engine -p pstore-server` passes, every
   existing test unchanged; `pstore-cache` is untouched by the diff.
10. **Gates** — NOT-RUN yet: the sweep over `715c74f..785a45e`'s source is in progress.

Spec review took two rounds: round 1 blocked on two blockers and four majors, and round 2
approved. Code review took two: round 1 passed with one major (existence without the prune),
and round 2 passed.
