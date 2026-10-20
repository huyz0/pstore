# M115 — the bulk-burst disk test waits for its writes

**Serves:** CI's `coverage` job, red intermittently on `main`:
`disk::a_bulk_burst_leaves_meta_on_disk` failed `bulk lost its share of the disk` with
`left: 305, right: 257` -- 48 extra store reads, from the burst's newest entries. No backlog
row: a defect in a test.

## What is true today

- **The burst is 64 MiB written in one loop**: 256 bulk inserts of a quarter block each, with
  no wait between them. The disk tier is written on admission (`WriteOnInsertion`).
- **`foyer` drops an insert it cannot queue.** foyer-storage 0.22.6's block engine skips an
  enqueue once the bytes queued exceed `submit_queue_size_threshold` (default 16 MiB), and a
  flusher drops an entry its buffer cannot take. pstore already documents this as intended:
  "an insert the tier cannot make is dropped, never an error" (`disk.rs`).
- **So the test asserts something no code promises.** When the flushers fall behind -- under
  coverage instrumentation, on a loaded runner -- the newest inserts are the ones dropped,
  and those are the ones the test reads back. The quota it means to check is not involved.
- **Closing and reopening is not a way to wait.** Tried first: each reopen starts a fresh
  block, so a burst split by reopens holds less, and the test failed with no load at all.
- **Nothing outside `close` waits for the tier.** `foyer` has `storage().wait()`; pstore does
  not expose it.

## Delta

1. `CacheCore::wait` waits for the disk tier's writes queued so far, through each class's
   `storage().wait()`, and leaves the tier open.
2. The test waits every 16 inserts (4 MiB, a quarter of the threshold).

**Not changed:** any assertion, the sizes, the quotas, and what the tier does with an insert
it cannot queue.

## Acceptance criteria

1. **`wait` waits.** `disk::a_waited_write_is_on_disk_before_close` finds an inserted value
   in the tier's files after `wait`, with the core open. Red first, against an empty `wait`.
2. **Red under load.** With the CPU saturated, the old `disk::a_bulk_burst_leaves_meta_on_disk`
   fails some runs out of 30 with the assertion CI saw.
3. **Green under the same load.** With the change, 30 of 30 runs, alternated with criterion
   2's under the same load, pass.
4. Every assertion in the burst test is unchanged: `git diff` adds only the wait.
5. `./scripts/gates.sh` passes.
