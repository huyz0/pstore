# M113 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. **No Mac was available.** Linux's wall
clock moves in nanoseconds, and here the old code's 10,000 identities were all distinct, so
the collision itself was not reproduced. The red that was observed here is criterion 3's.

1. `policy::identities_in_one_process_never_collide` passes. Its distinctness assertion was
   **green on the old code here**, as recorded above. On macOS the same collision failed CI's
   two-call test, quoted in the spec.
2. `policy::an_identity_is_fresh_on_every_start` passes, unedited (`cargo test -p pstore-node
   --test policy`: 18 passed, 0 failed).
3. `policy::identities_in_one_process_never_collide` checks the counter's bits. **Red before
   the fix:** `consecutive identities share the counter's bits`, `left: "00000000"`. Red again
   with the counter removed by hand, and with its shift moved from 64 to 63.
4. `./scripts/mutants.sh --check fresh_node_id --in-diff <working diff> --test-workspace=false --test-package pstore-node`:
   2 mutants, 2 caught. ⚠️ Not the bare command, which mutates only committed changes. The
   two shift mutants in criterion 3 were applied by hand, because the tool generates none.
   `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.
