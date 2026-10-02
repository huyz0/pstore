# M29 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0), against
`MemoryStore`. No number here is a latency.

Commands: `cargo test -p pstore-engine --test segment_names` and `--test abandon`.

1. **The abandoned segment is buried.** `a_committed_compaction_buries_a_name_it_abandoned`:
   - The merge creates its segment at the first name, is refused the planted table, and
     commits at `_1`.
   - The abandoned segment is in the graveyard under its key epoch.
   - Red on the spec's commit (`f5d7e81`): the graveyard held only the inputs and bundles.
   - Killed: the burial call removed.
2. **And reaped.** `a_committed_compaction_buries_a_name_it_abandoned`: after `gc(0)` the abandoned segment is gone, and the merge and
   its rows remain.
3. **Nothing live is buried.** The same test, and `a_refused_name_is_never_buried`. The planted
   table is untouched before GC and gone after it.
4. **Lost attempts are still buried.** `a_committed_compaction_buries_its_lost_attempts_seal`.
   - ⚠️ Amended at implementation: with the burial call removed, no existing test in the
     engine's suite failed, so this test pins it. A lost CAS, a re-seal, a commit, and the
     first seal buried and reaped.
   - It passes on the parent, whose `stale` loop did this. Killed: the burial call removed.
5. **Row 43 pinned.** `a_contended_branch_retry_buries_no_live_copy` passes on the parent, as the
   spec says.
   - Seen red with M23 reverted (`take` ignoring its counter, and a create that replaces).
   - The red was the copy key repeating, one object where two are expected. It was not the
     live-and-buried assertion, which that run never reached.
6. **Gates.**
   - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `aebd36f`, and on this ledger's
     commit.
   - The sweep over M29's source diff (`f5d7e81..aebd36f`), against the segment-name and
     abandon test files: 2 mutants, both **timeouts**, 0 missed.
     - Each replaces the compaction's attempt loop whole, and the pause-based tests then wait forever for
       a compaction that never runs.
     - `cargo mutants` does not delete statements. So the one-line change, the burial, is
       covered by the hand mutation in criteria 1 and 4.
   - ⚠️ Not `./scripts/mutants.sh` itself: `cargo mutants --in-diff` was run directly.

Spec review took two rounds, each approving with majors folded:
- round 1: test 5 had no red, and could pass with no copy;
- round 2: its stated red reason was wrong.

Code review took one round, which passed with one minor, recorded in `aebd36f`'s message.
