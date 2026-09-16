# M7b — The four rows the backlog leaves open

**Serves:** **D-101** (sensitivity sweeps that can see the axis they sweep), **D-34** (the
three-round-trip budget, which row 19 proposes trading bytes against), **OQ-5**'s refusal axis,
and the round-budget mechanism [`review`](../../../.agents/skills/review/SKILL.md) rests on.
Closes **BACKLOG rows 19, 20, 21, 22** — every row left open after M0c, and the whole of it.

**Depends on** [M0c](../M0c/SPEC.md), which measured rows 21 and 22 and fixed neither, and
[M6i](../M6i/SPEC.md), which measured row 19.

## ⚠️ Why these four in one milestone

They share nothing but their provenance: each is a previous milestone reporting something it
found and did not fix. Splitting them into four milestones would be four specs restating the
same four backlog rows. **They are four independent commits under one ledger**, and the ledger
is the reason to group them — after this, the carried-forward list is empty and what remains is
the Blocked table, which is blocked on accounts and hardware rather than on effort.

## Delta

### Row 22 — `scripts/review.sh` counts rounds that belong to other changes

**Changes.** The round counter gains a **base**: `${TAG}.base` records `HEAD` at round 1, and
any invocation finding `HEAD` has moved since **discards that tag's rounds and starts at 1**.
A commit is the end of a review, so the rounds it spent do not bind the next change.

The delta round gains a **true delta**. Today round *N* diffs the worktree against the sha
recorded *after* round *N-1*'s packet, which on a moved HEAD is an already-committed diff.
Each round now records `git write-tree` — the staged tree, which is exactly what was reviewed —
and round *N* prints `git diff <tree N-1> <tree N>`.

**Does not change** the budget of 4, the refusal, or the override. The budget is why the loop
terminates; what was wrong is what it counted.

⚠️ **Not keyed on the staged tree**, which the backlog offers as the alternative. The staged
tree *changes between rounds* — that is what a round is — so keying the counter on it resets
the budget on every fix and removes the mechanism entirely.

### Row 20 — the `Split` decorator's `head` is unasserted

**Changes.** `crates/pstore-engine/tests/` gains a routing assertion for `Split`, reached the
only way a private type can be: through `Engine`. ⚠️ **The mutant is inert today** — nothing
asks `Split` for an object's size — so this adds the caller that makes it die, as a test, and
does **not** add a production caller for one.

### Row 21 — a failed read is indistinguishable from an absent object

**Changes.** `BlobStore::get_tag` returns `Result<Option<CasTag>, BlobError>`: `Ok(None)` is
"nothing is there", `Err` is "the probe failed". Every implementation in the workspace
forwards it; `Faulty::get_tag` **injects `read_fault` like every other read**, which it cannot
do today.

`sweep::Point` gains `probe_failed` — rebases abandoned because the probe errored — so a point
taken under injected read errors is no longer byte-identical to a clean one, which is what M0c
measured and could not fix. `SweepError::Unseeded` keeps its meaning and gains a sibling,
`SweepError::Probe`, for a seed probe that **errored** rather than found nothing.

**Does not change** `put_conditional`, which still cannot carry a 503 — that is an
`object_store` and fake-backend question, named in M0c and still open.

### Row 19 — an open pays ~106 bytes of HEAD per index its tenant owns

**Adds** `crates/pstore-engine/examples/head_cost.rs`: the K-table M6i measured, plus the
crossover this row exists to argue — the K at which the surplus HEAD bytes cost as much as the
round trip that sharding HEAD would add. **No format change**, and the milestone's deliverable
for this row is a **number and a decision**, in the ledger, as rows 6 and 6b were closed.

## Acceptance criteria

1. `scripts/review.sh` starts a tag at round **1** again once `HEAD` has moved, and the rounds
   spent before the commit are discarded — asserted in a scratch repository, with a real commit.
2. Round *N*'s packet contains the diff between round *N-1*'s **staged tree** and round *N*'s,
   and **not** the whole worktree diff: a file staged before round 1 and untouched since does
   not appear in round 2's packet, and one edited between the rounds does.
3. The budget still refuses round 5 within one unmoved `HEAD`, and the override still works —
   the existing selftest, unchanged and still green.
4. `Split::head` returns the **fresh** store's size for a `mem/` key and the **durable** store's
   size for a tenant key, with the two sizes different, so both a constant and a
   forwarded-to-the-wrong-arm mutant fail.
5. `get_tag` returns `Err` when the backend refuses the read: on `Faulty` at `read_error` 1.0 a
   probe is `Err`, and on an absent key at 0.0 it is `Ok(None)` — the two outcomes the old
   signature could not tell apart.
6. A contention point taken at `read_error` 0.9 has **`probe_failed` > 0**, and one taken at 0.0
   has `probe_failed == 0` — M0c's "byte-identical to a clean one", asserted as a difference.
7. A seed probe that errors returns `SweepError::Probe`, and an absent key returns
   `SweepError::Unseeded`; the point is not taken in either case.
8. The whole workspace suite is green with the new signature — 17 implementations and every
   caller — and `scripts/gates.sh` passes.
9. `cargo run -p pstore-engine --example head_cost` prints read **bytes and requests** at
   K = 1, 50, 500, 5,000, reproducing M6i's numbers, and prints the **crossover K** at a stated
   per-stream throughput and round-trip time, both named in the output rather than assumed.
10. Row 19 is closed in the ledger with that number and an explicit **build / do-not-build**
    decision, and `docs/milestones/BACKLOG.md` carries no open row afterwards.
11. Region coverage ≥95% on the changed crates; the mutants added by the changed modules are
    caught, or named as equivalent with the reason on the line.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `scripts/selftest-review.sh` — scratch repo, round 1, commit, round again | a reset that never fires, and one that fires on every invocation (which would remove the budget) |
| 2 | same selftest, asserting on packet **content** | a delta computed against `HEAD` rather than the previous staged tree — green whenever `HEAD` has not moved, which is the case the defect hides in |
| 3 | existing `selftest-review.sh` assertions | a reset that discards rounds within one change, i.e. the budget deleted by the fix to criterion 1 |
| 4 | `the_split_store_routes_head_by_key_prefix` | `head` returning a constant; `head` forwarded to `durable` unconditionally |
| 5 | `a_refused_probe_is_an_error_and_an_absent_key_is_not` | `Faulty::get_tag` ignoring `read_fault` — the current behaviour; `Err` mapped to `Ok(None)` by any decorator, which is the old signature reintroduced one layer down |
| 6 | `a_point_under_read_errors_reports_the_probes_that_failed` | `probe_failed` incremented in the `Ok(None)` arm instead of the `Err` arm, which makes it a duplicate of `abandoned` |
| 7 | `an_unseeded_key_and_an_unreadable_one_are_different_errors` | both mapped to `Unseeded`, which is the defect one level up |
| 9 | `cargo run --example head_cost`, and the existing `an_open_reads_the_whole_head_including_the_indexes_it_is_not_opening` | an example that prints a constant: the test pins the relationship the example prints |

⚠️ **Criterion 5 is the load-bearing one.** `Faulty::get_tag` does not call `read_fault` today
*because it has nowhere to put the error* — so the signature change is not a refactor; it is the
thing that makes the refusal axis reachable at all. A change that updates 17 signatures and
leaves `Faulty` forwarding cleanly has done none of the work.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Everything on a serving path | **unchanged** | unchanged | unchanged | 0 |
| `get_tag` | 0 W, 1 R | unchanged — the signature moves, the request does not | 0 | 0 |
| An open at K indexes | unchanged: **3 Rseq**, bytes linear in K | unchanged | 0 | 0 |
| `head_cost` example, `review.sh` | off every serving path | — | — | 0 |

## Risks

- **The signature change is wide and shallow, which is the dangerous shape**: 17 mechanical
  edits where one silently swallows the new error (`.unwrap_or(None)`, `.ok().flatten()`) and
  restores the defect under a decorator. Criterion 5 asserts through a **stack**, not on
  `Faulty` alone, and `clippy -D warnings` refuses a dropped `Result`.
- **Row 19 may come out "build it"**, and then this milestone delivers a measurement and a task
  rather than a layout. That is the correct outcome and is named here so it is not a surprise;
  what it must not do is decide by preference. The number decides.
- **The `review.sh` reset can be too eager.** A `HEAD` that moves for an unrelated reason
  mid-review — an amend, a rebase — discards a live round budget. Accepted deliberately: the
  failure it replaces is a **refusal** of a change that has had no rounds, and the failure it
  risks is an **extra round** on a change whose author just rewrote history under it.
- **`Split::head` stays uncalled in production.** The test makes the mutant die; it does not
  make the method used. If the decorator is ever replaced wholesale the test goes with it, and
  that is correct — row 20 is about an unasserted forward, not about `head`.

## Tasks

| Id | Commit |
|---|---|
| M7b.1 | `review.sh`: rounds keyed to a base HEAD, delta to the previous staged tree, selftest first |
| M7b.2 | `Split`'s `head` routing, asserted |
| M7b.3 | `get_tag` becomes fallible, `Faulty` injects into it, and the sweep counts failed probes |
| M7b.4 | `head_cost` example, the crossover, and row 19's decision |
| M7b.5 | The ledger, the backlog, and the roadmap's M6 exit row |
