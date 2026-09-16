# M7b — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" below is a **mutation applied to the shipped code**, the named test
run, and the failure read — except criteria 1–3, whose subject is a shell script and whose red
was observed by running the new selftest against the **unmodified** `review.sh` first.

1. **Rounds do not survive the commit they reviewed** — `./scripts/selftest-review.sh`, in a
   scratch repository with a real commit: two rounds spent, `git commit`, and the counter reads
   **0** again. ⚠️ **Observed red before the fix**: `FAIL rounds survived the commit they
   reviewed`, which is exactly the state M0c met on `main` — "round 4 of hard budget 4" for a
   change whose review had not started. The reset is keyed on the **HEAD the review started
   on**, recorded in `${TAG}.base`.
2. **Round 2 is a tree-to-tree delta** — `./scripts/selftest-review.sh`, same scratch repository: `a.txt` is staged before round 1 and
   untouched after it, and does **not** appear in round 2's packet; `b.txt`, edited between the
   rounds, does. ⚠️ The old form diffed the **worktree against a sha written after the previous
   packet**, which on a moved HEAD is an already-committed diff — and on an unmoved one is the
   whole diff again rather than the delta it announces. `git write-tree` per round is what
   makes "what was reviewed" a nameable object.
3. **The budget still refuses, and the reset is not indiscriminate** — the pre-existing half of
   `./scripts/selftest-review.sh` is unchanged and green: the counter advances to 2, round 3 is
   refused at `REVIEW_ROUND_BUDGET=2`, and the override is honoured. That is the test that a
   reset firing on every invocation would fail, which is the way this fix deletes the mechanism
   it repairs.
4. **`Split` routes `head` by key prefix** — `the_split_store_routes_head_by_key_prefix`
   (`cargo test -p pstore-engine --lib split_tests`). ⚠️ **Both mutants observed red**:
   `head` → `Ok(0)` fails `left: 0, right: 14`, and `head` → `self.durable.head(key)`
   unconditionally fails on the fresh arm. The two objects have different lengths precisely so
   neither survives. ⚠️ The mutant was **inert** before this test — nothing asks the decorator
   for a size — so this adds the caller that kills it, as a test, and no production caller.
5. **A refused probe is an error and an absent key is not** —
   `a_refused_probe_is_an_error_and_an_absent_key_is_not`
   (`cargo test -p pstore-blob --test faults`), asserted through `Congested` over a
   `TenantView` over `Faulty`, not on `Faulty` alone. ⚠️ **Observed red against the code that
   shipped before this milestone**: restoring `Faulty::get_tag`'s clean forward fails the test.
   That forward was not an oversight — there was nowhere to put the error, which is why row 21
   was a trait change and not a refactor. The refused probe is still billed as the read it
   issued.
6. **The refusal axis is now visible in a point** —
   `a_point_under_read_errors_reports_the_probes_that_failed`
   (`cargo test -p pstore-testkit --test sweep_axes`). Two findings, both recorded rather than
   smoothed: ⚠️ **M0c's exact configuration (0.9/0.9/0.9) no longer returns a point at all** —
   the seed probe is a read, so it is refused, and the point is `SweepError::Probe`. Asserted
   as such. ⚠️ And the loop's own counter is measured at **`read_error` 0.25**, because at
   0.3 and above the seed probe at seed 11 is refused before the loop is reached. Both arms are
   deterministic per seed, not flaky. Observed red by incrementing `probe_failed` in the
   `Ok(None)` arm: `probe_failed: 0` against an `abandoned: 1`.
7. **A refused seed probe and an absent key are different errors** —
   `an_unseeded_key_and_an_unreadable_one_are_different_errors` (same command). Observed red by
   mapping `Err` back to `SweepError::Unseeded`. ⚠️ The absent arm uses `cas_lost: 1.0`, which
   refuses the **seeding write** and leaves every read clean — an absent key with no faults is
   seeded successfully by the harness, so the distinction needs a seed that really did not land.
8. **The whole workspace is green with the new signature** — `cargo test --workspace
   --all-features` passes, and `./scripts/gates.sh` reports **all gates green** — including
   `check-poison.sh`, which matters here because the probe grew a `Result` that a recovery
   arm could drop. 17 implementations and every caller updated;
   the object-store backend's probe no longer swallows its
   metadata read with `.ok()`, so a 503 there is an error and only a missing object is absence.
9. **The crossover, measured** — `cargo run -p pstore-engine --example head_cost`:
   reads are **3 at every K** (1, 10, 50, 100, 500, 5,000) and the surplus over K=1 is
   **954 / 5,194 / 10,494 / 52,894 / 529,894 bytes**, i.e. **106.0 bytes per index at K=50** —
   M6i's figure, reproduced. At the corpus's own 30 ms round trip and a deliberately generous
   100 MB/s per stream, one round trip is worth **3,145,728 bytes**, so the crossover is
   **K ≈ 29,677 indexes**, which is **594×** above the product ceiling of 50 indexes per
   tenant. ⚠️ The absolute read bytes differ from M6i's (5,599 at K=1 against 7,903) because
   this harness folds 4-dimensional documents and M6i folded 8-dimensional ones; the surplus,
   which is the subject, is identical. `provisional`: WSL2, `MemoryStore`, one run.
10. **Row 19's decision: do not build**, on the number criterion 9 produced
    (`cargo run -p pstore-engine --example head_cost`). Sharding HEAD per index — or any layout that avoids
    reading the whole manifest — buys back **0.05 ms** at the shape a real tenant has and costs
    **one sequential round trip** of a three-round-trip budget an open already spends in full.
    The trade is 30 ms against 0.05 ms, and it stays a bad trade until a tenant owns ~30,000
    indexes, which the product does not permit. Recorded in
    [`BACKLOG.md`](../BACKLOG.md) and in the M6 exit row of
    [`roadmap.md`](../../research/11-design/roadmap.md): **M6's exit criterion stays scored as
    not fully met**, because the criterion says what it says. What changes is that the unmet
    half now has a decision behind it rather than an open question.
11. **Coverage and mutation** — `./scripts/coverage.sh --fail-under-regions 95` passes at
    **95.05% regions**, 96.53% functions, 96.88% lines across the shipping crates. Mutation:
    `docker compose -f dev/docker-compose.yml exec dev scripts/mutants.sh --file
    crates/pstore-testkit/src/sweep.rs crates/pstore-blob/src/faulty.rs
    crates/pstore-blob/src/object_store_backend.rs crates/pstore-blob/src/memory.rs` —
    **125 of 133 viable mutants caught**, 36 unviable. ⚠️ **All eight survivors are in
    `faulty.rs` and none is in code this milestone changed**: the constructor
    replaced by its own default (identical by construction), one operator in the SplitMix step,
    and six `< -> <=` on the fault-probability comparisons, which differ only when
    the 53-bit draw lands exactly on a rate boundary. Pre-existing, named rather than claimed
    as caught, and not fixed here — a test that pins a 2⁻⁵³ boundary is a test about the RNG.
    ⚠️ **A probe mutated to "absent" is caught in all three of `Faulty`,
    `MemoryStore` and the object-store backend**, which is the mutation the old `Option`
    signature made unkillable. ⚠️ The split store's size forward is **not covered by this
    sweep** — `pstore-engine`'s lib was excluded to keep the run bounded — so its two mutants
    were applied by hand against
    `the_split_store_routes_head_by_key_prefix` and read, as criterion 4 records.

## Drift, recorded

⚠️ **One test was replaced, on its own terms.**
`reads_and_slow_downs_never_reach_the_commit_loop` (M0c) pinned a **gap** rather than a
behaviour, and said in as many words: *"if that is now true, the axis can be widened and this
test should be replaced rather than deleted."* M7b made it true. Its replacement,
`a_point_under_read_errors_reports_the_probes_that_failed`, asserts the difference it could
only assert the absence of. This is the one case in this milestone where a passing test was
removed, and it is recorded here rather than in a commit body because the non-negotiable it
sits next to — never weaken a test to make a check pass — deserves the explicit argument.

⚠️ **The split store's assertion is an in-crate unit test, not an integration one.** The spec
says `crates/pstore-engine/tests/` gains it, "reached the only way a private type can be:
through `Engine`". It is a `#[cfg(test)] mod split_tests` in the crate's own lib instead, which
constructs the decorator directly — strictly better, because reaching `head` through `Engine`
is impossible while nothing on that path asks for a size, which is the whole content of row 20.
Criterion 4 never said "through `Engine`", so the criterion is met as written and the spec's
plan is what drifted.

⚠️ **Review round 1 found a number in this ledger that was not the number measured**, and it
is recorded rather than quietly corrected because it is the failure mode the second
non-negotiable names: criterion 6 said `read_error` 0.25 while the shipped constant was 0.2,
left behind by the search for the rate the seed probe survives. Both say 0.25 now, and the
value was re-run. `check-verified.py` resolves a test *name* and cannot see a constant — the
reviewer is the only thing that catches this class, which is the argument for the round.

⚠️ **One minor is a backlog row, not a fix** — row 23, `Congested::get_tag` being the only
read that is not retried on a 503.

⚠️ **`503` still cannot be injected into a conditional write.** Row 21 is closed for the
*read* half of the commit loop only. `put_conditional` consults the CAS classes alone, so the
most realistic error a CAS path meets at scale still cannot reach it. Named in M0c, still open,
and not smuggled into this ledger as fixed.
