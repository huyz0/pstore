# M117 — parallel legs are timed against the holds as measured

**Serves:** CI's `portable (macos-latest)` job, red on `main` at `5437167` and `8e1118a`:
`parallel_legs::legs_are_scored_in_parallel` failed at `tests/parallel_legs.rs:221` with
`16 leg reads holding 25 ms each took 397.237208ms: serial is 400ms` (and 405.648 ms at
`5437167`). No backlog row: a defect in a test.

## What is true today

- **The test's serial time is declared, not measured.** It is 25 ms times the reads,
  while each read holds its thread with `std::thread::sleep(25 ms)`. A sleep can run long,
  never short, so on a runner that oversleeps, the declared sum understates what running
  the reads one after another would cost.
- **This reproduces macOS's number here.** With every hold slowed to 4× (100 ms instead
  of 25 ms), the old assertion fails at `took 403.329264ms: serial is 400ms`, the same
  shape as CI's 397 ms and 405 ms. ⚠️ That macOS oversleeps this much is inferred from the
  match, not measured on macOS.
- **The deterministic checks pass on macOS.** `most >= 2` comes before the timed
  assertion and passed in both CI failures.

## Delta

1. `Legs` sums how long each blocking read actually held its thread, in nanoseconds.
   The timer starts after anything the read waits on and stops after the sleep.
2. `serial` is that measured sum. The bound is still `took < serial / 2`, which means two
   reads at once. The failure message reports both the measured and the declared sum.

**Not weakened:** if the reads run one after another, the query takes at least the
measured sum, so the assertion still fails. Numerically the bound can only grow, because
the measured sum is at least the declared one. What it asks for, at least twice the
overlap of what was held, has not changed.

**Not changed:** production code, the other assertions, the 25 ms hold, the worker count.

## Acceptance criteria

1. `parallel_legs::legs_are_scored_in_parallel` passes here.
2. **Still catches serial reads.** Hand mutation: a global lock taken before the timer,
   so the reads run one after another. The test fails.
3. **Tolerates oversleep.** Hand mutation: every hold slowed to 4×. The old assertion
   fails and the new one passes.
4. **macOS.** CI's `portable (macos-latest)` job passes this test on the commit after
   M117. ⚠️ Observable only there.
5. `./scripts/gates.sh` passes.
