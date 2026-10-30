# M161 verification

Implemented in `crates/pstore-engine/src/lib.rs` (`Engine::indexes_at`, which returns
the names with the epoch of the HEAD they were read from; `indexes` delegates to it),
`crates/pstore-server/src/lib.rs` and `src/types.rs` (the page's `epoch`, and the walk's
model on `IndexList::epoch`), and `docs/deploy.md` ("Listing indexes while they change").
All runs are in a 4-core cloud container, and every number is `provisional`.

How to run: `cargo test -p pstore-server --test it list_walk::`.

Red run (.harness/m161-red.log): both tests were written before the field existed and
failed at the page helper's "a page reports its epoch".

Mutations, applied one at a time by script and reverted, each running the engine
`accessors::` tests and the server `list_walk::` tests (.harness/m161-mutations.log,
.harness/m161-mutations-r2.log, the second rerunning the first mutant after its anchor
was made unique). The script reports a compile error as invalid, not as a kill, and none
was invalid. All 4 were killed: the page's epoch answered as 0, `indexes_at` answering
epoch 0 or the epoch after HEAD's, and `indexes` answering no names.

No `cargo mutants` run is claimed.

1. PASS: `list_reports_its_epoch`. With no HEAD a page reports epoch 0; after seven
   creates every page reports the last create's epoch.
2. PASS: `list_walk_under_changes`. Twelve names, pages of five; between the first two
   pages another process creates `n07x` past the cursor, drops `n08` past it, drops
   `n01` already listed, creates `m` before it, and drops and re-creates `n10`. The walk
   is exactly `n00`..`n07`, `n07x`, `n09`, `n10`, `n11`: no name twice, `n01` once,
   `n08` and `m` absent, `n10` once. The second page's epoch is above the first's, and
   the third's equals the second's.
3. PASS: `list_walk_under_changes`. With the cursor's index `n03` dropped, the next page
   is `n04`..`n07`.
4. PASS: `list_reports_its_epoch`. A walk of three pages with no change between them
   reports one epoch, and its names equal one page of 1000.
5. PASS: `list_reports_its_epoch`. A page costs one blob read and no LIST, with no HEAD
   and with one, and holds `epoch`.
6. PASS, with the IPv6 exception M127 records.
   - `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D
     warnings` passed (.harness/m161-clippy.log).
   - The workspace suite passed 2,374 tests with none failing
     (.harness/m161-quality-suite.log). It ran with all features and both `[::1]` tests
     skipped: the container has no IPv6, and those tests run in CI.
   - `./scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed at
     97.19% lines and 95.76% regions (.harness/m161-coverage.log).
   - `scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed
     (.harness/m161-quality.log). `check-links` failed in that run only on the links to
     this file, which did not exist yet; it was run again after this file was written.

Code review: see the commit message.
