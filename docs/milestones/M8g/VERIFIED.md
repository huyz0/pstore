# M8g — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

All runs are in the Linux dev container. Every "killed" was a mutant applied by hand, one at a
time, restored afterwards, and confirmed to fail the named test's assertion, not the build
(`cargo test -p pstore-engine`).

1. **`Split` routes every read by prefix, and deletes and lists durably** —
   `the_split_store_routes_reads_by_prefix_and_deletes_and_lists_durably`. **Mutations verified
   killed**, all five: `get`, `get_suffix` → `Ok(Default::default())`, `get_tag` → `Ok(None)`,
   `delete_batch` → `Ok(())`, `list_unrestricted` → `Ok(vec![])`. (`get_tag` had been caught in
   the clean sweep only by `pstore-testkit`'s `sweep_reports_a_curve_not_a_point`, indirectly.)
2. **`backoff` waits** — `backoff_waits_the_delay_it_computes`, under a paused clock, asserts
   `[delay, delay + 1ms)`. **Mutation verified killed**: `backoff` emptied.
3. **The fresh cache moves with writes, and is per index** — `a_query_after_a_second_write_sees_it`
   and `a_query_on_one_index_is_not_served_another_indexs_fresh_rows`. **Mutations verified
   killed**: `generation += 1` → `*= 1` (now in `Memtable::buffer`), and `f.index == index` →
   `!=` in `fresh_target`.
4. **One buffering step** — the real write and its test-only twin both call one private
   buffering step; `grep -c 'generation += 1' crates/pstore-engine/src/lib.rs` went from
   **6 to 5**, and `a_query_after_a_second_write_sees_it` exercises the shared bump.
5. **A sparse leg reaches unfolded rows** — `a_sparse_leg_reaches_unfolded_rows`. **Mutation
   verified killed**: `delete !` in `fresh_target`'s dictionary filter.
6. **A conforming fold records no rejects** — `a_conforming_fold_records_no_rejects`.
   **Mutation verified killed**: `dropped > 0` → `>=`.
7. **An idle lane gets no watermark** — `a_lane_with_nothing_to_fold_gets_no_watermark`.
   **Mutation verified killed**: `tail > 0` → `>=`, where the code's comment had called that
   mutant provably equivalent; the comment now says it is pinned.
8. **The stale-commit helper** — it now commits a default head, and its three callers still see
   refusal: `a_commit_reached_past_the_doors_is_refused`, `a_stale_committer_is_fenced` and
   `a_writer_paused_past_many_commits_cannot_corrupt` (`cargo test -p pstore-engine`). ⚠️ The
   first draft of this line named a test that does not exist; `check-verified.py` refused it.
9. **Only the deferred five remain** — `./scripts/mutants.sh --check "^crates/pstore-engine/src/"`:
   318 tested in 44m. The `pstore-engine` misses are exactly the five deferred to BACKLOG row
   35 — the flush path's two generation bumps (`*=`; `-=` and `*=`) and the restore step's (`-=` and `*=`) —
   and nothing else. The regex also selects `pstore-testkit` mutants through their
   descriptions; 5 of those were missed and are the queued `pstore-testkit` work.
10. **The deferred defects are recorded** — [`BACKLOG.md`](../BACKLOG.md) rows 35, 36 and 37
    under "Opened by M8g" (`grep -n 'Opened by M8g' docs/milestones/BACKLOG.md`): rows vanish
    while their flush is in flight, a fold can hide rows a concurrent flush made durable, and
    `query` re-locks the fresh segment without re-checking its index. Code review added row 38:
    rows another process folds are served twice by this one until it folds itself.
11. **The full gate** — `./scripts/gates.sh` in the dev container on this tree: all fifteen gates
    PASS. (Its first run was red: `check-verified.py` refused a function name written as a test.)
