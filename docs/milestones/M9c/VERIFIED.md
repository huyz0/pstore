# M9c — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).
M9c.1 is delivered here; M9c.2's criteria are **NOT-RUN** until that task lands.

1. **Visible during a flush** — `rows_stay_visible_while_their_flush_is_in_flight`
   (`cargo test -p pstore-engine --test memtable_races`): the bundle PUT is held by the test's
   store while the query runs. **Observed red** on the M9b engine (the row was missing).
   ⚠️ Extended at code review, which found a drain of ALL of `pending` (not the first `n`)
   survived every test: a row written during the held PUT must stay pending and survive the
   next fold. That hand mutation now fails it.
2. **A fold keeps a later flush** — `a_fold_keeps_rows_flushed_after_it_read_the_lane`: the
   fold's HEAD commit is held while a second flush lands. **Observed red** on the M9b engine.
3. **Folded elsewhere, served once** — `rows_another_process_folded_are_served_once`.
   **Observed red** on the M9b engine (`a` and `b` twice).
4. **Prune and stale HEAD** — `a_prune_drops_exactly_the_batches_a_watermark_folded`,
   `a_head_older_than_a_prune_is_reported_stale` (`cargo test -p pstore-engine --lib`), and
   `a_refused_flush_leaves_its_rows_where_they_were`, which passed on the M9b engine too: it
   pins behaviour the rewrite had to keep. Row 37's fix is structural and **OBSERVED-NOT** red:
   no store can force the interleaving between two queries' lock acquisitions.
5. NOT-RUN — M9c.2.
6. NOT-RUN — M9c.2.
7. NOT-RUN — M9c.2.
8. **Gates** — the mutation sweep over M9c.1's diff,
   `./scripts/mutants.sh --check . --in-diff <the engine diff>`: **35 tested in 19m, 25 caught,
   7 unviable, 3 missed**, each rewritten away: `prune`'s `watermark > pruned` guard (an older
   watermark's `retain` is a no-op) and the flush's generation bump (the moved rows keep their
   order). Re-swept: `./scripts/mutants.sh --check Memtable::prune --file crates/pstore-engine/src/lib.rs`
   7 of 7 caught, and `--check flush_inner` 5 of 5 caught after review's changes. ⚠️ Round 2
   of code review blocked the one behavioural change of round 1's minors -- dropping a batch
   already folded, which removed rows without a generation bump, so a cached fresh view served
   them twice while the index stayed idle -- and it was **reverted** to round 1's push (healed by
   the next prune), with the residual window named in a comment. The rest of round 2 passed.
   `./scripts/gates.sh` on the committed tree (M9c.1): all fifteen PASS. The stale-HEAD re-read in the fresh view and the scan is
   pinned only at the memtable (review minor), not by an engine test.
