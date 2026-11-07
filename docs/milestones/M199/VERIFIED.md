# M199 verification

Implemented in:
- `crates/pstore-engine/src/lib.rs`: `Engine::ordered_projected_as_of`, the shared
  `projection`, and `scope`'s metric under `as_of`, taken from `schema_recorded_at` (the
  wrapper `schema_at` is folded into it).
- `crates/pstore-server/src/lib.rs`: the refusal of `include_vectors` with `as_of` moves from
  `vector_projection` to the query, after the plan, for a ranked or scored query only; the order
  arm calls `ordered_projected_as_of` when `as_of` is given.

The docs are in `docs/api-migrations.md` (M199) and the DELIVERY row for H's snapshot export.
All runs were in a 4-core cloud container, so every number is `provisional`.

How to run:
- `cargo test -p pstore-engine --test it export_pages::`
- `cargo test -p pstore-server --test it vectors_out::`

**Red first.**
- The engine tests did not compile against the parent, which has no `ordered_projected_as_of`.
  With the call added and the metric still the present's, `a_past_epoch_gives_vectors_back_in_its_own_metric`
  failed: a euclidean vector came back with its extra dimension (.harness/m199-red-engine.log).
  The other two passed at that point: the order already read a past epoch's rows and vectors,
  and only the call to ask for both was missing.
- The server test failed against the parent's server, which refused the order with `as_of`
  (.harness/m199-red-server.log).

**Suite:** 2,727 passed and 0 failed, with the two IPv6 tests skipped because the container has
no IPv6. fmt and clippy pass (.harness/m199-suite.log).

**Coverage:** `scripts/coverage.sh` over the shipping crates gave lines 97.32%, regions 95.99%
and functions 97.05% (.harness/m199-coverage.log).

**Mutations.** I applied 9 mutations one at a time by script and reverted each one
(.harness/m199-mutations.log). All 9 were killed. The run came before `cargo fmt`, so the line numbers in its log name the
test files before formatting:
- the past metric the present's, or the default, or the latest drop's schema;
- the epoch not passed (the present paged), and the named field not projected;
- the server refusal dropped, lifted for a scored query, kept for an order, and the order
  answered at the present.

**Acceptance criteria.**
1. PASS: `an_order_pages_one_past_epoch_with_its_vectors`. 30 rows over two folds, then a
   delete, a replacement, two new rows and a compaction. Pages of 7 by `id > last` at `E` equal
   the unpaged answer taken before the first change, each row with `vector`, the named field
   and the sparse field.
2. PASS: `a_past_epoch_gives_vectors_back_in_its_own_metric`. `x` is dropped after `E` and
   re-created under `dot_product`; `y` is dropped, re-created and dropped again. Both answer
   every vector as written, with no extra dimension.
3. PASS: `a_restore_of_a_paged_export_answers_as_its_epoch`. The pages, written into a fresh
   tenant under the summary's metric, equal the source's rows at `E` with every field, and an
   exact, filtered vector query answers the same ten ids as the source `as_of E`. It kills no
   mutation beyond AC 1's, as the spec says.
4. PASS: `an_export_page_takes_as_of_and_vectors`. The order `as_of E` answers `200` with the
   vectors taken at `E` before three rows were replaced. A ranked and a scored query with both
   are `400`, and an epoch after the present is `400 time_travel_horizon`. The existing
   refusal test (`what_cannot_be_projected_is_refused_naming_the_rule`) is unchanged and passes.
5. PASS for `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the suite and
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95`, run here (see above).
   `scripts/mutants.sh` is pending CI's shards.

Code review (packet `.harness/review/m199.r2.packet`; the r1 packet was cut short by a full
disk, and the coverage build was removed to free it) passed with no blocker or major. The
packet's own `cargo test --workspace` replay runs without the two IPv6 skips, which fail in a
container with no IPv6; the suite log above is the run with them skipped. Minors taken: AC 5
says mutants are pending CI, and the mutation log's line numbers are explained. Left as it is:
AC 3 reads the summary's schema after the later writes, not in the read that picked `E`. Its
metric cannot change after the first fold, so the test is unaffected.
