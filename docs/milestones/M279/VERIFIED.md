# M279 verification

The spec was not sent to a separate reviewer before code; the PR's review covers it.

1. `cargo test -p pstore-testkit` (all 94 tests in its suites green, after the changes below):
   `the_memory_store_conforms` and `every_decorator_conforms` pass with 13 probes.
   `cargo test -p pstore-blob --all-features --test it object_store`:
   `the_conformance_suite_runs_through_the_adapter` passes, so `object_store`'s `InMemory`
   answers all three as the spec requires.
2. `a_backend_that_admits_two_racing_winners_does_not_fence` passes: exactly
   `concurrent_create_one_winner` and `concurrent_cas_one_winner` diverge ("8 of 8 concurrent
   conditional writes succeeded"), and the profile refuses durable writes.
3. `a_backend_that_errs_on_a_deleted_objects_cas_does_not_fence` passes: exactly
   `cas_after_delete_is_lost` diverges.
4. `the_suite_detects_each_defect_a_real_backend_actually_has` passes. Its first run with the
   new probes failed: `IgnoresCreateIfAbsent` also tripped `concurrent_create_one_winner`, and
   `IgnoresDeletes` also tripped `cas_after_delete_is_lost`. Both are the defect showing where
   it should, so each case now names its exact set of probes, still asserted equal.
5. `conformance_costs_a_bounded_number_of_requests` passes against `conformance_requests`,
   exact 48. Before, it asserted `< 40`; the first run with the probes reported 48, and the
   row records it. Exact is stronger than the old bound in both directions.
   `hostile::the_suite_survives_a_backend_that_fails_everything` now counts 13 probes.
6. `./scripts/conformance.sh --check` against `dev/docker-compose.yml`'s emulators, after
   regenerating the matrix: "ok the matrix matches a fresh run". RustFS and Azurite pass all
   three race probes. fake-gcs-server was already UNREACHABLE in the checked-in matrix
   (`invalid uploadType`), and still is: not a change.
