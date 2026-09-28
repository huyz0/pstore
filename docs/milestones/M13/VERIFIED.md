# M13 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

⚠️ **The gate checks M13.1's criteria only.** It reads the first "Acceptance criteria"
section, as it did for M11. M13.2's lines below restart at 1, so they are enumerated here
and not by the script.

## M13.1 — per-id operations

Every server test below is in `cargo test -p pstore-server --test patch`, and every engine
test in `cargo test -p pstore-engine --test patch`.

**Observed red.** Every server test written before the implementation failed the same way:
a write carrying only `patch_rows` was refused, `400` "a write with no documents and no
deletes". The engine tests were written after the code, and each was seen failing under a
hand mutation, as named below. So were the tests added at code review.

1. **Patch** — `a_patch_merges_into_the_folded_row`:
   - the merged attributes, `null` removing one;
   - a filter on the new value finds the row, and one on the old value does not;
   - the count is unchanged, and a patch of a missing id makes no row;
   - `patch_columns` gives the same result as `patch_rows`.

   The original vector is criterion 5's.
2. **Order** — `operations_apply_in_fold_order`, in one fold:
   - an upsert and two patches compose;
   - a patch before an upsert is overwritten by it;
   - a patch after a delete leaves the row deleted.

   `a_patch_after_a_delete_supersedes_the_right_row_across_blocks` (code review) covers 300
   rows over several blocks, with an earlier row already deleted. Every row keeps its values,
   and the patched row keeps its whole base version and vector. With the fold's base rows cut
   to ids alone (`keep` answering false), it failed.
3. **Conditions** — `conditions_decide_against_the_current_version`:
   - each kind, refused and then admitted;
   - a refused operation leaves `updated_epoch` unchanged, so no segment or delete vector is
     rewritten.

   `a_missing_id_meets_each_kind_as_the_table_says` (code review) covers the missing-id rows
   of the table:
   - a patch and a conditional delete of a missing id do nothing, and neither does a patch
     that changes nothing;
   - a conditional upsert of a missing id inserts it.

   With the no-change skip removed by hand, it failed.
4. **Deferred visibility** — `a_deferred_operation_is_invisible_until_its_fold`:
   - `eventual`, on the writer and on another process, answers the pre-patch row;
   - `session` with the patch's token is `503` on both;
   - `strong` through the writer is `503`;
   - after the fold, the token is served and the patch is visible.
5. **Vectors and schema**
   - `a_patch_is_never_a_reject_and_a_conditional_upsert_is_checked`: a conditional upsert of
     the wrong width is counted in `rejected_rows`, and a patch never is.
   - `a_conditional_upsert_is_judged_by_the_reject_pass` covers the two-writer race the
     door cannot see. With conditional upserts treated as rowless, it failed.
   - `a_patched_row_keeps_its_stored_vector`: under cosine and euclidean, the distance
     before and after the patch is equal.
     - ⚠️ Its cosine half cannot detect a double transform, because normalising twice is
       normalising once. The euclidean half can. This was noted at code review and is not
       fixed.
   - `a_patch_keeps_the_rows_sparse_and_dense_vectors`: both are kept. With the merge
     clearing the base's vectors, it failed.
   - `a_patch_of_a_value_no_segment_stores_is_refused` (code review): NaN and a nested array
     are refused, and nothing is buffered. With the storable check removed, it failed.
6. **Cost**
   - `a_patch_costs_one_put`: 1 blob write and 0 reads.
   - `a_plain_fold_reads_what_it_did_before_m13`: 13 reads and 859 bytes, measured on the
     code before M13.
7. **Refusals** — `what_a_patch_cannot_mean_is_refused`. Each is `400`, and its message names
   the rule:
   - a patch with `vector`, in `patch_rows` and, since code review, in `patch_columns`;
   - unequal columns;
   - a condition that is not a filter;
   - each condition with no operations of its kind;
   - `batched`.

   A condition that nests too deeply to reach the fold is also refused (code review). With
   the fix removed by hand, it failed. So did the `patch_columns` vector case.
8. **Gates** — NOT-RUN yet: the mutation sweep over the M13.1 diff and code review round 2 are running; this line is replaced with their results.
