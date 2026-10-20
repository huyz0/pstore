# M115 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores, debug profile without debug
info. "Under load" is eight busy shell loops running for the whole comparison. The CI failure
was under `cargo llvm-cov`, which this did not use: the load stands in for its slowness.

⚠️ **The first fix was wrong, and was dropped.** Closing and reopening the core every 32
inserts made the test fail 19 of 20 runs under load, and 5 of 5 with none: each reopen
starts a fresh block, so the split burst held less. The spec records it.

⚠️ **A second defect was seen and is not fixed here.** One old run hung for 30 minutes at
0% CPU. gdb showed `foyer`'s `BlockManager::on_reclaim_finish` taking its state lock, spawning
the next reclaim onto a runtime that was shutting down, and the dropped task's
`ReclaimingBlock` re-entering `on_reclaim_finish` on the same lock: a self-deadlock in
foyer-storage 0.22.6, the latest release. It is outside this change, and is reported to the
owner.

1. `disk::a_waited_write_is_on_disk_before_close` passes, 5 of 5. **Red first**, with
   `CacheCore::wait`'s body removed: `the value was not on disk after wait`, `left: 0`,
   6 of 6. Mutation: `./scripts/mutants.sh --check wait --in-diff <staged diff> --test-workspace=false --test-package pstore-cache`
   found 2 mutants, 2 caught.
2. **Red under load.** The old `disk::a_bulk_burst_leaves_meta_on_disk`, 30 runs alternated
   with criterion 3's: 24 passed, 5 failed `bulk lost its share of the disk` (`left` 260 to
   269 against 257), and 1 hung (the foyer deadlock above). A first batch of 20 under the
   same load failed 3.
3. **Green under the same load.** The new `disk::a_bulk_burst_leaves_meta_on_disk`, the other
   30 of those runs: 30 passed.
4. `grep -n "core.wait().await" crates/pstore-cache/tests/disk.rs` prints two lines: 205, in
   `disk::a_bulk_burst_leaves_meta_on_disk`, and 258, in criterion 1's test. The diff of
   that file against `HEAD` removes no line, so no assertion changed.
5. The whole crate passed (`cargo test -q -p pstore-cache`: 0 failed), with clippy and fmt.
   `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.
