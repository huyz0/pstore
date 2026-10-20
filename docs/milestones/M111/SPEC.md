# M111 — a fold that lost stops once a winner folded what it read

**Serves:** D-34's commit protocol as the engine runs it. No backlog row: CI found it.
`every_acknowledged_document_survives_contention` failed on `portable (ubuntu-latest)` at
`a4a0af3`, with `fold()` returning `Lost` at `linearizability.rs:100`.

## What is true today

- **A fold is tenant-scoped on every attempt.** `fold_attempts` reads HEAD, then folds
  every lane's unfolded span. When its commit loses, it reads HEAD again and folds what is
  left, up to `MAX_COMMIT_ATTEMPTS` (24). After that it returns `Lost`.
- **Under steady writes, something is always left.** Other lanes flush while the fold
  works, so a rebased attempt still has bundles to fold and goes back to the CAS. One
  attempt reads, seals and writes segments, which is far longer than the backoff (at most
  512 µs). So a fold slower than its peers can lose 24 commits in a row.
- **`Ok` promises less than it retries for.** `Ok` means every bundle acknowledged before
  the call is folded at the epoch returned. `branch_index`, `POST /v1/admin/fold` and the
  scheduled fold behind a `strong` read rely on that. A loser keeps retrying for bundles
  that landed after its first read, which `Ok` never promised.

## Delta

1. `fold_attempts` remembers each `(lane, tail)` its **first** attempt read. After any
   attempt that did not commit (`Lost` or `Contended`), once the HEAD it reads has every
   one of those lanes' watermarks at or past that tail, it returns `Folded::Nothing` at
   that HEAD's epoch. It does not probe or commit again. Before returning it remembers that
   HEAD's schemas and prunes to it, as a commit would.
2. **Only what the first read named.** A bundle that lands later is not this call's to
   fold. This process's own batches alone are not the test: another lane's bundle can sit
   past the winner's read and inside this one's (spec review M1).
3. **Never for a drop.** `delete_index` needs its own commit, so it keeps rebasing as today.

**Not changed:** `MAX_COMMIT_ATTEMPTS`, the backoff, the commit, what a winning fold folds,
and a first attempt, which still folds every lane.

## Acceptance criteria

1. **The forced schedule.** A fold whose every commit is preceded by a rival that folds
   everything and then flushes a new row returns `Ok` after exactly **1** lost commit, its
   row is visible to a fresh reader, and it holds nothing unfolded (`unfolded()` is `None`).
2. **A winner that missed a bundle is not enough.** When the winning commit read before a
   third lane's bundle landed, and the losing fold's first read named that bundle, the fold
   commits it: a reader that folds nothing sees the row at the epoch returned.
3. **A drop still commits.** `delete_index` under the rival schedule never returns
   `Ok(None)` for an index that exists.
4. **A fold that stops knows its winner's schemas.** An index the winner created refuses a
   wrong-width write at the door of the process that stopped.
5. **A fold by a process that flushed nothing still folds.** `linearizability.rs` is not
   edited, and its four tests pass. Its readers fold on a lane that flushed nothing.
6. `./scripts/gates.sh` passes, and `scripts/mutants.sh` finds no surviving mutant in the
   lines this changes.

## Test plan

| # | Test | Red before | Mutation it catches |
|---|---|---|---|
| 1 | `fold_overtaken::a_fold_whose_rows_a_winner_folded_does_not_keep_losing` | `Err(Lost)` after 24 lost commits | removing the yield; `>=` to `>`; removing `prune_to`; yielding on the first attempt |
| 2 | `fold_overtaken::a_fold_whose_winner_missed_a_bundle_it_read_folds_it` | green (a guard) | yielding after any loss; `all` to `any` |
| 3 | `fold_overtaken::a_drop_still_commits_against_the_same_rival` | green (a guard) | yielding during a drop |
| 4 | `fold_overtaken::a_fold_that_stops_knows_the_schemas_its_winner_recorded` | written after the yield, red with `remember_schemas` removed | removing `remember_schemas` |
| 5 | `linearizability::every_acknowledged_document_survives_contention` | red on CI only (timing) | yielding on the first attempt |

## RA budget

Unchanged per attempt. A fold that yields skips the rest of its attempts. The yield check
reads nothing: it uses the HEAD the attempt already read.

## Risks

- **Abandoned segments wait longer.** A lost attempt's segments are buried by this
  process's next commit (M23). A yield is not a commit, so under contention a process
  that yields can hold them until it next commits. That is true today of a fold that finds
  nothing after a loss; the yield makes it the usual outcome.
- **The bound is not a proof of progress.** A loser still loses while no winner read past
  its first read. A winner on the epoch the loser read at attempt 1 read after attempt 0's
  probes, so about two losses are the usual worst case. The 24-attempt bound stays.

## Tasks

- **M111.1** — the yield, the tests, and this ledger.
