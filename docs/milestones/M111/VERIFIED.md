# M111 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores. The CI failure this fixes was
`portable (ubuntu-latest)` at `a4a0af3`. It never reproduced here under load, so criterion 1
forces that schedule with a store decorator instead of waiting for a slow machine.

⚠️ **A test was edited.** `segment_names::a_fold_buries_what_its_discarded_attempts_created`
(M23) failed with the yield in place (`left: 1, right: 2` at line 612). Its schedule made A's
retry commit by having another lane fold everything and then flush a row only the retry
could fold. Since M111 that retry stops at the rebase instead: the winner folded all that A's
first read named. The interference is now a compaction, which commits without moving a
watermark, so A's retry still has the bundle it read and still commits. Every assertion is
unchanged except the final id set, which names the rows the new setup writes:
`epochs + 2`, two segments of A's, no leak, something buried, and GC reaps it. The case the
old schedule now reaches is `a_fold_left_with_nothing_buries_at_its_next_commit`, which passes
unedited.

1. `fold_overtaken::a_fold_whose_rows_a_winner_folded_does_not_keep_losing` passes. **Red
   before the fix:** `the fold returned Err(Lost) after 24 lost commits`. Kills, by hand:
   the yield removed, `>=` to `>`, `prune_to` removed (`unfolded()` is `Some`), and the yield
   on the first attempt (`left: 0, right: 1`). `cargo mutants`' own `>=` to `<` is caught.
2. `fold_overtaken::a_fold_whose_winner_missed_a_bundle_it_read_folds_it` passes, and was
   green before the fix, as a guard. Kills, by hand: yielding after any loss (`all(..)` to
   `true`) and `all` to `any`.
3. `fold_overtaken::a_drop_still_commits_against_the_same_rival` passes, and was green before
   the fix. Kills, by hand: the `drop.is_none()` guard removed.
4. `fold_overtaken::a_fold_that_stops_knows_the_schemas_its_winner_recorded` passes.
   ⚠️ **Written after the code, not before**: the first mutation run found
   `remember_schemas` removed survived every test. It was then observed red against
   that mutant (`fold_overtaken.rs:288`), and is green without it.
5. `linearizability::every_acknowledged_document_survives_contention` passes, with the rest
   of `linearizability.rs` (4 of 4), unedited: `git diff --stat` does not list it. The
   whole engine suite passed (`cargo test -q -p pstore-engine`, 0 failed) before criterion
   4's test was added; the hook's `gates.sh` runs it again with it.
6. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit. Mutation:
   ⚠️ **not the bare `scripts/mutants.sh`**, which mutates only committed changes and found
   none on uncommitted `main`. Ran
   `./scripts/mutants.sh --check fold_attempts --in-diff <staged diff> --test-workspace=false --test-package pstore-engine`:
   4 mutants, 1 caught, 3 unviable, 0 missed. It generates none of the guards' mutants, so
   eight were applied by hand. All eight were caught, by the tests listed above.
