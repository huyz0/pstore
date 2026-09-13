# M0c — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Tests: `cargo test -p pstore-testkit`. Curves:
`cargo run --release -p pstore-testkit --example contention`, whole run **2.9s**.

1. **A latency sweep reports a curve** — five spreads at 8 writers, one zero. Wall clock
   **0 → 355 → 1310 ms** for fixed 0/5/20 ms, and 385 ms for a 0–20 ms jitter against 1310 ms
   for a flat 20 ms, which is the mean doing what a mean does.
   `latency_is_paid_inside_the_commit_loop` pins that the delay is awaited **inside** the loop
   and not merely configured. ⚠️ **No direction in the attempt ratio, as predicted and now
   observed**: 3.60 → 5.24 → 6.32 flat, then 2.94 and 2.75 under jitter. The delay lands on
   the rebase read and the CAS alike, so it scales the vulnerable window and the whole cycle
   together. Reported because the spec forbade claiming a direction either way.
2. **A 412/409 sweep reports a curve** — six rates at 8 writers, 0.0 through **1.0**, which
   the budget makes reachable rather than a hang: attempts/commit **3.65 → 5.43 → 5.91 → 8.30
   → 14.39 → ∞**, commits landed 31 → 30 → 32 → 27 → 23 → **0**.
   `at_total_cas_loss_nothing_lands_and_the_budget_is_spent` pins that endpoint as a shape
   inside a timeout — its own fixture is 2 writers × 3 commits, so **96 attempts, 6 abandoned,
   0 landed**, each commit spending exactly the budget. (512 and 32 are the example's row.)
   Observed red before the budget existed: the same configuration ran until killed at 5s.
3. ⚠️ **Commits are counted, not assumed** —
   `every_asked_commit_is_either_landed_or_abandoned`, at a 97% refusal rate so that something
   is actually abandoned. `a_point_on_an_unseeded_key_is_refused_not_reported` covers the worse
   case measuring turned up: seeded *through* the injecting store at rate 1.0 the seed never
   lands, and the point used to return **`attempts: 0, commits: 4`** — four commits on zero
   attempts. `SweepError::Unseeded` now. Observed red against the restored product.
4. ⚠️ **M0a's curve did not survive the budget, and this is the finding.** One writer is still
   exactly 1.00 attempts/commit and 100%, and 2 writers still land everything — pinned by
   `a_single_writer_never_contends`, where the budget must change nothing. At **128 writers**,
   over four quiet runs: **240, 244, 248 and 253 commits abandoned of 1,024** — a steady
   **23–25% that a real caller would have seen fail**, reproduced by the reviewer at 253, 263
   and 272. M0a recorded 10.95 attempts/commit there and the cost is
   reproducible (10.84–11.82), but it was taken with an **unbounded** loop, so it reported no
   failures because it could not have any. ⚠️ The two ratios are also not the same statistic:
   M0a divided by commits *asked*, this divides by commits *landed* with each numerator capped
   at 16. M0a's co-published success rate of 9.1% rests on the same assumed numerator and
   measures **8.2–8.5%** now. ⚠️ **The cost figure stands; the implied completion
   was never measured.** Recorded in [M0a](../M0a/VERIFIED.md) beside the original.
5. **Where it breaks, against a give-up budget of 16 attempts** — from `cargo run --release
   -p pstore-testkit --example contention`, four clean runs: abandonment first appears at
   **4 writers** (1 commit of 32 in three runs of four), and **no swept contention level
   reaches 16 attempts/commit** — 128 writers peak at **10.84, 11.47, 11.65, 11.82**, so the
   crossing lies **above the swept range**. On the refusal axis it does cross, between **0.90
   and 1.0** (14.39, then infinite). ⚠️ **An earlier draft of this line claimed the crossing
   was between 16 and 64 writers, from two samples of 18.2 at 32 writers.** Those were taken
   while the machine was loaded; seven subsequent runs on a quiet one give 3.91–5.43 there,
   and the reviewer reproduced 4.16–5.32 independently. The band was real variance in the
   measurement apparatus, not in the protocol, and the honest reading is the quiet one.
6. **`render` carries the caveat and the new columns** —
   `the_table_carries_every_column_and_its_caveat`: latency, 412/409, abandoned and wall, plus
   `PROVISIONAL`, `M0b`, and the budget's value flagged **per row** rather than in prose under
   the table, because a table gets pasted away from its prose.
7. **`provisional`** — WSL2, an in-process store, `cargo run --release … --example contention`
   at **2.9s** for all three arms. `./scripts/gates.sh` green.

## What this found that it was not looking for

- ⚠️ **Only 412 and 409 can reach the commit protocol at all.** Measured before speccing, and
  pinned by `reads_and_slow_downs_never_reach_the_commit_loop`: at `read_error`, `write_error`
  and `slow_down` all at 0.9 an uncontended point is exactly 4 attempts, 4 commits, nothing
  abandoned. The loop's only read is `get_tag`, which returns `Option` and therefore has **no
  error channel**, and `put_conditional` consults only the CAS classes. So **503 — the most
  realistic error a CAS path meets at scale — cannot be injected into a conditional write**,
  and a failed probe is indistinguishable from an absent object to every caller in the
  workspace. Not fixed here — making `get_tag` fallible is a trait change across every
  backend. Backlog row 21.
- ⚠️ **The sweep's own clock was wrong for a paused runtime.** `Point::elapsed` used
  `std::time::Instant`, which a paused tokio clock does not move, so the latency test failed
  against correct code until it became `tokio::time::Instant`. Found by writing the test.
- ⚠️ **`scripts/review.sh` refuses rounds it should not.** Its counter is keyed on the branch
  name and never resets, so on `main` it accrues rounds across unrelated tasks: it called this
  change "round 4 of hard budget 4" before its review began, and handed the reviewer a diff
  against a sha four commits stale. Backlog row 22 — the budget is right, what it counts is not.
- **`MAX_CAS_ATTEMPTS` now lives in `pstore-types`**, because a second crate has to agree with
  it and a matching literal would drift the first time the catalog moved the number.

## Review, and what it changed

⚠️ **The code review failed this change on criterion 5, and it was right.** The draft said
attempts/commit crosses the budget between 16 and 64 writers, on two samples of 18.2 at 32
writers. The reviewer could not reproduce it — 4.16, 5.32, 4.91 — and re-running on a quiet
machine gives 3.91–5.43. My two samples came from runs taken while a release build and a
container sweep were competing for the box: **variance in the apparatus, reported as a
threshold of the protocol.** Corrected above, and it is the same failure this session has hit
three times now — a dramatic number from a contaminated measurement, which looks like a
finding precisely because it is large.

It also found three tests weaker than they read, all fixed and each verified against the
mutation it now kills: the conservation assertion ran at a refusal rate that **abandoned
nothing in ten runs of ten**, degenerating into the product it replaced; the footer naming the
budget could be deleted whole with the suite green, because the check for "16" matched the
attempts column; and a second test taking a 100% refusal point had no timeout, so removing the
budget hung it rather than failing it — an argument the spec made and I applied to only one of
the two tests.

## What this does not do

- **No real-cloud number** (M0b's, blocked), **no backoff** — the sweep measures the loop that
  exists — and ⚠️ **no gate on any threshold**: these are provisional shapes, and a floor under
  one would be a floor under a guess.
