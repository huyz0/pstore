# M27 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0), against
`MemoryStore` behind a store that counts reads. Every number here is a request count, not a
latency.

Command: `cargo test -p pstore-engine --test centroid_skip`.

**Observed red** on the spec's commit (`b891476`), against a reader asking for every table: 3
of the 7 tests failed. There were 8 centroid reads over 8 small segments, and 31 warm reads
both with and without the 404s. The other 4 guard mutations and were checked by them.

1. **No 404 for a table that cannot exist.** `small_segments_ask_for_no_centroid_table`: 0
   centroid reads over 8 small segments, against 8 on the parent.
2. **A table that exists is still used.** `a_segment_at_the_threshold_still_uses_its_table`:
   one centroid read, a warm read count equal to the every-table reader's, and the same answer.
   Killed: `>=` as `>`, and the predicate answering false.
3. **Answers are unchanged.** `answers_are_unchanged`: dense, filtered and hybrid queries over 8
   small segments and one large, ranked equal to the every-table reader's. Also:
   - `a_large_fresh_segment_still_uses_its_table`. Killed: the fresh segment's table dropped
     (a sweep survivor, closed after code review).
   - `a_query_with_no_dense_leg_reads_no_table`. Killed: the open round's dense-leg filter
     dropped (likewise).
4. **`as_of` is unchanged.** `an_as_of_query_still_uses_a_resurrected_table` and
   `an_as_of_query_over_a_dropped_index_uses_its_tables`. Killed: `rows == 0` read as small,
   which code review also applied by hand.
5. **A higher threshold still answers correctly.** `a_higher_threshold_reader_scans_exactly`:
   no centroid read, ranked equal to an exact query.
6. **Cheaper by exactly the 404s.** `the_saving_is_exactly_the_404s`: 8 fewer reads than the
   every-table reader, warm. The server's `read_cache.rs` now pins a warm vector query at 1
   read (was `1 + segments`).
   - ⚠️ Amended at implementation: `served_epoch.rs` also pins reads. Its vector queries fell
     from 5 and 7 to 4 and 6, bytes unchanged.
   - ⚠️ The depth is not asserted (code review, minor): only parallel reads were removed.
7. **Warm agrees.** `warm.rs` is unchanged and passes, all 13 tests, among them
   `a_warm_reads_only_what_exists`, `every_sidecar_that_exists_is_warmed` and
   `an_index_without_dense_vectors_has_no_centroids_to_ask_for`. Killed by them: every viable
   mutation of warm's own terms but one (below).
8. **Gates.**
   - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `8bcca80`, and on this ledger's
     commit.
   - The sweep over M27's source diff (`b891476..8bcca80`), against the centroid and warm
     tests: 22 mutants, 13 caught, 8 unviable, **1 missed, equivalent**. That miss is warm's
     `rows > 0` as `>= 0`. No present HEAD carries a 0-row ref: a fold drops an empty index,
     an empty compaction commits no output, and a replica copies its source's count.
   - By hand, 4 mutations the tool does not generate:
     - `>=` as `>`: killed.
     - the fresh table always named: **equivalent**. The fresh segment is in an in-process
       store, so its 404 is no request.
     - the fresh table never named, and the dense-leg filter dropped: both survived the
       sweep's tests, and both are killed by the two tests added to `centroid_skip.rs` after
       it.
   - ⚠️ Not `./scripts/mutants.sh` itself: `cargo mutants --in-diff` was run directly, limited
     to those test files.

Spec review took three rounds. Code review took one, and it passed with the two minors above.
The review counter records two earlier packets that no agent saw: one was lost to a full disk,
and one was built on a red tree (the `served_epoch.rs` pin).
