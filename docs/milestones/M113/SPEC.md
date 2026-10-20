# M113 — two identities from one process never collide

**Serves:** CI's `portable (macos-latest)` job, red on `main` on 2026-10-19 (23:09 UTC):
`policy::an_identity_is_fresh_on_every_start` panicked at `tests/policy.rs:149` with
`two starts produced the same identity`. No backlog row: a defect.

## What is true today

- **`fresh_node_id` is the wall clock in nanoseconds, XOR the process id shifted to the top
  32 bits** (`pstore-node/src/policy.rs`). Nothing else goes in.
- **macOS's wall clock moves in microseconds.** Two calls within the same microsecond in one
  process return the same identity. The test makes two calls back to back, so on a fast Mac
  they usually land in one tick. Linux's clock moves in nanoseconds, so the collision is rare
  there, but nothing rules it out.
- **One process calls it once in production** (`main.rs`). The defect is in the claim the
  doc comment makes, "fresh on every start", which the test checks by calling it twice.

## Delta

1. `fresh_node_id` also mixes in a process-wide call counter, into bits the clock and the
   process id leave unused: 64 to 95 (nanoseconds since 1970 need 61 bits; the process id
   sits at 96 and up). So two calls in one process differ in those bits, whatever the clock
   reads. Across processes, nothing changes: clock and process id, as before.

**Not changed:** the identity's format (32 hex digits), what main.rs does with it, and the
rule that it is never derived from IP or hostname.

## Acceptance criteria

1. **Ten thousand identities from one process are distinct.**
   `policy::identities_in_one_process_never_collide` makes 10,000 and finds no repeat. Red
   first, if the old code collides here; if it does not on Linux, that is recorded, and the
   red is macOS's, quoted above.
2. `policy::an_identity_is_fresh_on_every_start` passes, unedited.
3. **The counter is in bits nothing else uses.** The same test checks that bits 64 to 95 of
   two consecutive identities differ.
4. `./scripts/gates.sh` passes, and the mutation run on `fresh_node_id` misses nothing.
