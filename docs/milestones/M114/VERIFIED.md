# M114 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores. The failure this fixes is
macOS-only, so the datagram itself cannot be seen red here (criterion 3).

1. `swim::a_member_socket_holds_the_largest_frame` passes. **Red first**, with `swim::bind`
   binding without sizing the buffer: `send buffer 212992 is under 262144`
   (`tests/swim.rs:384`). That stub is the hand mutation "the `set_send_buffer_size` line
   removed", so it is caught. Mutation: `./scripts/mutants.sh --check bind --in-diff <staged diff> --test-workspace=false --test-package pstore-node`
   found 1 mutant, unviable, 0 missed.
2. `swim::the_largest_sealed_part_is_received` passes, with the rest of the `swim` suite
   (`cargo test -q -p pstore-node --test swim`: 19 passed, 0 failed). Only its sending
   socket's setup was added.
3. NOT-RUN: macOS. This needs CI's `portable (macos-latest)` job on the commit after M114,
   and is read there, not here.
4. `cargo deny check` printed `advisories ok, bans ok, licenses ok, sources ok`. The lock
   file gains one edge, `pstore-node` to `socket2`, and no package. `./scripts/gates.sh`
   passed, run by the pre-commit hook on this commit.
