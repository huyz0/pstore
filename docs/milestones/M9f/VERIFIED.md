# M9f — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

1. **Gone everywhere** — `a_dropped_index_is_gone_wherever_its_rows_were`
   (`cargo test -p pstore-engine --test drop_index`): folded rows, another process's flushed
   bundle, pending rows here with a fresh view of them cached; a bystander index untouched;
   later folds by either process bring nothing back; the other process stops reporting it --
   seen red with the pending rows kept, and with the generation bump removed. Through the API,
   `delete_removes_an_index_and_refuses_a_missing_one` (`--test lifecycle`), **observed red**
   on the M9f.1 server (`405`). `an_index_only_a_bundle_holds_is_dropped_before_anything_is_sealed`
   -- no schema created or kept, only the HEAD commit written -- seen red with the index left
   in the fold's rows. `an_index_head_names_only_by_its_rejects_can_be_dropped` -- ⚠️ code
   review round 2: a fold can count a reject for an index it names nowhere else -- seen red
   without the reject-count term. The `schemas` term never decides anything and is not in the
   rule (the spec is amended).
2. **Recreated afresh** — `rows_written_after_a_drop_create_the_index_again_with_a_new_schema`:
   new width and metric; a write-only engine with **both** door rungs stale (the old schema
   read, an old-width row flushed) accepts the new width -- seen red with the refusal's re-read
   not remembering the schemas, and with it not pruning.
   `the_write_door_uses_the_schema_the_process_has_read` (M7d) pinned a refusal at zero
   requests; since this task a refusal costs exactly one HEAD read, as the spec states, and the
   test now pins that and an accepted write at zero.
3. **`as_of` and GC** — `as_of_before_a_drop_answers_with_the_old_metric`: cosine `$dist` for an
   epoch before the drop, after the name was made `euclidean_squared`, dropped again and made
   `dot_product`, with another index dropped in between -- seen red with the later drop's
   schema chosen. `gc_directly_after_a_drop_reaps_it_and_its_dropped_schema` (segments and
   delete vectors absent from the store, `dropped` empty);
   `a_dropped_schema_round_trips_and_is_reaped_at_its_epoch` (`--test head`: a horizon of 6
   keeps and 7 reaps a drop at 7).
4. **Missing, and two deletes** — `a_missing_index_is_not_deleted_and_two_deletes_answer_once`:
   `None` and HEAD's epoch unchanged; two engines, the second's delete landing between the
   first's reads and its commit (asserted to have run), the first answering `None` on its
   retry; an index held only by pending rows exists.
5. **A flush in flight** — `a_flush_in_flight_during_a_drop_loses_no_later_write`: the bundle
   PUT held, the drop not finished while it is, a later write served and surviving a fold, the
   held row absent -- seen red with the flush lock not taken.
   ⚠️ **Rebuilt once:** the container restarted before M9f.2 was committed and restored an
   older snapshot, so the task was rebuilt from its record; every "seen red" above was
   re-observed on the rebuilt code.
6. **Another process's folded rows** — `rows_another_process_folded_are_no_longer_reported_unfolded`
   (`cargo test -p pstore-server --test lifecycle`): a writer flushes, a second process folds,
   and the writer's next `GET` -- its only read of HEAD since -- reports `unfolded: false`.
   **Observed red** on the M9e server, as were the other two tests in the file. The list's prune:
   `a_list_forgets_what_another_process_folded_away` -- ⚠️ added at code review, which found no
   test noticed the list's prune removed; now seen red with it removed. (The delete half of
   this criterion is M9f.2's.)
7. **The list, a page at a time** — `indexes_list_a_page_at_a_time`: 250 names, 240 folded and 10
   only in memory, three pages of 100 with every name once and in order, `next_cursor` `null`
   on the last; a prefix with its own cursor; `page_size` 1000 accepted (code review: the upper
   edge was untested) and 0, 1001 and `x` refused; at most one read and zero LISTs a page.
8. **`updated_epoch`** — `an_index_reports_when_its_contents_last_changed` (another index's
   fold does not move it, a folded delete does, `null` with only unfolded rows) and
   `a_compaction_moves_the_updated_epoch_and_another_indexs_fold_does_not`
   (`cargo test -p pstore-engine --test updated_epoch`; no route compacts).
9. **Gates** (M9f.1) — `./scripts/mutants.sh --check . --in-diff <the M9f.1 diff>`: **23
   tested in 13m, 12 caught, 9 unviable, 1 missed, 1 timeout**, both in `list_indexes`: the
   page-full test as `>=` (a cursor to an empty page) is now pinned by an exact final page in
   `indexes_list_a_page_at_a_time`, and the timeout -- the cursor ignored, the first page
   returned forever -- is a failure since the test's page loop is bounded. Re-swept
   `--check list_indexes`: **17 tested, 13 caught, 4 unviable, 0 missed**. Code review: two
   rounds (block on the list's untested prune, then pass). `./scripts/gates.sh` on the committed
   tree: all fifteen PASS.
   **Gates** (M9f.2) — `./scripts/mutants.sh --check . --in-diff <the M9f.2 diff>` on the
   rebuilt code: **41 tested in 18m, 32 caught, 8 unviable, 1 missed** -- `metric_at`'s `>` as
   `>=`, **equivalent**: `as_of` the drop's own epoch finds no segment of the index, so its
   schema is never observed (a comment at the line says so). The first sweep, before code
   review's test changes, had missed five more, each since pinned by a test above (the
   interference hook's answer, the existence terms, the generation bump, `metric_at`'s name
   test). Code review: two rounds. Round 2 blocked on a regression the sweep's cleanup had
   introduced (the reject-count term deleted as redundant), which was reverted to the reviewed
   rule and tested; not reviewed a third time. `./scripts/gates.sh` on the committed tree: all fifteen PASS.
