# M51 — A gossip cost bound per loss pattern

**Serves:** the gossip residue the project owner asked to address on 2026-10-05, "a steadier cost
bound". The single bound sat at 1,163 B against 1,300 after [M49](../M49/VERIFIED.md), and M48's
code review found the parent breaking 1,300 at other loss rates.

## What is true today

- **One bound, one point.** `a_tagged_digest_cuts_heavy_loss_at_200` measures 200 members at
  10% loss, where the `Sim` drops a datagram whenever its count of datagrams sent since the
  run began (the 50 converged rounds included) is a multiple of 10. It asserts at most 1,300
  B per node per round.
- **The drop pattern moves the number.** Measured while planning on [M50](../M50/VERIFIED.md)'s
  code (`6583af2`), with a temporary `phase` that offsets which datagram in each N is
  dropped: 4 waves, 200 lossy rounds after 50 converged (not `cost()`'s 400), at each row's
  members and loss. Bytes per node per round, over phases:

  | Members | Loss | Phases | Range | Max |
  |---|---|---|---|---|
  | 100 | 1/10 | 10 | 550–753 | 753 |
  | 200 | 1/7 | 7 | 2,139–2,245 | 2,245 |
  | 200 | 1/10 | 10 | 937–1,116 | 1,116 |
  | 200 | 1/13 | 13 | 389–629 | 629 |
  | 400 | 1/10 | 10 | 1,649–1,809 | 1,809 |

  The committed point, phase 0 at 1/10, is that row's maximum. A different phase reads up to 16%
  lower, so a single point says little about the margin.
- **Since M50 the curve is smooth.** At phase 0, cost rises monotonically with loss for 100, 200
  and 400 members over 1/50 to 1/5, and every view stays whole. M48's non-monotone readings,
  where 1 in 9 cost less than 1 in 10, are gone. The probe is the ledger's.
- At equal members, the phase ranges for different loss rates do not overlap. So "more loss
  costs more, at every phase" is a property, not a hope.
- No gate measures any of it. A gate-scale `Sim` run takes minutes, and `cargo mutants` reruns
  the suite once per mutant, which is why `depth.sh` and `recall.sh` live outside it.

## Delta

1. **The `Sim` gains `phase`**: a datagram is dropped when `(sent + phase)` is a multiple of
   `drop_every`. At `phase` 0 every existing test runs exactly as today.
2. **A gate-scale test, `#[ignore]`d**, `the_loss_curve_holds_at_every_phase`, in
   `pstore-gossip`'s `tests/protocol.rs`. For each row of the table, at every phase:
   - bytes per node per round at most the row's bound, the measured maximum plus 10%, rounded
     up to a ten: **830, 2,470, 1,230, 700 and 1,990**;
   - every view whole;
   - `budget_drops()` 0.

   For 200 members, the cheapest phase at each loss rate costs more than the dearest phase at
   the next lower rate: cost rises with loss whatever the pattern. The margins are wide
   (2,139 against 1,116, +92%; 937 against 629, +49%), so this is a property of the
   protocol. A future protocol change may legitimately revisit it.

   And for each row, the phases' maximum exceeds their minimum: phase is a real input. Without
   this, a `Sim` that ignored `phase` would replay phase 0 everywhere and pass every bound
   (spec review).
   - ⚠️ **The bounds are a regression guard, not a design target**: the measured maximum plus
     10%. The `Sim` is deterministic, so a reading past one means the protocol's cost moved,
     and the bound is never moved to absorb that.
   - **Not asserted across phases:** `untagged_marks`. The 1/5 end is out of scope too: there
     marks rise to 26 at 200 members and 54 at 400 (M50's probe), and the per-tick rule there
     is M52's to revisit with scale.
3. **`scripts/gossip-loss.sh`** runs it in release, `cargo test --release -p pstore-gossip
   --test protocol -- --ignored the_loss_curve_holds_at_every_phase`. CI runs it beside
   `depth.sh` in the `recall` job, and AGENTS.md's Gates table names it.
   - **Runtime:** the planning probes ran these configurations in release in about 145 s on
     this container: 131 s for the 200-member 1/7 and 1/10 rows plus 400 members, and 14 s for
     the 1/13 row plus 100 members. Under three minutes with the build, which `depth.sh`'s
     job already pays.
4. **The in-suite point test is unchanged**: phase 0, ≤ 1,300, as the fast guard. The new
   table is what says how much margin 1,300 leaves.

**Not changed:** the protocol. This milestone adds measurement and a gate, nothing else.

## Acceptance criteria

1. **Every pattern is bounded.** `./scripts/gossip-loss.sh` passes: 50 runs across the five
   rows and their phases, each within its bound, every view whole, and no budget drops.
2. **The shape holds, and phase is real.** The same run asserts, for 200 members, min(1/7) >
   max(1/10) and min(1/10) > max(1/13), and for every row max > min across phases. Killed by
   hand:
   - the `Sim` ignoring `phase` (the row spread check fails);
   - every bound rounded down to its row's minimum (criterion 1 fails);
   - two loss rates' order swapped (the ordering check fails).
3. **`phase` changes nothing at 0.** It starts at 0 in `Sim::new`, and no existing test sets
   it. Every existing gossip test passes unchanged.
   `./scripts/gates.sh`.
4. **The gate is wired.**
   - `grep -n "gossip-loss.sh" .github/workflows/ci.yml` matches;
   - `python3 scripts/build-index.py --check` passes with the Gates row added;
   - `scripts/check-portable.sh` passes for the new script.
5. **Mutation:** the change is confined to `tests/` and `scripts/`, which produce no mutants,
   and `cargo mutants` does not run `#[ignore]` tests. So the sweep is empty by construction,
   and the ledger says so rather than reporting a 0. Criterion 2's hand mutations are the
   evidence that the gate can fail.
