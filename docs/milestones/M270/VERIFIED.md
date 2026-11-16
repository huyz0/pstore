# M270 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.

The SDK suite (clients/turbopuffer/test_sdk.py: test_explain_warm_recall_schema_and_metadata
and the four from M268 and M269) is Python, so it is named without code marks:
`check-verified.py` resolves only Rust tests.

The spec changed twice while it was built:
- The native explain refuses an order-by query (a 400), so the facade passes that refusal on
  rather than answering a `null` plan.
- The in-memory store refuses a read cache (`PSTORE_CACHE_DIR` is refused with
  `PSTORE_BACKEND=memory`), so `scripts/facade.sh` cannot warm one. Its warm gets the
  nothing-to-warm `ACCEPTED`. The in-process test warms a real cache.

Red runs.
- With the new tests written and no facade change, `facade_explain_warm_and_recall_are_native_calls`
  failed with a 404 on `explain_query`, `facade_adds_no_request` with the same 404, and
  `facade_schema_and_metadata_updates_are_native_calls` with a 405 on the schema update.
- The new SDK test was not run against M269's facade, which has none of these routes; the
  in-process red run covers the same absence.
- Each spec mutation was applied by a script to `src/facade.rs`, which ran `cargo test -p
  pstore-server facade` and reverted. See criterion 2.

1. The SDK runs against pstore (`./scripts/facade.sh`): green, 5 tests OK. The new test
   checked:
   - explain_query on an ANN query answers plan_text naming the `dense` leg;
   - hint_cache_warm answers ACCEPTED (the nothing-to-warm answer, as above);
   - recall with num 2, top_k 2 and include_ground_truth answers an avg_recall from 0 to 1
     and two ground truths whose neighbors carry a float `$dist`;
   - recall with rank_by is refused BadRequestError, naming it;
   - update_schema is refused BadRequestError, naming schema_immutable;
   - update_metadata(read_only=True) answers metadata with 3 rows and approx_logical_bytes
     above 0; a write is then refused BadRequestError naming index_read_only;
     update_metadata(read_only=False) lets the next write in;
   - update_metadata(pinning=True) is refused BadRequestError, naming pinning.
2. Translation, in process: `facade_explain_warm_and_recall_are_native_calls`,
   `facade_schema_and_metadata_updates_are_native_calls` and the extended
   `facade_adds_no_request`, with the five earlier facade tests, by `cargo test -p
   pstore-server --test it facade`: green, 8 passed. The 6 unit tests in `src/facade.rs`
   pass too.

   `facade_adds_no_request` now measures each side in a fresh world with a read cache: a
   facade call against the native calls it makes. Explain, warm, recall and the refused
   schema update each cost their one native call, and update_metadata costs the native patch
   and the summary together.

   The mutations and the tests that killed each:
   - `explain` not set on the native query: `facade_explain_warm_and_recall_are_native_calls`.
   - The plan taken from `meta.cost` instead of `meta.explain`: the same test.
   - `no_read_cache` answered as an error: the same test.
   - Recall's unknown keys, `rank_by` among them, not refused by the facade: the same test,
     which checks the facade's own wording. The native recall refuses `rank_by` too, so a
     check that only named the key could not tell the two apart.
   - `dist` not renamed `$dist`: the same test.
   - `read_only` not passed to the native patch: `facade_schema_and_metadata_updates_are_native_calls`
     and `facade_adds_no_request`.
   - `pinning: true` accepted: `facade_schema_and_metadata_updates_are_native_calls`.
   - `approx_logical_bytes` answered 0: the same test.

   ⚠️ Found by reading, before the mutation run, not by running the first version: recall
   had a dedicated `rank_by` check, and explain refused `queries` by name, but the generic
   refusals behind each also name the key, so the tests as first written could not have seen
   either check removed. The recall check was folded into the unknown-key refusal, and both
   tests now match the facade's own wording. The run above was against that version.
3. Gates:
   - `cargo fmt --check` passes.
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings` first failed
     twice: `update_schema` had an unused type parameter, and a test compared a bool with
     `assert_eq!`. Both are fixed, and clippy passes.
   - The workspace suite, `cargo test --workspace --no-fail-fast -j 3`, ran 56 test binaries
     before those two fixes: 2,931 passed, 0 failed, 15 ignored. The fixes removed a type
     parameter and rewrote one assertion. After them, the facade tests (8 in-process, 6 unit)
     and `./scripts/facade.sh` (5 OK) were run again, green.
   - The two IPv6 tests were NOT-RUN, as in M267 to M269, because this container has no IPv6
     loopback.
   - `scripts/check-verified.py`, `scripts/check-links.sh`, `scripts/build-index.py --check`
     and `scripts/check-portable.sh` pass.
   - `cargo deny check`, coverage and the mutation sweep were NOT-RUN here; CI runs them on
     the PR.
