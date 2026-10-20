# M116 — a timed dead peer fails at once on every OS

**Serves:** CI's `portable (windows-latest)` job, red on `main` at `a9e1c03`:
`peers::failed_shares_are_run_here_together` failed at `tests/peers.rs:944` with
`two failed shares took 3.6344759s: more than six rounds of 400 ms`. No backlog row: a
defect in a test.

## What is true today

- **The test's dead peer is a closed port.** It binds a listener, reads its address, and
  drops it, so a part sent there fails to connect.
- **How long that takes depends on the OS.** Linux answers a connect to a closed loopback
  port with a reset, and the client fails at once. Windows retries the SYN after a reset
  before reporting the refusal, so the same connect takes a second or more. ⚠️ This is the
  probable cause, from Windows' documented behaviour, not measured here: this container
  runs Linux only. 3.63 s is 1.2 s over six rounds, which is about what one slow refusal
  beside a 400 ms round would add.
- **Only this test times the query.** The other dead-peer tests count failed parts and
  compare answers, which a slow refusal does not change.

## Delta

1. In `failed_shares_are_run_here_together` only, the dead peer is a listener that accepts
   each connection and closes it at once. A part sent there fails within one loopback round
   trip on every OS, with a transport error, as a closed port's did on Linux.

**Not changed:** the bound (2.9 s), any assertion, the other tests' dead peers, and
production code.

## Acceptance criteria

1. `peers::failed_shares_are_run_here_together` passes here, with the same failed-part count
   (`before + 2`) and the same bound.
2. **Windows.** CI's `portable (windows-latest)` job passes this test on the commit after
   M116. ⚠️ Its red is observable only there: Linux refuses a closed port at once, so this
   test cannot fail here on the old fixture.
3. `./scripts/gates.sh` passes.
