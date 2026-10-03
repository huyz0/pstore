# M31 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0). No
number here is a latency. The fleet harness was not run: `scripts/cluster.sh` needs Docker,
and this container has none (**NOT-RUN**).

Command: `cargo test -p pstore-node --test schedule --test policy`.

⚠️ **A test that fails to compile is not counted as red** (spec review M4). The tests were first
red that way, against an API that did not exist. Each row below then names the mutation that
was applied by hand and the test that caught it. All 13 were caught.

1. **Heal at `HEAL_PERIOD`, at the node's slot.** `heals_at_the_heal_period_at_its_slot`.
   - `heal_every` is 10, 10 and 5 views at 200 ms, 1 s and 2 s.
   - Killed: `heal_every` counted in polls (the parent's arithmetic, which gives 50 at
     200 ms), and the slot fixed at 0.
2. **Ownership in seconds.** `ownership_is_reported_in_seconds`.
   - 5, 5 and 2 views. Killed: `owns_every` counted in views (the parent's), a zero period
     reporting, and the parse default.
   - The parent's `owns_every > 0` guard was dropped as equivalent: `u64::is_multiple_of(0)`
     holds only for 0, and the view count is at least 1 there.
3. **One poll's order.** `one_poll_runs_in_mains_order`. Killed: the view count moved after the
   checks, `last_size` starting at 0, and the slot.
4. **Settings.** `settings_fall_back_as_main_did`. Killed:
   - the positive filter dropped;
   - the poll falling back to the default rather than the gossip period;
   - the loss default;
   - `chitchat` chosen by any value.
5. **Filters.** `seeds_and_in_cell_filter_exactly`. Killed: each filter's comparison inverted.
6. **Log lines.** Each of the ten prefixes is in `main.rs` once, before (`0c8eba0`) and after
   (`36c76f7`). Checked with `grep -c` and compared by `diff`, and the code review checked
   it again.
7. **`main` decides nothing.** The spec's `sed | grep` printed nothing, here and in code review,
   and so does its single-`grep` form, which reads only text before any `//`:
   `grep -nE '^[^/]*(%|is_multiple_of|last_size|\.filter\(|==|!=|\.max\()' crates/pstore-node/src/main.rs`.
8. **Gates.**
   - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `36c76f7`, and on this ledger's
     commit.
   - The sweep over the new module and the two filters (`0c8eba0..36c76f7`): 30 mutants,
     **30 caught**.
   - ⚠️ Not `./scripts/mutants.sh` itself: `cargo mutants --in-diff` was run directly.

**Residue (code review, minor):** an ownership period near `u64::MAX` wraps in
`Cadence::new`'s cast instead of saturating. The result is still about 10^19 views.

Spec review took two rounds: the first blocked on the unit bug, the grep and the inventory.
Code review took one round, which passed. One of its minors (a measured figure dropped from a
comment) was restored before the commit.
