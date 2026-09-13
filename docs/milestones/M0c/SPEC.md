# M0c — The two sweeps D-101 named and M0a never ran

**Serves:** **D-101** in [`roadmap.md`](../../research/11-design/roadmap.md) — *"rather than
'what is the CAS rate?', sweep 0.5–50 CAS/s and find where the design breaks. **Same for
latency and error rate**"* — and decision question 1, *"at what CAS rate does the design
break? (A threshold, not a measurement)"*.

## ⚠️ M0a's ledger says the shortfall in its own words

> *"criterion 7's curve, and the sweep is reusable for latency and error rate"*

Reusable, not reused. `Point` in the sweep module carries `writers`
and nothing else, so one of the three axes D-101 asks for exists. ⚠️ **The prerequisite for a
second one landed after M0a closed**: M0a.10 added latency injection with a SplitMix64 stream
of its own, so enabling latency cannot change *which* operations fail. A latency sweep could
not have been written when M0a shipped; it can be now.

## ⚠️ A threshold needs something to be a threshold of, and there is one

`MAX_CAS_ATTEMPTS = 16`: the catalog's appender gives up after sixteen attempts and returns
`Contended`. So "where the design breaks" is not a matter of taste — it is where a commit
costs more attempts than a caller is willing to spend. M0a's existing curve ends at **10.95
attempts/commit for 128 writers**, which is 68% of that budget and had never been compared
against it.

## ⚠️ Two things measured before speccing, because both change the shape of this work

**Only 412 and 409 reach the commit protocol at all.** Measured, not read: at
`read_error: 0.9, write_error: 0.9` a sweep point is *identical* to a clean one, and `get_tag`
returned a tag 50 times out of 50. The loop uses only `get_tag` and `put_conditional`;
`get_tag` returns `Option` so it has **no error channel**, and `put_conditional` consults only
the CAS classes — so `slow_down` cannot be injected into a conditional write either, and 503
is the most realistic error a CAS path meets at scale. ⚠️ A failed probe and an absent object
are the same value to every caller,
and the "error rate" axis can only be swept over the classes CAS can express. The axis is
named **412/409 rate** for that reason, and the wider gap is reported, not fixed — making
`get_tag` fallible is a trait change across the workspace.

**The loop has no give-up budget, the real caller does, and the seeding put is inside the
injection.** `Appender::record` stops after `MAX_CAS_ATTEMPTS`; the sweep retries forever. At
`cas_lost: 0.5` it still terminates (measured: 20 attempts, 8 commits). At 1.0, measured both
ways:

| seeded | result at `cas_lost: 1.0` |
|---|---|
| inside the injection, as the code does today | returns at once: **`attempts: 0, commits: 4, lost: 0`** |
| outside it, key already present | **livelocks** — killed at 5s |

⚠️ **The first row is the assumed count at its worst**: `contention_point` seeds the key
through the same store, so at a high rate the seed never lands, every writer's `get_tag`
returns `None` and takes the missing-tag break, and the row still claims four commits landed
on zero attempts. A point that reports commits it did not make is not a weaker measurement
than the livelock — it is worse, because it returns.

Which makes `commits: (writers * commits_each) as u64` — a **product of its own inputs**, not
a count — the thing to fix first. Under a budget a writer can abandon a commit, and the row
would still claim it landed: every derived ratio wrong **in the flattering direction**.

## Delta

- `Point` gains the configuration it was taken under — the latency **floor and ceiling**, since
  the spread is what produces jitter, and the 412/409 rate — plus `abandoned`, and `commits`
  becomes **counted**. ⚠️ Three new fields break the two `Point` literals in
  `a_point_with_no_commits_is_infinite_not_a_divide_by_zero` as missing-field compile errors;
  no assertion in it becomes false.
- **The key is seeded outside the injection**, and `contention_point` **refuses to run** when
  the key is absent instead of silently reporting a point. The new sweeps own their store for
  that reason: seed a `MemoryStore`, then wrap it in `Faulty`.
- **A per-commit attempt budget**, so the sweep models the caller that exists rather than an
  infinitely patient one, and the error axis reaches 1.0 without livelocking. ⚠️ It must be the
  **same constant**, not a matching literal: `MAX_CAS_ATTEMPTS` is private to `pstore-catalog`
  and `pstore-testkit` cannot see it, so it moves to `pstore-types` and the catalog re-exports
  it. A 16 with a comment beside it drifts the first time the catalog's changes.
- ⚠️ **`sweep_reports_a_curve_not_a_point` is amended as part of this work, not after it.** It
  asserts `commits == writers × commits_each`, which a budget can falsify at 32 writers on a
  tail commit. The replacement is `commits + abandoned == writers × commits_each` — the
  invariant that is actually true, and **stronger**, because it also catches a miscount that
  the product cannot. Naming it here is what keeps it a correction rather than a test weakened
  to make a check pass.
- `latency_sweep` and `cas_error_sweep` beside `contention_sweep`, over `Faulty`, each
  returning the same `Point` so one `render` serves all three.
- `render` gains the columns and flags any row at or above the budget.
- `crates/pstore-testkit/examples/contention.rs` prints all three curves.

**Does not add** — **a real-cloud number.** Which point on these curves reality occupies is
M0b's, and blocked. **Backoff.** The sweep measures the loop that exists; a budget is what the
caller has, a delay between attempts is not. **A fallible `get_tag`.** The gap is real and is
reported; closing it is a trait change touching every backend. **A gate on any threshold.**
These are shapes, and a floor under a provisional shape is a floor under a guess.

## Acceptance criteria

1. **A latency sweep reports a curve** — wall clock, attempts/commit and completion at fixed
   contention across at least four injected latency spreads, one of them zero. ⚠️ **No
   direction is claimed for attempts.** The delay applies to `get_tag` and `put_conditional`
   alike, so raising both scales the vulnerable read-to-CAS window and the whole cycle
   together and the ratio has no reason to move; what must rise is elapsed time. Whether
   jitter moves the ratio is the open question this axis answers, either way.
2. **A 412/409 sweep reports a curve** — the same across at least four injected rates, one of
   them zero and one of them 1.0, which the budget makes reachable rather than a hang.
3. ⚠️ **Commits are counted, not assumed** — at a high injected rate a point reports **fewer**
   commits than `writers × commits_each`, with the shortfall in `abandoned`, and a test pins
   that the two differ. Nothing else here matters if this is wrong: every ratio rests on it.
4. ⚠️ **The contention curve is reported against M0a's, and any movement is the finding** —
   1 writer at 1.00 attempts/commit and 100% success is unconditional. ⚠️ At 128 writers M0a
   recorded 10.95 attempts/commit under an *unbounded* loop; a 16-attempt budget may abandon
   commits there, and if it does, **M0a's published curve was optimistic** and this criterion
   says so rather than quietly replacing it. Either outcome is reportable; neither may be
   silent.
5. **Each sweep names where it breaks, against `MAX_CAS_ATTEMPTS`** — the row where
   attempts/commit crosses 16 or a commit is abandoned, or the statement that no swept point
   reaches it, with the value at the worst point.
6. **`render` carries the caveat and the new columns** — a table pasted anywhere still says
   `PROVISIONAL` and still names M0b.
7. **`provisional`**, said so, and gates green — in-process store, WSL2, one run. The example's
   wall clock is reported, since injected latency is real time.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | one writer under a paused clock takes injected time to commit | latency configured and never awaited, so the axis is a column of zeroes. ⚠️ `start_paused` needs `current_thread`, so this test uses one writer and costs the suite nothing; the multi-writer curve is the example's, outside `cargo test` |
| 2 | at rate 1.0 the point is `attempts == writers × commits_each × 16`, `commits == 0`, `abandoned == all`, inside a `timeout` | the unbounded loop, restored. ⚠️ Asserted as a **shape**, not as a hang: a hang is not an observed failure and `cargo mutants` scores it a timeout rather than a kill |
| 2 | a point on a key that was never seeded is refused, not reported | today's silent break, which returns four commits on zero attempts |
| 2 | injecting `read_error` **and `slow_down`** alone leaves a point unchanged | a future claim that the error axis covers reads or 503s, which it cannot |
| 3 | commits counted differ from `writers × commits_each` under a high rate | the product, restored — which passes every existing test |
| 4 | one writer is exactly 1.00 attempts/commit and 100% | a budget that changes the uncontended case, where nothing may change |
| 6 | the caveat and every new column survive in `render` | a column added to the struct and forgotten in the table |

⚠️ Criterion 5 is a **number in the ledger**, not a test: a threshold asserted as a test is a
gate on a provisional shape, which the Delta refuses.

## RA budget

Unchanged — `pstore-testkit` is not on any path. The sweep issues one `get_tag` and one
`put_conditional` per attempt, which is the commit protocol's own budget and not a new one.

## Risks

- ⚠️ **Injected latency is real wall time in an example**, which has no paused clock. Ranges
  stay small enough that the example remains runnable and criterion 7 reports what it cost.
  ⚠️ It stays **out of `cargo test`** beyond the single paused-clock case: a sweep of real
  sleeps inside the suite is a cost `cargo mutants` pays once per mutant, which is the argument
  that already keeps `recall.sh` and `depth.sh` outside it.
- ⚠️ **The budget may move M0a's 128-writer number.** That is a finding, not a regression to
  paper over: the old figure was taken from a loop no caller has. Criterion 4 requires it be
  reported either way.
- **Three curves from one in-process store are still one store.** The shape is the deliverable;
  D-104 applies and the render says so.

## Tasks

| Id | Commit |
|---|---|
| **M0c.1** | `MAX_CAS_ATTEMPTS` moves to `pstore-types`; the sweep gains the budget, counts its commits, refuses an unseeded key, and its own test's product assertion becomes the sum that is true under a budget |
| **M0c.2** | The latency and 412/409 axes, the columns that record them, and the example that prints all three |
