# M279: concurrent one-winner probes on every backend

## Serves
D-100 (capabilities are measured, not declared) and D-99, through lesson 3 of
[lessons-from-hsearch-opensearch.md](../../comparisons/lessons-from-hsearch-opensearch.md).

## Delta
1. **Three probes** in `pstore_testkit::conformance`, so they run wherever the suite runs
   (the memory store, every decorator, the `object_store` adapter, and the emulators through
   `scripts/conformance.sh`):
   - `concurrent_create_one_winner`: `RACERS` (8) create-if-absent writes to one key at once;
   - `concurrent_cas_one_winner`: 8 compare-and-swaps on one observed tag at once;
   - `cas_after_delete_is_lost`: a compare-and-swap on a tag whose object was deleted.
   A race passes only with exactly one winner, every loser a conflict (`Lost`, or a 409's
   `Contended`), and the object holding the winner's body.
2. **They can take support away.** The derived profile's `cas` is the worst of
   `compare_and_swap` and the two CAS races; `create_if_absent` the worst of its probe and its
   race. A backend that fences one writer and not eight no longer admits durable writes.
3. **`pstore_testkit::racy::Racy`**, a store with one of two flaws (check-then-write, and a
   deleted object's CAS as an I/O error), so the probes' detection is itself tested.
4. **The capability matrix regenerated** against RustFS and Azurite.

Not changed: any backend, any engine path. **No setting**: probes.

## Acceptance criteria
1. The memory store and every decorator still conform with 13 probes:
   `the_memory_store_conforms`, `every_decorator_conforms`.
2. A check-then-write store fails exactly the two race probes, and its derived `cas` and
   `create_if_absent` are divergent: `a_backend_that_admits_two_racing_winners_does_not_fence`.
3. A store that errs on a deleted object's CAS fails exactly `cas_after_delete_is_lost`:
   `a_backend_that_errs_on_a_deleted_objects_cas_does_not_fence`.
4. Each `Broken` defect trips exactly its probes:
   `the_suite_detects_each_defect_a_real_backend_actually_has`.
5. The suite's request cost is an exact budget (`conformance_requests`, M278):
   `conformance_costs_a_bounded_number_of_requests`.
6. `scripts/conformance.sh --check` passes against the emulators.

## Test plan
Criteria 2 and 3 are the red runs of the probes themselves: the flawed stores must fail them.

## RA budget
The suite: 48 requests, from under 40. No serving path changes.

## Risks
- **Emulators are plumbing** (D-99). RustFS and Azurite passing says nothing about S3 under
  real contention; that stays M0b.

## Tasks
- **M279.1** The spec.
- **M279.2** The probes, the flawed store, the matrix.
