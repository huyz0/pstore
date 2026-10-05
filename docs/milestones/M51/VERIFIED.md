# M51 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `pstore-gossip`'s deterministic `Sim`,
in release. Every figure is datagram bytes per node per round, none a timing. The run time
below is this container's.

1. **Every pattern is bounded.** `./scripts/gossip-loss.sh` passes in 2 min 26 s (137 s for the
   test itself). Bytes per node per round over the phases, against each bound:

   | Members | Loss | Phases | Range | Bound |
   |---|---|---|---|---|
   | 100 | 1/10 | 10 | 550–753 | 830 |
   | 200 | 1/7 | 7 | 2,139–2,245 | 2,470 |
   | 200 | 1/10 | 10 | 937–1,116 | 1,230 |
   | 200 | 1/13 | 13 | 389–629 | 700 |
   | 400 | 1/10 | 10 | 1,649–1,809 | 1,990 |

   Every view stayed whole, and no digest was dropped for want of budget. The ranges are
   exactly the planning probe's.
2. **The shape holds, and phase is real.** The same run asserts min(1/7) > max(1/10)
   (2,139 > 1,116) and min(1/10) > max(1/13) (937 > 629), and max > min in every row. Each
   hand mutation was run against the full gate and fails it:
   - the `Sim` ignoring `phase`: "100 members at 1/10: every phase cost 608";
   - every bound set to its row's minimum: "100 members at 1/10, phase 0: 608 B/node/round,
     bound 550";
   - the 1/7 and 1/10 order swapped: "1/7 against 1/10: (2139, 2245) (937, 1116)".
   - ⚠️ A first attempt at the swap did not apply, because `cargo fmt` had reflowed the
     assertion. That run passed and proved nothing, so it was redone and is not counted.
3. **`phase` changes nothing at 0.** It starts at 0 in the `Sim` constructor, and no other test sets it.
   `cargo test -p pstore-gossip`: every test passes, with the gate the one ignored.
   `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - Added in code review: the script refuses unless exactly one test passed. Run with
     `--skip the_loss_curve_holds_at_every_phase`, it exits 1 with "the loss-curve test did
     not run", where it would have passed with "0 passed". A missing table row now fails
     loudly (`expect`) instead of comparing against zero.
4. **The gate is wired.**
   - `grep -n "gossip-loss.sh" .github/workflows/ci.yml` matches, in the `recall` job after
     `depth.sh`.
   - `python3 scripts/build-index.py --check`: "ok", with the AGENTS.md Gates row added.
   - `scripts/check-portable.sh`: "ok".
5. **Mutation.** The change touches only `tests/`, `scripts/`, CI and AGENTS.md, which produce
   no mutants, and `cargo mutants` does not run `#[ignore]` tests. So the sweep is empty by
   construction, and no 0 is reported. Criterion 2's hand mutations are the evidence that the
   gate can fail.

**What the margin under the suite's bound is, now known:** the in-suite point (200 members, 1/10,
phase 0) is 1,116 against 1,300. That point is its row's maximum, so the 1,300 bound has 16%
over the worst phase measured.

**Residue (gossip):** marks across phases and the 1/5 end are not asserted. At 1/5, M50's probe
measured 26 marks at 200 members and 54 at 400, and cost 3,462 and 7,423 B. That is the
heavy-loss scaling M52 takes on, with fleet size.
