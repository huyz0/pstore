# M19 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

The tests are in `cargo test -p pstore-engine --test abandon`. **Observed red:** the four burial
tests failed on the tree before M19, with each orphan named as not buried. The two same-lane tests
and the no-burial test passed there, since nothing was buried at all. They are pinned by the hand
mutations below. Ten were run, each by replacing the line and running these tests, and each
fails a test: two only after tests were added for them (in 5 and 7).

1. **A discarded compaction** — `a_discarded_compaction_is_buried`: every orphan buried at its own
   key epoch, the past unchanged, `gc(0)` taking it and its sidecars, and the live rows kept. It
   failed with the first seal or a re-seal not recorded, and with keys buried at the committing
   epoch.
2. **A moved delete vector** — `a_compaction_whose_delete_vector_moved_is_buried`, which failed
   the same ways.
3. **A refused branch retry** — `a_refused_branch_retry_is_buried`: the copy buried at its own
   epoch, reaped, and `dest`'s rows kept. It failed with the branch never burying, and with the
   copy not recorded.
4. **The past** — `a_discarded_compaction_is_buried`, `a_refused_branch_retry_is_buried` and
   their siblings compare `history` at every epoch up to the burial, which includes each buried
   key's own epoch. Burying at the committing epoch fails all four burial tests.
5. **Same-lane winners.**
   - `a_same_lane_winners_key_is_not_buried` failed with the `indexes` half of the filter
     removed.
   - `a_same_lane_winners_copies_are_not_buried` failed with the `deletes` half removed.
   - `a_key_already_buried_is_not_buried_twice` failed with the already-buried check removed.
6. **An error exit** — `an_error_after_sealing_buries_the_seal`. It failed with the burial made
   only on `Ok(None)`.
7. **Cost.**
   - `an_abandonment_costs_one_read_and_one_commit` came from code review round 1. It counts 2
     reads and 2 CASes after the winner, and 2 and 3 when the burial's first CAS is contended.
     It failed at 3 reads, which is how the review's finding showed.
   - `nothing_written_and_success_commit_no_burial`: a merge that sealed nothing adds no read,
     and a success reads HEAD once. It failed with the burial also run on success.
   - Code review also found that GC never reaped a segment's centroid table. That fix is its own
     commit (`e63f00d`), with `gc_reaps_a_segments_centroid_table_with_it`
     (`cargo test -p pstore-engine --test dense_index`), seen failing first.
8. **Gates** — NOT-RUN yet: swept with M17's remainder and M18, then `./scripts/gates.sh`.

Spec review took two rounds. Code review took two: round 1 blocked on two majors (a contended
burial re-read HEAD; GC leaked centroid tables); round 2 passed.
