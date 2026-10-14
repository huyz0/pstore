# M77 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store.

1. `a_dense_leg_over_a_named_field_scans_that_field` (`cargo test -p pstore-query --test
   named_dense`): two segments of 30 and 20 documents with a 3-wide `tag` beside a 4-wide
   `vector`; the leg's top 10 equals the test's own dot-product ranking over `tag`, to the bit, and
   differs from `vector`'s. `a_missing_or_unsuitable_field_is_answered_or_refused`: a segment
   without `tag` answers nothing (the other's 20 rows are all there is); a field no segment carries
   is refused; a width of 2 against 3 is an error naming both; a two-vector field is refused
   naming `multi`.
2. `the_api_searches_a_named_field` (`cargo test -p pstore-server --test named_dense`): after a
   write and a fold, `{"vector": q, "field": "tag"}` returns the exact dot-product top 10 over
   `tag`; `field: "vector"` equals the unfielded query; `tag$fde` is `400`; a field no document
   carries is `400 unknown_field` (it was `200` with no results, then `500 retryable`: both
   wrong, fixed); a 2-wide query is a schema conflict; a two-vector field is `400` naming `multi`
   (it was `500`).
3. `a_named_field_follows_the_index_metric`: under a cosine index, the top 5 by `tag` are the exact
   cosine order and each `$dist` equals `1 - cosine` to 1e-4. (A first version read `distance`, not
   `$dist`, and so asserted nothing; found and fixed.)
4. **Gates:** green by the pre-commit hook on this commit. **Mutation** (`cargo mutants`,
   `--no-config`, changed lines of `pstore-query`, `pstore-engine` and `pstore-server`): first sweep
   23 mutants, 15 caught, 6 unviable, **2 missed**, both guards in the engine's `query_error`
   (which errors are the client's). Each is killed by
   `only_a_named_dense_or_multi_field_is_the_clients`, a unit test of that function; re-run on
   `query_error` (after adding the `multi` and `fde` cases to that unit test, which the narrowed
   selection had stopped reaching): **9 mutants matched, 6 caught, 1 unviable; every one in
   `query_error` is caught**. The 2 reported missed are `Head::decode` lines the filter matched by
   accident, unrelated to this change, which the narrowed `--lib query_error` selection cannot
   reach (the workspace suite covers them).

**Not covered:** an approximate (clustered) index per named field; splitting the leg across
servers; named-field `$dist` for a multi-vector field.
