# M268 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.

The SDK suite (clients/turbopuffer/test_sdk.py: test_the_sdk_against_the_facade,
test_a_refusal_is_the_sdks_error) is Python, so it is named without code marks:
`check-verified.py` resolves only Rust tests.

Red runs.
- With the facade's routes removed from `Api::router`, all four `tests/it/facade.rs` tests
  fail: every facade path is a 404.
- AC1's SDK suite was not run against a server without the routes. It needs the routes
  AC2's red run removed, and its first call is a write to one of them.
- Each test was also seen red by the mutations below. A script applied each one to
  `src/facade.rs`, ran `cargo test -p pstore-server facade`, and reverted. All five were
  killed, and so was one more: `&& !retryable` dropped.

1. The SDK runs against pstore (`./scripts/facade.sh`): green. It downloaded the
   `turbopuffer==3.1.0` wheel, checked its sha256 against the pinned
   `f5e44b54…35254389`, and installed it into a scratch virtual environment under Python
   3.11. It ran 2 tests against an unsecured server on the in-memory store, OK:
   - a 3-row write (3 rows affected);
   - an ANN query whose ids and `$dist` equal the native query's;
   - a BM25 query, which ranks `red red dog` first;
   - an order by `n` descending under a `kind` filter;
   - `limit` with `per`;
   - an eventual read;
   - `metadata()` with 3 rows, and `exists()`;
   - `namespaces(prefix, page_size=1)`, which lists both namespaces over two pages;
   - a patch and a delete, then the rows a query sees;
   - `delete_all()`, then `exists()` false;
   - `NotFoundError` for a missing namespace's metadata, and `BadRequestError` naming
     the delete-by-filter key.
2. Translation, in process:
   - `facade_write_and_query_are_native_calls`, `facade_errors_are_the_vendors`,
     `facade_identity_never_reads_the_tenant_header` and `facade_adds_no_request`, by
     `cargo test -p pstore-server --test it facade::`: green, 4 passed.
   - `knn_is_exact_and_ann_is_not`, `eventual_is_bounded_in_milliseconds` and
     `only_a_permanent_conflict_is_answered_400`, by `cargo test -p pstore-server --lib
     facade::`: green, 3 passed.

   The mutations and the tests that killed each:
   - `$dist` taken from `score` first: `facade_write_and_query_are_native_calls`.
   - A permanent 409 kept 409: `facade_errors_are_the_vendors` and
     `only_a_permanent_conflict_is_answered_400`.
   - The tenant header read: `facade_identity_never_reads_the_tenant_header`.
   - `kNN` without `exact`: `knn_is_exact_and_ann_is_not`.
   - `max_staleness_seconds` not multiplied: `eventual_is_bounded_in_milliseconds`.

   ⚠️ The first mutation run missed three of them. The in-process tests drew the same
   answers from a three-row corpus whether or not `exact` was set and whatever the
   staleness bound. The 409 case also never reached a facade route: a dims mismatch is a 400
   natively. So the unit tests were added, and the 409 case became a facade write past a
   one-index limit, which is `index_limit`. The second run killed all five.
3. Gates:
   - `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
     -D warnings` pass.
   - The workspace suite, `cargo test --workspace --no-fail-fast -j 3`, ran 56 test binaries:
     2,922 passed, 1 failed, 15 ignored. The failure was `openapi::error_codes_are_the_servers`,
     which scans the server's source for status and code pairs. It found the unit test's
     made-up `unavailable` code. The test now uses `storage_unavailable`, and the `openapi`
     module then passed, 7 of 7.
   - The two IPv6 tests were NOT-RUN, as in M267, because this container has no IPv6
     loopback.
   - `scripts/check-portable.sh`, `scripts/build-index.py --check` (which matches the new
     Gates row against CI's new `facade` job), `scripts/check-links.sh` and
     `scripts/check-verified.py` pass.
   - `cargo deny check`, coverage and the mutation sweep were NOT-RUN here; CI runs them on
     the PR.

## After CI's mutation shard (PR #66)
Shard 1: `!t.is_empty() && t.bytes().all(..)` in `identity` replaced by `||` was missed.
With it, `Bearer +7` passes the filter and `u128`'s parser reads it as tenant 7. Every
non-digit key the test sent also failed to parse, so the mutant made no difference to it.
`facade_identity_never_reads_the_tenant_header` now refuses `Bearer +7` and an empty key,
each with a 401. The mutant, applied by hand, turns it red: `Bearer +7` answers 200.

Shard 3: two mutants in `query_body` were missed.
- The default-field check `!=` replaced by `==`: no test used a named vector field.
- The `strong` guard `c.is_empty()` replaced by `false`: no test sent an explicit
  `{"level": "strong"}`, because an absent consistency is strong too.

`knn_is_exact_and_ann_is_not` now checks that `vector` is left unnamed and that `title_v` is
named. `eventual_is_bounded_in_milliseconds` checks that an explicit strong level is strong,
and that a strong level holding a staleness is refused. Each mutant, applied by hand, turns
its test red.
