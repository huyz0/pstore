# M24 — A peer's zone, once learned, is never lost

**Serves:** [BACKLOG](../BACKLOG.md) row 33, and D-79 (one placement ring per AZ), whose rings
are built from the zones gossip carries. The zone is in a member's checksum (`cluster.rs`).

## What is true today

- A peer first learned from a seed, a `dial` or a bare probe is joined with an **empty** zone
  (`Protocol::learn`), and that record is gossiped as news (`note_update`).
- The peer's own record (incarnation 0, `Alive`, its zone) never replaces it. `absorb` applies a
  record only at a higher incarnation, or a worse state at an equal one.
- So two members in `az-a` and `az-b`, loss-free, still hold each other's zone as empty after 200
  rounds (M8e's spec review, in simulation). The checksum covers the zone, so their views never
  agree, and they exchange **4 `Sync` datagrams per round forever**. `pstore-node` publishes
  only peers whose zone matches its own, so each is missing from the other's cell roster.
- The reverse also happens. `apply` takes a winning record's zone along with its state. A worse
  state at an equal incarnation carrying an **empty** zone replaces a zone that was known. A
  `Suspect` built by a node that learned the peer from a probe does that.

## Delta

**A zone is self-declared, and an empty one means "unknown", never "none".** In `pstore-gossip`:

- **Fill.** At an equal incarnation, a record carrying a zone fills a member's empty one,
  whatever the two records' states. ⚠️ **A fill changes the zone and nothing else** (spec review
  M1): not the state, the incarnation or the suspicion clock. An `Alive` record filling a
  `Suspect` or `Dead` member's zone at an equal incarnation is the stale resurrection the cluster
  forbids (`cluster.rs`), so a fill is never applied as a winning record. A record that wins on
  precedence carries its zone in as today. The fill is news (`note_update`), so it spreads by
  piggyback, and it goes through `Cluster`'s incremental checksum like any change.
- **Never clear.** A record with an empty zone never replaces a known one, at any incarnation.
  `apply` keeps the member's zone when the winning record's is empty.
- **Never move at an equal incarnation.** A record carrying a different non-empty zone at an
  equal incarnation does not replace the known one. A member that moves zone does so at a higher
  incarnation, which replaces it as today.
- **`Cluster::join` is not changed.** No caller passes a zone for a member already known: `dial`,
  the seeds and `learn` all pass an empty one, and `absorb` joins only unknown members.

**Not changed:** the wire format, incarnation and state precedence, `refute`, the checksum,
chitchat, and `pstore-node`'s roster rule.

## Acceptance criteria

Each simulation criterion is polled after every round, and passes at the first round where it
holds.

1. **Two members in two zones learn each other's zone.** In the lockstep simulation, two members
   in `az-a` and `az-b`, each seeded with the other's address and an **empty** zone, as
   `pstore-node` seeds (`swim.rs`), loss-free, each hold the other's zone within 10 rounds.
2. **And then their views agree.** From then on, over 50 rounds, they exchange no `Sync`
   datagram: only checksums.
3. **A fleet converges on every zone.** Six members over three zones, seeded from one member's
   address with an empty zone, loss-free: within 30 rounds every member holds every other's
   zone, and the six checksums are equal. At a 10% drop rate, within 100 rounds.
4. **An empty zone never clears a known one.** A `Suspect` record at an equal incarnation with an
   empty zone leaves the member `Suspect` with its zone. The same holds for a record with an
   empty zone at a higher incarnation.
5. **An equal incarnation never moves a zone.** A record at an equal incarnation with another
   non-empty zone leaves the known zone. One at a higher incarnation replaces it.
6. **A fill never revives.** An `Alive` record with a zone, at an equal incarnation, fills the
   empty zone of a `Suspect` member and of a `Dead` one. Each keeps its state and incarnation, and
   the `Suspect` one is still declared dead on its original schedule.
7. **A fill is news, and keeps the checksum honest.** After a fill, the next piggyback carries
   the member with its zone. After every zone change in tests 4–7, `checksum()` equals
   `checksum_from_scratch()`.
8. **Gates.** `./scripts/gates.sh` is green, and the mutation sweep over M24's source diff misses
   0, every miss closed by a test or named as equivalent.

## Test plan

Simulation tests in `crates/pstore-gossip/tests/protocol.rs`. Its `Sim` gains per-node zones, a
seeding that joins with an empty zone, and a count of `Sync` datagrams. Unit tests go through
`receive` with hand-built records.

| # | Test | Must fail first because / mutation it catches |
|---|---|---|
| 1 | `two_zones_learn_each_others_zone` | today: zones stay empty (M8e's 200 rounds) |
| 2 | `once_zones_are_known_no_sync_is_sent` | today: 4 `Sync` per round |
| 3 | `a_fleet_over_three_zones_converges_with_and_without_loss` | today: no zone is learned |
| 4 | `an_empty_zone_never_clears_a_known_one` | today: a worse record clears the zone |
| 5 | `an_equal_incarnation_never_moves_a_zone` | a fill that replaces any zone, not only an empty one |
| 6 | `a_fill_never_revives` | a fill applied as a winning record |
| 7 | `a_fill_is_news_and_keeps_the_checksum` | no `note_update`; a fill that bypasses the checksum |

## RA budget

No blob requests: gossip is UDP between nodes. Datagrams per round in a converged fleet fall from
4 per mismatched pair to 0. A fill is news, so it rides each of its piggyback slots once.

## Risks

- **A member that lies about its zone.** Zones are self-declared today, and this changes nothing
  about that. A first lie at incarnation 0 now spreads where before it stayed local. It is equally
  wrong either way.
- **A member restarted under the same id in another zone, at the same incarnation.** It holds its
  own new zone and ignores records about itself, while peers keep the old one: their views stay
  split, with `Sync` forever, the same symptom as row 33, and nothing raises the incarnation.
  This predates M24 and M24 does not make it worse. A zone move needs a higher incarnation, which
  nothing produces today.

## Tasks

- **M24.1** — `absorb` and `apply`, and tests 1–7. The comments M24 makes wrong: `upsert`'s
  "replaces rather than merges", `apply`'s, and the "do not learn each other's zone" note in
  `crates/pstore-node/tests/swim.rs`.
- **M24.2** — The ledger, `BACKLOG.md` row 33 closed, and the roadmap row.
