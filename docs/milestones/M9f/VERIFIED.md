# M9f — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).
M9f.1 is delivered here; M9f.2's criteria (the delete) are **NOT-RUN** until that task lands.

1. NOT-RUN — M9f.2.
2. NOT-RUN — M9f.2.
3. NOT-RUN — M9f.2.
4. NOT-RUN — M9f.2.
5. NOT-RUN — M9f.2.
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
