# M9c — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

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
5. **Newest wins, deleted is gone** — `the_newest_version_wins_wherever_the_older_one_is`,
   `a_deleted_id_is_gone_until_it_is_written_again`,
   `within_a_request_the_later_duplicate_wins_and_deletes_apply_last`
   (`cargo test -p pstore-server --test upsert`), **observed red** on the M9c.1 server;
   and `a_scan_sees_only_each_ids_newest_version`
   (`cargo test -p pstore-engine --test upsert`), seen red with the scan's shadow removed.
6. **Compaction and `as_of`** — `a_fold_racing_a_compaction_resurrects_nothing` (the fold is
   forced between the merge's seal and its commit), seen red with the snapshot check disabled;
   `compaction_drops_deleted_rows_and_buries_their_vectors`, seen red with compaction ignoring
   the vectors; `gc_never_reaps_a_live_vector`, seen red with GC's live set lacking HEAD's
   vectors (the state is built by `commit_head_for_test`, since the protocol never buries a live
   vector); `time_travel_sees_the_version_that_was_current` (server, observed red on M9c.1).
   HEAD's section: `a_head_without_delete_vectors_decodes_as_none` (`--test head`).
7. **Top-k under supersession** — exact: `a_mostly_superseded_segment_still_answers_top_k`
   (server, 45 of 50 folded rows superseded, observed red on M9c.1) and
   `unfolded_upserts_of_the_best_rows_still_leave_top_k` -- ⚠️ added at code review, which
   found the widening by the shadow untested; seen red with it removed. Clustered:
   `a_mostly_superseded_clustered_segment_still_answers_top_k` (600 rows, 540 deleted), seen red
   with `p` unscaled. Code review also found the shadow check fetching every widened
   candidate's documents -- a read growing with the deleted count; candidates are now cut to
   `limit + |shadow|` first, and `the_shadow_check_reads_the_top_candidates_not_every_widened_one`
   pins the bytes (56,836 against 29,124 before the cut; equal after).
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

   **M9c.2:** `./scripts/mutants.sh --check . --in-diff <the M9c.2 diff>` reached **93 of 128**
   before the container restarted (73 caught, 16 unviable, 4 missed); the 35 it never reached,
   and every function changed since, were swept with
   `--check 'write_documents|widened|resolve_rows|query_rows_filtered|is_tombstone|fetch_rows|as_of|run|open'`,
   which ran out of disk after 50 of 51 (32 caught, 15 unviable, 3 missed). Each miss resolved:
   `Head::as_of`'s newest-vector comparison deleted (at most one vector per segment can be
   current at an epoch -- each is buried when its successor is written; verified at review),
   the shadow gate's redundant `&&` deleted, the server's `deleted > 0` guard deleted, and
   the text and sparse legs' widening pinned by `text_and_sparse_legs_are_widened_past_deleted_rows_too`.
   Re-swept `--check 'widened|write_documents'`: **17 tested, 12 caught, 5 unviable, 0 missed**.
   Code review: two rounds (block on the two majors in criterion 7, then pass).
   `./scripts/gates.sh` on the committed tree: all fifteen PASS.
