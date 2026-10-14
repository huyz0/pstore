# M78 — A dense query over a named field, split across servers

**Serves:** backlog row 65's "split" remainder, after [M77](../M77/SPEC.md).

## What is true, found rather than built

M54's wire format already carries a dense leg's `field` (`WireLeg::Dense { field, .. }`) and
`splittable` already admits any dense leg. A share on a peer runs the same `query` code, which
since M77 scans a named field exactly. So no code changes: **a named dense leg already splits.**
What was missing was a test that could tell. The existing `two dense legs` case in
`a_split_query_equals_the_unsplit_one` has used `v2`, a named field, since M54, but before M77 that
leg searched `vector`'s codes, so split and unsplit were two equally wrong answers that matched.

## Acceptance criteria

1. `a_split_query_equals_the_unsplit_one` gains `dense over a named field` and `dense over a named
   field, filtered`: split equals unsplit bit for bit, over eight folded segments with deletes,
   unfolded upserts and a shadow.
2. `a_split_named_dense_query_ranks_that_field`: on 100 folded documents, a split query's top 10
   equals the test's own exact dot-product ranking of `v2`, and the query was split (2 exchanges).
   **Seen red** with M77's named-field branch disabled (wrong ids), green restored.
3. **Gates:** `./scripts/gates.sh` passes. No source line changes, so there is no mutation sweep.

**Not covered:** an approximate (clustered) index per named field.
