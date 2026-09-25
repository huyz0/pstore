# M8j — `pstore-index`'s LIRE maintenance: a dead scope set, a redundant loop, four edges

**Serves:** **D-111**, and OQ-51, which `lire` exists to answer. Measured on `feefbd8` in eight
shards against `pstore-index`'s own tests ([`sweep/`](sweep/)), then every candidate re-run
against the whole workspace ([`sweep/workspace-recheck.txt`](sweep/workspace-recheck.txt)):
**50** survive in the crate, **12** of them in `lire.rs` — this milestone. The rest is M8k/M8l.

## Delta

**Rung 1: `dirty` is dead code (2 mutants, `65:38` `-`→`+`, `/`).** `split_pass` and the merge
loop fill a `dirty` set that **nothing reads**: after the merges, reassignment recomputes its
scope as `dirty_now`, from centroid movement, because merged lists shift the indices (the
code's own comment says so). The set, its parameter and its three inserts are deleted.
⚠️ Two test comments state the opposite — "that set is the reassignment scope" — and one
records the inert `+ 1` mutant as measured and "not chased". Both comments are corrected;
**no assertion changes**.

**Rung 1: `split_pass`'s loop, and the loop around it (6 mutants).**
- `while i < lists.len()` then `lists.get(i).is_some_and(..)`, then `let Some(rows) = ..
  else { i += 1; continue }`: the bound is checked twice and the `else` is unreachable, so
  `<`→`<=` (47:13) and the dead branch's `+=`→`-=`/`*=` (50:19) cannot change anything.
  Rewritten as `while let Some(list) = lists.get(i)`, one `i += 1`.
- The round loop's `any(|l| l.len() > bounds.max)` pre-check (215:49 `>`→`<`, `>=`) only
  duplicates `split_pass`'s own test, and **masks** it: a mutant of `split_pass`'s bound
  (48:49) is never reached for a list of exactly `max`, because the pre-check never calls the
  pass (observed: the split-bound test below does not fail under 48:49 while it stands). It is
  deleted and the bounded loop always runs its `SPLIT_ROUNDS` passes. Equivalent: a pass that
  splits nothing changes nothing, so extra passes only re-read list lengths.
  ⚠️ **Amended at spec review:** the first draft instead had `split_pass` return its count and
  break at 0 -- which creates `== 0`→`!=` and `+= 1`→`*=` mutants meaning "stop after one
  splitting pass", NOT equivalent (a half rewritten in place is not revisited in its pass), and
  no test killed them.

**Tests (4 edges).**
- **The split bound** (48:49 `>`→`>=`): a list of exactly `max` rows is not split; `max + 1` is.
- **The disturbance threshold** (304:49 `>`→`>=`) **and the neighbour rule** (314:69
  `!=`→`==`), with `Scope::Touched`, `Bounds { max: 10, min: 1 }`, two single-row lists with
  centroids at the origin and far away. The first row sits at `(2⁻¹⁰, 0.031607694923877716)`,
  whose squared distance from the origin is **exactly** `SPLIT_DISTURBANCE` in f32 (found by
  search in a Python f32 model; no 1-D value squares to it). At the edge, nothing is disturbed:
  `examined == 0`. One float further in `y`: the moved list **and its nearest neighbour** are
  examined, `examined == 2` — `== 1` under the neighbour mutant, which pairs a list with itself.
- **Reassignment moves a misplaced row** (340:25 `!=`→`==`): `Scope::All`, rows `0, 9, 10` in
  lists `[[0, 9], [10]]`; recentred, 9 is nearer the second list. After: `[[0], [9, 10]]`,
  `reassigned == 1`.
- **`bisect` is the documented algorithm** (449:30 `<=`→`>`). ⚠️ The mutant's final
  partition, `[1]` against the rest, is a 2-means fixed point of the correct rule and a
  *lower-cost* one on the pinned input (the mutant itself oscillates with period 2 and stops
  there), so no quality property separates them. What is pinned is the algorithm the doc states: Lloyd from the
  farthest pair, ties to the first seed. On `[1, −4, 1, 0, 4]`: halves `[1, 3]` and `[0, 2, 4]`,
  centroids `−2` and `2`, from an independent Python model (the mutant gives `[1]`, `−4`).
  Inline, since `bisect` is private.

**Does not change:** any maintenance outcome, `Work` count bar the spin rounds above (which
counted nothing), or any threshold.

## Acceptance criteria

1. `lire.rs` has no `dirty` set; the two test comments no longer call it the scope, and the one
   in `the_work_report_counts_what_actually_happened` no longer calls the `best != li` and
   `bisect` inversions invisible; every existing `tests/lire.rs` assertion is unchanged.
2. `split_pass` has one loop condition and no unreachable branch; the round loop has no
   `any(.. > bounds.max)` and no early exit.
3. A list of `max` rows is not split and one of `max + 1` is.
4. At the exact threshold `examined == 0`; one float past it `examined == 2`.
5. The misplaced row moves: `[[0], [9, 10]]`, `reassigned == 1`.
6. `bisect` on the pinned input returns the model's halves and centroids.
7. Every `lire.rs` mutant, run against `pstore-index`'s own tests
   (`--test-workspace=false --test-package pstore-index`), then any survivor re-run against
   the workspace: **0 missed**. Timeouts count as caught, as `mutants.sh` states.
8. `./scripts/gates.sh` passes.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 3 | `>`→`>=` on the split bound | splitting a list that fits |
| 4 | `>`→`>=` on the threshold; `!=`→`==` in the neighbour filter | a stationary list rescanned; a moved list's neighbour skipped |
| 5 | `!=`→`==` on `best != li` | reassignment that moves nothing |
| 6 | `<=`→`>` in `bisect` | a split that is not the documented one |
| 7 | the measurement: 12 missed | all of the above |

## RA budget

Unchanged: in-memory maintenance, no blob access.

## Risks

- Criterion 4 rests on f32 summation order in `dist2` (`0 + d1² + d2²`); the model sums in the
  same order. Rust's float `sum` starting at `-0.0` rather than `0.0` does not change it.
- Criterion 7's two steps are the measurement's own method: index-only tests can only
  overstate survivors, and the short run survives this container's restarts, which have
  killed every run over about three hours.

## Tasks

- **M8j.1** — the two rung-1 changes, the four tests, the comment corrections, this ledger.
