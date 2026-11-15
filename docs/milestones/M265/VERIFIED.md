# M265 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.
Changes from the spec's first draft made while building, all now stated in the spec:
- A computed expression may be signed. The first test found that `Expr::check`'s root rule
  refused a bare `["Attribute", "n"]`. That rule exists only so a ranking's scores stay
  non-negative, so `compute_attributes` uses the new `Expr::check_limits`.
- The width and text-field refusals need the index's schema, so they come from the
  snapshot, not before any blob request.
- A multi-vector field answers its nearest vector.
- A field no row holds reports `null`.
- An outer fusion (M142) drops `$computed`, as it drops `$highlight`.
- The score field is `score`, not `$score`.

The pinned SDK, `turbopuffer` 3.1.0 (wheel sha256 `f5e44b54…4389`), fixed the expression
shapes: `types/custom.py`, `Expr` and `ExprVectorDist`.

How to run the tests: `cargo test -p pstore-server --test it computed::`.

Red runs. All five tests are red against the parent: `compute_attributes` is not a field
there and `$computed` is never written. Each test was also seen red by the mutations below.
A script applied each mutation, ran the tests and reverted it. No `cargo mutants` run is
claimed. All seven were killed.

1. `computed_bm25_equals_rank_by`: green. The corpus is two segments plus unfolded rows,
   queried with both a vector query and a text query. `$computed.s` equals the `rank_by: E`
   `score` exactly; a root `Attribute` gives the row's value, and `null` for a missing
   attribute. Killed:
   - the computed statistics leaving out the unfolded rows (Q1);
   - a root with no value reported as `0` (Q2);
   - values given to the wrong rows (Q3).
2. `computed_attributes_never_change_the_page`: green for a vector, a text and a hybrid
   query, with no vector spelled where none was projected.
3. `computed_vector_dist`: green under dot product, cosine distance and euclidean squared,
   for the query vector (against `$dist`), another vector, a named field and its absence,
   with another width refused. Killed: dot product measured as L2 (Q4); the width check
   dropped (Q6).
4. `computed_attributes_add_no_round`: green. Depth with `compute_attributes` (a BM25
   expression and a `VectorDist`) equals depth without, for a vector query and a text
   query. Killed: statistics read after the query instead of beside it (Q5).
5. `computed_attributes_refusals`: green for twelve refusals, none reading a blob, plus a
   BM25 over a non-text field refused from the snapshot. Killed: the refusal of a
   non-relevance query dropped (Q7).
6. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. `scripts/gen-clients.py --check` passes after regenerating the clients
   for `compute_attributes` and `$computed`.
   The workspace suite at this commit, `cargo test --workspace --no-fail-fast -j 3`, ran 56
   test binaries: 2,898 passed, 0 failed, 15 ignored. The two IPv6 tests were NOT-RUN, as in
   M264, because this container has no IPv6 loopback. `cargo deny check`, coverage and the
   mutation sweep were NOT-RUN here; CI runs them on the PR.

## After CI's mutation shards (PR #65)
- Shard 4: `map.len() > MAX_COMPUTED` replaced by `>=` was missed, since the refusals tested
  257 names and none tested 256. `computed_attributes_refusals` now also asks for 256 names and
  gets 256 values in `$computed`. The mutant, applied by hand, turns it red.
- Shard 1: `Engine::computed`'s retry guard `hit` replaced by `true` and by `false` were missed:
  nothing reaped a segment a cached HEAD named. `computed_retries_only_a_cached_snapshot` in
  the engine's `snapshot` tests follows `a_scored_query_retries_only_a_cached_snapshot`. A
  bounded read caches a HEAD, a compaction and GC reap its segments, and the next read
  succeeds with one fresh HEAD read. An unknown text field on a fresh HEAD fails after one read,
  with no retry. Each mutant, applied by hand, turns it red.
