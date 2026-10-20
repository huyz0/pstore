# M117 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores. The failure this fixes is
macOS-only, and these timings are `provisional`. The hand mutations were applied to a copy
and reverted; none is in the tree.

1. `parallel_legs::legs_are_scored_in_parallel` passes, 6 of 6 runs, each finishing in
   0.11 s. clippy and fmt pass for `pstore-query`.
2. `parallel_legs::legs_are_scored_in_parallel` fails under a global lock taken before the
   timer, 2 of 2: `16 leg reads took 405.228392ms: serial is 402.457208ms as held (400ms as
   asked)`, and 420.110215 ms against 417.043921 ms. A first version of this mutation took
   the lock after the timer started, so the wait counted as held, and it passed. That was
   the mutation's error, not the test's.
3. `parallel_legs::legs_are_scored_in_parallel` with every hold slowed to 4×: the old
   assertion fails at `took 403.329264ms: serial is 400ms`; the new one passes in 0.41 s.
4. NOT-RUN: macOS. This needs CI's `portable (macos-latest)` job on the commit after M117,
   and is read there, not here.
5. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.
