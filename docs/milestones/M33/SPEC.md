# M33 — Gossip's cost under loss, measured on a model of the real network

**Serves:** [BACKLOG](../BACKLOG.md) row 50, which [M24](../M24/VERIFIED.md) opened: under constant
loss a gossip fleet's checksums almost never agree, and the row asked for a measurement of
`Sync` bytes against loss "before a fix is chosen". This milestone takes that measurement, on
both the simulator's model and a model of the real network. It changes no production code,
and it hands the design question to M34 with the numbers it needs.

## What is true today

- `pstore-gossip`'s lockstep `Sim` delivers one hop per period: a reply arrives the period
  after its request.
  - Its comment explains why: an earlier version delivered within the round, and certified a
    protocol that flapped on a real fleet.
  - But a real node (`pstore-node/src/swim.rs`) handles each datagram as it arrives, and only
    *probes* once a period. A hop is milliseconds against a 1 s period.
- **On the `Sim`'s model, the indirect probe can never succeed** (spec review, round 1). A
  direct probe is two hops and waits `PROBE_TIMEOUT` (3) periods. The indirect round adds two
  more hops (ping-req, then the relayed ack), and is re-armed with the same 3. So every lost
  direct ping or ack becomes a suspicion there.
  - On a real network the four hops take milliseconds, and the round completes. **The
    production constant is not the bug; the model is pessimistic about it.**
- **Measured** in a probe that was not committed: seeded fleets, 50 loss-free rounds, then 400
  at every Nth datagram dropped. "Waves" is how many hops are delivered within one period: 1
  is the `Sim` today, 4 is a period long enough for the indirect round, as on a real
  network.

| Fleet, loss | 1 wave: B/node/round, rounds agreeing | 4 waves |
|---|---|---|
| 6, 2% | 566, 57 | 163, 286 |
| 6, 10% | 1,296, 6 | 643, 74 |
| 20, 2% | 2,396, 4 | 98, 395 |
| 20, 10% | 4,108, 4 | 2,748, 5 |
| 50, 2% | 6,574, 4 | 1,786, 54 |
| 100, 2% | 12,579, 4 | 5,533, 8 |
| 100, 10% | 16,738, 4 | 15,225, 5 |
| any, 0% | 74, 400 | 74, 400 |

  - Row 50's measurement was largely the model's: at 20 members and 2% loss, a realistic
    period agrees in 395 of 400 rounds.
  - **But the cost still grows with the fleet** under heavier loss, or at 50 members and up.
    The extra bytes are `Sync`s, each the whole member list, sent on every exchange between
    members whose checksums differ.
- **Throttling `Sync` is not the fix.** Measured on the 1-wave model and reverted:
  - no `Sync` while updates are pending;
  - a per-peer cooldown of 3 or 8 periods;
  - a global one-per-10-periods limit.
  - Each either barely moved the bytes, or cut them ~10× and **manufactured deaths**: as few
    as 94 of 100 alive, with `a_lossy_network_does_not_manufacture_deaths` failing. `Sync` is
    what rescues a refutation whose piggybacked copies were lost.

## Delta

1. **`Sim` gains `waves`**: how many hops it delivers within one period, default 1, today's
   model and its every test unchanged. A period's deliveries run in waves, each wave's
   replies delivered by the next, up to `waves`; whatever remains in flight is delivered next
   period, as now. Timeouts still count periods.
2. **Two pinned measurements**, one per model, at the points above. They are bounds, not
   goals, so a change to the protocol that moves them must move them on purpose.
3. **BACKLOG row 50 closes, and row 52 opens:** the design question, with these numbers, the
   throttles tried and the deaths they caused. M34 takes it.

**Not changed:** any production code; every existing test; the default model.

## Acceptance criteria

1. **The default model is unchanged.** Every existing `pstore-gossip` test passes, with `waves`
   defaulting to 1.
2. **The real-network model completes an indirect round.** A three-member `Sim` with 4 waves:
   the test removes A's direct ping to B from `in_flight` by hand, delivers everything else,
   and A never suspects B. With 1 wave, A suspects B.
3. **The measurements are pinned.** At 20 members and 2% loss with 4 waves: at least 350 rounds
   agreeing and at most 200 B/node/round. ⚠️ **Tripwires, not floors**, which M34 is meant to
   trip: at 50 members and 10% loss with 4 waves at least 4,000 B/node/round, and with 1 wave
   at 20 members and 2% at most 10 rounds agreeing. Debug run time is recorded.
4. **Row 50 closes, row 52 opens**, carrying the table, the throttles and the deaths.
5. **Gates.** `./scripts/gates.sh` is green. No production source changes, so the sweep's
   in-diff set is empty, and that is recorded.

## Test plan

In `crates/pstore-gossip/tests/protocol.rs`.

| # | Test | Red because / mutation it catches |
|---|---|---|
| 2 | `a_real_network_completes_an_indirect_round` | 1 wave suspects (the parent's model); waves ignored |
| 3 | `gossip_cost_under_loss_on_both_models` | the waves loop delivering nothing extra; the default changed |

The tests use `waves`, which does not exist on the parent, so their red is the mutation named,
recorded in `VERIFIED.md`.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| None: no production change | 0 | 0 | 0 | 0 | unchanged |

## Risks

- **The waves model may be generous where the 1-wave one is harsh.** A real period can contain a
  slow hop. The two models bracket the truth rather than locate it, which is why both are
  pinned.
- **Criterion 3's tripwires pin a problem.** They exist so that M34's fix shows as a red test to
  move on purpose. Lowering them then is the fix landing, not a threshold weakened, and row 52
  says so.
- **Run time in `cargo test`.** The 50-member case is the cheapest that shows the storm, chosen
  over 100 members. Every nightly mutant reruns it, so its time is measured and recorded.

## Tasks

- **M33.1** — `waves`, and tests 2–3.
- **M33.2** — The ledger, row 50 closed and row 52 opened, and the roadmap row.
