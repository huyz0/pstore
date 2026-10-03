# M31 — `pstore-node`'s `main` wires, and its library decides

**Serves:** [BACKLOG](../BACKLOG.md) row 34, which [M8d](../M8d/SPEC.md) opened: `main.rs` makes
decisions no gate can see, and the crate's own rule is "`main.rs` wires, `lib.rs` decides".
Reviewing the move found a bug in one of them, which this milestone fixes.

## What is true today

Coverage has excluded binary entry points since M5, and mutation has since M8d. In
`crates/pstore-node/src/main.rs`:

**Decisions, which move:**
- **The cadences.** `view_every = 1s / poll`, at least 1. A view is one pass of the loop's
  second half, once every `view_every` polls.
  - ⚠️ **`heal_every = HEAL_PERIOD / poll` counts polls, but it is compared with `ticks`, which
    counts views** (spec review B1). At the 200 ms poll a fleet measures at, a node heals
    every 50 views, which is 50 s, not `HEAL_PERIOD`'s 10 s. `main.rs`'s comment says this
    was fixed; only the 1 s and 2 s polls are right.
  - `owns_every` is `PSTORE_OWNS_PERIOD_S` taken as a count of **views** (default 5, 0
    disables). So it means seconds only at a poll of 1 s or less, and at a 2 s poll the
    default reports every 10 s.
- **One poll, in order:**
  1. `sub += 1`.
  2. If `size != last_size` (which starts at `usize::MAX`), report a view change.
  3. Unless `sub` is a multiple of `view_every`, the poll ends there.
  4. Otherwise `ticks += 1`, then the view is reported as `t=ticks`, so the first is 1.
  5. Heal when `ticks % heal_every == jitter(node_id, heal_every)`.
  6. Report ownership when `owns_every > 0 && ticks % owns_every == 0`.
- **The filters:** the seeds (the roster without this node's advertised address), and what to
  publish (the gossip view in this node's zone).
- **The settings:** `PSTORE_GOSSIP_PERIOD_MS` (positive, else `DEFAULT_GOSSIP_PERIOD`),
  `PSTORE_POLL_PERIOD_MS` (positive, else the gossip period), `PSTORE_PROBE_LOSS` (parsed,
  else 0), and `PSTORE_GOSSIP` (`chitchat`, else SWIM).

**Wiring, which stays** (named so that criterion 7 is not read as covering it):
- the announce, with its one rebase on a lost CAS;
- the heal's I/O, with each request bounded by `ATTEMPT_TIMEOUT`, and the choice between the
  `HEAL` and `REFOLD` lines from `policy::union_to_publish`'s answer;
- `Membership`'s per-protocol arms, including chitchat's labelling of every peer with this
  node's zone (one cell, M4c's known limit);
- `owned_shards(…, 1000, 3)`'s diagnostic constants;
- the store builder.

## Delta

**A `schedule` module holds the decisions, and the cadences count views.**
- **`Cadence::new(poll, owns_period_s, node_id)`** fixes the unit bug. With a view interval of
  `view_every × poll`:
  - `heal_every` is `HEAL_PERIOD / view interval`, at least 1;
  - `owns_every` is `owns_period_s` seconds over the view interval, at least 1, and 0
    disables.
  - Healing is unchanged at every poll ≥ 1 s, and below 1 s it returns to `HEAL_PERIOD`.
  - Both round **down**, so a report is never later than asked: at a 2 s poll a 5 s
    ownership period reports every 4 s, where it reported every 10 s.
- **`Clock::tick(size) -> Tick`** runs exactly the order above. `last_size` stays a `usize`
  starting at `usize::MAX`, not an `Option`, so a reported size of `usize::MAX` behaves as
  today.
- **`periods(gossip_ms, poll_ms)`, `probe_loss(raw)`, `owns_period(raw)` and
  `chitchat(raw)`** take raw strings, as `policy::zone` does.
- **`policy::seeds(roster, advertise)` and `policy::in_cell(zoned, zone)`.**
- `main.rs`'s stale comment about the heal fix is corrected.

**Not changed:** the wiring above, every log line's text, the protocols, and the roster.

## Acceptance criteria

1. **Heal at `HEAL_PERIOD`, at the node's slot.** Polls of 200 ms, 1 s and 2 s give `heal_every`
   of 10, 10 and 5 views; heals fall where `ticks % heal_every == jitter`. Parent: 50 at 200 ms.
2. **Ownership in seconds.** `owns_period` 5 is 5, 5 and 2 views at those polls (2 s: 5 s over a
   2 s view, rounded down); 0 never reports; unparsable means 5. Parent: 5 views at 2 s.
3. **One poll's order.** A view every `view_every` polls, numbered from 1; a change on the first
   poll and on each size change only; heal and ownership only on view polls, after the count.
4. **Settings.** Each fallback in "What is true today" holds, for absent, unparsable and zero.
5. **Filters.** `seeds` drops only this node's address; `in_cell` keeps this zone, in order.
6. **Log lines.** Each of `JOIN node=`, `SELF advertise=`, `ANNOUNCE ok`, `VIEWCHANGE members=`,
   `VIEW t=`, `HEAL dialled=`, `REFOLD ok`, `REFOLD lost`, `REFOLD timed out`, `OWNS shards=` is
   in `main.rs` after as before.
7. **`main` decides nothing.** `sed 's://.*$::' crates/pstore-node/src/main.rs | grep -nE '%|is_multiple_of|last_size|\.filter\(|==|!=|\.max\('`
   prints nothing.
8. **Gates.** `./scripts/gates.sh` is green; the sweep over `schedule` and the two filters
   misses 0, with no equivalent among the mutations the Test plan names.

## Test plan

Unit tests in `crates/pstore-node/tests/schedule.rs` and `tests/policy.rs`. A test that fails
because the API does not exist yet is not "seen red" (spec review M4). Criteria 1 and 2 are red
on the parent's arithmetic, since the tests compute it. Every row's mutation is applied by
hand, and `VERIFIED.md` names the test that caught it.

| # | Test | Mutation it must catch |
|---|---|---|
| 1 | `heals_at_the_heal_period_at_its_slot` | `heal_every` in polls (the parent); the slot |
| 2 | `ownership_is_reported_in_seconds` | `owns_every` in views (the parent); the zero guard |
| 3 | `one_poll_runs_in_mains_order` | `ticks` incremented after the checks; `last_size`'s start |
| 4 | `settings_fall_back_as_main_did` | each fallback dropped |
| 5 | `seeds_and_in_cell_filter_exactly` | each filter's comparison |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| The heal (a roster read, and a refold when the union grows) | unchanged | unchanged | unchanged | 0 | unchanged |

At a sub-second poll, heals become 5× more frequent, back to the `HEAL_PERIOD` they were sized
for: one roster GET per node per 10 s, and no PUT once converged (`union_to_publish`).

## Risks

- **The fleet harness is not run here.** `scripts/cluster.sh` needs Docker. The ledger records the
  fleet run as `NOT-RUN`. Unit tests pin every decision; criterion 6 pins the scraped text.
- **Ownership reports change frequency at a poll above 1 s**, as the fix intends. `cluster.sh`
  counts `VIEW` lines, not `OWNS` lines, so no scraped count moves.

## Tasks

- **M31.1** — `schedule`, the filters, the cadence fix, `main` rewired, and the tests.
- **M31.2** — The ledger, `BACKLOG.md` row 34 closed, and the roadmap row.
