# M8f — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

Unless a line says otherwise, runs are in the Linux dev container. Every "killed" was a mutant
applied by hand, one at a time, restored afterwards, and confirmed to fail the named test's
assertion rather than the build.

1. **The timeout scales with the log of the fleet** —
   `the_suspect_timeout_grows_with_the_log_of_the_fleet` (`cargo test -p pstore-gossip --lib`):
   6 at one member, 63 at 2^20. **Mutation verified killed**: `3 * log2` → `3 + log2`.
2. **A change rides exactly `RETRANSMITS` times** — `a_change_rides_along_exactly_retransmits_times`.
   **Mutations verified killed**: `*sent < RETRANSMITS` → `==`, and → `>`.
3. **Re-noting replaces only its own entry** — `noting_a_member_again_replaces_only_its_own_entry`
   asserts the pending list is exactly `[B, A]`. **Mutation verified killed**: `!=` → `==`.
4. **The revisit rule** — `the_dead_are_revisited_on_revisit_ticks_and_whenever_no_one_is_alive`,
   `tick` set directly, 64 seeds per tick. **Mutations verified killed**, all four: `&&` → `||`
   in the buried filter, `||` → `&&` in `revisit`, `&&` → `||` in the candidate choice, and the
   deleted `!`.
5. **The mixing matches an independent model** — `peer_selection_mixing_matches_an_independent_model`,
   the Python model recorded in its doc comment. **Mutations verified killed**, all seven: `^` →
   `|` on the seed, and `^=` → `&=`, `^=` → `|=`, `>>` → `<<` in each of the two finalising steps.
   The bound tests `a_change_is_retransmitted_a_bounded_number_of_times` and
   `probes_rotate_across_peers` are unchanged.
6. **A hostile member count is refused without reserving it** —
   `a_hostile_member_count_is_refused_without_reserving_it` (`cargo test -p pstore-gossip --test wire`).
   ⚠️ With `Vec::with_capacity(n)` inserted it **survived in the dev container**, which runs
   with `overcommit_memory=1`, so a 343 GB reservation succeeds lazily — exactly the platform
   limit spec review predicted. Observed red **natively on Windows** instead
   (`cargo test -q -p pstore-gossip --test wire`): `memory allocation of 343597383600 bytes
   failed`, process aborted; green again once restored. The pin therefore works where the OS
   refuses the reservation — Windows, and Linux under the default overcommit heuristic (expected
   of CI's ubuntu runners, **not checked**; CI's `windows-latest` job does run it) — and cannot fail on macOS or with overcommit=1. The guard is gone:
   `grep -n 'buf.len()' crates/pstore-gossip/src/wire.rs` shows the comment explaining its
   absence and `finished()`, and no comparison in `members`.
7. **No survivor in the two files** — `./scripts/mutants.sh --check "pstore-gossip/src/(protocol|wire)\.rs"`
   (`--file` cannot run on this crate: see the flagged `mutants.sh --file` defect; `--check`
   tests the whole workspace). The first sweep found **four more** in these files:
   `mark_alive` emptied, `helpers` returning `vec![String::new()]` or `vec!["xyzzy"]`, and
   `put_member`'s `len > 0` → `>=`. Added `a_suspect_that_speaks_is_not_buried_on_the_old_timer`
   and `indirect_probes_go_to_live_peers_other_than_the_target`; **mutations verified killed**
   by hand, all three. `put_member` restructured so the comparison does not exist. Re-swept:
   144 tested in 17m, **0 missed in `protocol.rs` or `wire.rs`**. The regex also selects 16
   mutants elsewhere through their descriptions; 6 of those were missed (`pstore-engine`'s
   `commit_stale_for_test`, `pstore-testkit`'s `flaky.rs` and `sweep.rs`) and are outside this
   milestone — the queued `pstore-engine` and `pstore-testkit` work.
   ⚠️ An earlier attempt hung for ~20 hours: the dev container had no init process, zombies
   exhausted the VM's PID space, and builds failed with EAGAIN — fixed in `c889d5d`.
8. **The full gate** — `./scripts/gates.sh` in the dev container on this tree: all fifteen gates PASS.
