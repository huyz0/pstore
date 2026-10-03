# M41 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `MemoryStore`. Every result is a
test outcome. There is no source change.

Command: `cargo test -p pstore-engine --test schema`.

⚠️ **A test that fails to compile is not counted as red.** This milestone pins the absence of a
defect, so its test is green on the parent by design, and its red is the hand mutation below.

1. **No answer changes across a fold.** `a_fresh_view_never_matches_another_writers_row`: a `body`
   engine's row carrying `revenue` under an ordinary `text` attribute, on an index whose text
   field is empty. A default engine and the `body` engine each query `text` and `body`, before
   and after a fold: all eight answers are `Ok([])` both times.
   - Killed: the fresh view's fallback using the default field. The answers before the fold
     were `[Ok([]), Ok([]), Ok(["r"]), Err("query: no such vector field")]`.
   - `before_any_text_the_fresh_segment_indexes_the_process_field` also kills that mutant.
2. **The agreement holds for text that is indexed.** In `a_fresh_view_never_matches_another_writers_row`, the `body` engine's own
   `body` text is found by its fresh view before the fold. A fold by the default engine then
   records `body`, and the row is still found.
3. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on M41.1, and on this ledger's commit.
   - No source changes, so the sweep has nothing to mutate.

**Out of scope, as the spec states:** a row visible in its own engine's fresh view can be
quarantined at the fold when two writers' fields meet in one fold of an empty-field index. That
is M25's contract for every conflict, the same as for a width or a metric, and no server reaches
the text-field case.
