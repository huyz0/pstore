# M269 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.

The SDK suite (clients/turbopuffer/test_sdk.py: test_writes_by_filter_and_by_condition,
test_multi_query, and M268's two) is Python, so it is named without code marks:
`check-verified.py` resolves only Rust tests.

Red runs.
- With `src/facade.rs` put back to its M268 version and the new tests kept,
  `facade_filters_and_conditions_are_native_calls`, `facade_multi_query_is_the_native_multi_query`
  and the extended `facade_adds_no_request` fail: M268 refuses each new key.
- The SDK tests were not run against M268's facade, which refuses each key they send; the
  in-process red run above covers the same refusal.
- Each spec mutation was applied by a script to `src/facade.rs`, which ran the facade tests
  and reverted. See criterion 2 for what killed each one.

1. The SDK runs against pstore (`./scripts/facade.sh`): green, 4 tests OK. It downloaded
   the pinned `turbopuffer==3.1.0` wheel, checked its sha256, installed it into a scratch
   virtual environment and ran against an unsecured server on the in-memory store with
   applied writes on. Each of these was checked:
   - delete_by_filter removes the matching rows and counts them in rows_deleted;
   - patch_by_filter sets an attribute on the matching rows;
   - an upsert_condition that admits one of two rows writes only that row, with
     rows_upserted 1 and upserted_ids holding its id;
   - patch_condition and delete_condition the same way;
   - return_affected_ids on a plain write gives the ids;
   - multi_query with a vector sub-query and an order sub-query under an eventual root
     answers two results: the vector result's ids equal the native query's, and the order
     result is the three rows in `n` order (checked against known values, not a native call);
   - multi_query with rerank_by is refused BadRequestError, naming it.

   ⚠️ Its first run failed in the test itself: a row written without `n` has no `n`
   attribute on the SDK's `Row`, so `r["n"]` raised. The test reads it with a default now.
2. Translation, in process:
   - `facade_filters_and_conditions_are_native_calls`, `facade_multi_query_is_the_native_multi_query`
     and `facade_adds_no_request` (a conditional patch and a two-query multi-query added, each
     call now measured in a fresh store), by `cargo test -p pstore-server --test it facade`:
     green, 6 passed.
   - `a_write_is_applied_only_when_its_answer_needs_it`, `each_sub_query_takes_the_roots_consistency`
     and `an_applied_write_answers_what_changed_not_what_was_sent`, by `cargo test -p
     pstore-server --lib facade`: green, 6 passed.

   The mutations and the tests that killed each:
   - `patch` not renamed `attributes`: `facade_filters_and_conditions_are_native_calls`.
   - A conditional write kept `durable`: `facade_filters_and_conditions_are_native_calls`,
     `facade_adds_no_request` and `a_write_is_applied_only_when_its_answer_needs_it`.
   - The requested counts answered instead of the outcome's:
     `an_applied_write_answers_what_changed_not_what_was_sent`.
   - An empty id list sent: `facade_filters_and_conditions_are_native_calls`.
   - The root consistency not applied to a sub-query: `each_sub_query_takes_the_roots_consistency`.

   ⚠️ The first run missed two. The requested-counts mutation as first written did not
   compile, so it tested nothing. The root-consistency mutation survived every in-process
   test: on a folded store a strong read and an eventual read return the same rows. Both
   unit tests were added for them, and the second run killed both.
3. Gates:
   - `cargo fmt --check` passes.
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings` first failed on
     two writes by index into the answer in `write_answer`. The answer is built as a map
     now, and clippy passes.
   - The workspace suite, `cargo test --workspace --no-fail-fast -j 3`, ran 56 test binaries
     before that clippy fix: 2,929 passed, 0 failed, 15 ignored. The fix changed only how
     `write_answer` builds its map. After it, the facade tests (6 in-process, 6 unit) and
     `./scripts/facade.sh` (4 OK) were run again, green.
   - The two IPv6 tests were NOT-RUN, as in M267 and M268, because this container has no
     IPv6 loopback.
   - `scripts/check-verified.py`, `scripts/check-links.sh` and `scripts/build-index.py
     --check` pass.
   - `cargo deny check`, coverage and the mutation sweep were NOT-RUN here; CI runs them on
     the PR.
