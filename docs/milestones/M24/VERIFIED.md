# M24 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0). Gossip
is simulated in lockstep, so every round count here is exact and machine-independent.

Commands: `cargo test -p pstore-gossip` (unit tests in `protocol.rs`, simulation in
`tests/protocol.rs`), and `cargo test -p pstore-node --test swim`.

**Observed red.** All seven spec tests failed on the parent commit, `95f24a0`:
- the simulations: zones not learned in 10 rounds, never converged, and a fleet that did not
  converge in 30 rounds;
- the unit tests: a known zone cleared to `""`, a winning record moving a zone, no fill, and a
  fill that was not news.

`two_members_in_two_zones_learn_each_others_zone`, over loopback, failed on the old gossip code
with each member holding the other's zone as `""`.

1. **Two members in two zones learn each other's zone.** `two_zones_learn_each_others_zone`,
   seeded with an empty zone as `pstore-node` seeds. End to end:
   `two_members_in_two_zones_learn_each_others_zone`.
2. **And then their views agree.** `once_zones_are_known_no_sync_is_sent`: 0 `Sync` datagrams
   over 50 rounds once converged.
3. **A fleet converges on every zone.**
   `a_fleet_over_three_zones_converges_with_and_without_loss`, six members over three zones:
   - loss-free, every zone known and every checksum equal within 30 rounds;
   - at 10% loss, every zone known within 100 rounds.

   ⚠️ Amended at implementation, as the spec records. The lossy half passes on the parent
   commit too, so only the loss-free half tests M24. The checksum churn under loss is BACKLOG
   row 50: 7 of 400 rounds agree in this commit's measurement, and code review's harness
   measured 6.
4. **An empty zone never clears a known one.** `an_empty_zone_never_clears_a_known_one`, at an
   equal and at a higher incarnation.
5. **An equal incarnation never moves a zone.** `an_equal_incarnation_never_moves_a_zone`.
   `a_winning_record_fills_an_empty_zone` came from code review. It fails with `apply`'s
   known-zone condition dropped, applied by hand.
6. **A fill never revives.** `a_fill_never_revives`: a `Suspect` member keeps its state and
   incarnation, and is still declared dead at its timeout; a `Dead` member stays dead.
7. **A fill is news, and keeps the checksum honest.** `a_fill_is_news_and_keeps_the_checksum`.
   Each of tests 4–7 asserts `checksum() == checksum_from_scratch()` after every zone change.
8. **Gates.**
   - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `b874c7a`, and on this ledger's
     commit.
   - The sweep over M24's source diff (`95f24a0..b874c7a`): 13 mutants, 11 caught, 2 unviable,
     **0 missed**. It was run with `cargo mutants --no-config --in-diff`, testing `pstore-gossip`
     and `pstore-node`.
   - ⚠️ `--no-config`, because the repo's `mutants.toml` adds `--features
     pstore-blob/object_store`, which `pstore-gossip` alone cannot build with. That file's
     exclusions name other crates only.
   - ⚠️ Not `./scripts/mutants.sh` itself.

Spec review took two rounds: round 1 blocked (a fill must never revive), and round 2 passed.
Code review took two: round 1 blocked (a link to this file before it existed), and round 2
passed. Its minors m-3 to m-5 are in M24.1's commit.
