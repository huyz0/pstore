# M7d — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a test written before the code and run against its absence, or a
mutation applied to the shipped code and the failure read.

1. **The first fold records what it inferred** — `a_fold_records_the_schema_it_inferred`
   (`cargo test -p pstore-engine --test schema`), read back by **decoding the committed
   object** rather than asking the engine that wrote it: `dims: 4` from the rows, `text_field:
   "body"` from the rows that carry text.
   ⚠️ `a_vector_only_index_records_no_text_field` is the other half and it came from spec
   review: an index of pure vectors records **no** text field, so a process configured over a
   different attribute may still fold into it. Recording the knob would have been a conflict
   over a field neither segment has postings for.
2. **A contradicting index seals nothing** — `a_contradicting_index_seals_nothing`, asserted on
   `OpClass::Write` per fold and **not** on the epoch, which is local until the commit and
   therefore cannot show the difference. Spec review caught that vacuity in the first draft.
3. ⚠️ **A contradiction does not stop the tenant** — `a_wrong_width_row_does_not_stop_the_tenant`.
   One wrong-width row durable in a lane bundle: the fold **succeeds**, the watermark advances,
   the other index in the same bundle folds and is queryable, `schema_rejects` counts the
   dropped row, and a later correct write folds normally. **This test is the milestone.** The
   first draft of the spec refused the fold, and spec review measured the consequence: a fold
   is all-or-nothing across every index in its bundle set and re-reads the same bundles on
   every attempt, so one accepted API call would have stopped every later fold for that
   tenant, forever.
4. **The flush refuses before the bundle exists** — `a_flush_refuses_before_the_bundle_is_written`
   (zero write-class requests at the refusal), and `the_schema_is_read_once_per_process`: the
   first flush reads, the second reads **0**. ⚠️ Observed red at 2 requests per batch, because
   the cache conflated "empty" with "never read" — a tenant with no schemas yet reads back an
   empty map, so every flush paid the read until some fold recorded one. `Option`, not
   `is_empty()`: "I have not looked" and "I looked and there was nothing" are different facts.
5. **The door uses what the process has read** — `the_write_door_uses_the_schema_the_process_has_read`
   at **zero requests**, warmed by `indexes()` — any HEAD read will do — and M7c's
   memtable-local check still refuses a self-inconsistent batch in a process that has never
   read HEAD.
6. **BACKLOG row 27 is narrowed, not closed** — the case it names, a wrong width *after a
   fold*, is now `400 schema_conflict` at the door, asserted in
   `a_dimension_mismatch_is_a_client_error_on_both_query_paths`, which had to be **rewritten
   in the good direction**: M7c asserted the write was accepted. The residue is a write-only
   process that has never read HEAD; it is refused at its flush instead, so nothing wrong
   becomes durable, and the row stays open with that scope.
7. **The text field is compared only when the rows carry it** —
   `a_text_field_that_contradicts_the_schema_is_refused` for the conflict, and
   `a_vector_only_index_records_no_text_field` for the false-positive half.
8. **A HEAD from the old encoder decodes** — `a_head_without_a_schema_section_decodes_as_no_schemas`
   (`cargo test -p pstore-engine --test head`), asserted against **bytes built without the
   section** by a local encoder in the test, so it cannot pass by round-tripping the new one.
   ⚠️ And the price is asserted rather than tolerated:
   `a_truncated_head_is_refused_at_every_length_but_a_section_boundary` enumerates **every**
   truncation and requires exactly **two** to decode — the two optional-section boundaries. A
   third means a section stopped being checked.
9. **The API reports it and refuses a change** — `the_api_reports_the_schema_and_refuses_a_width_change`:
   `schema.dims`, `schema.text_field`, `rejected_rows`, and `400 schema_conflict` whose message
   names which fact was contradicted. ⚠️ Observed red as **`500 internal`**: the error-table arm
   was missing, which is exactly the defect M7c added a typed `DimensionMismatch` to fix,
   repeated one milestone later — which is why the spec made the arm its own criterion.
10. **`PATCH` refuses with the path** — `patching_a_schema_is_refused_with_the_migration_path`:
    `400 schema_immutable`, not a `404`, because "there is no such route" and "that operation is
    not permitted, here is what to do instead" are different answers and only one is true. The
    tenant header is still required, so nothing is learnable by omitting it.
11. **The existing suite is green**, and three fixtures had to change — recorded because each
    change is a behaviour change, not a test repair:
    - `the_whole_flow_stays_inside_its_request_budget`: a lane's **first** flush is now 4
      requests, not 3, the extra one being the schema read. Once per process; the steady-state
      number is unchanged at **1 PUT per batch**, which is what the assertion protects.
    - `a_write_the_format_cannot_store_is_refused_at_the_door`: its fixture mixed widths
      incidentally, which this milestone forbids.
    - `a_compaction_of_disagreeing_inputs_is_refused`: its state **can no longer be produced by
      writing at all**, so the fixture now builds what a *pre-M7d store already contains* —
      two segments under one index name over different attributes, assembled through HEAD. The
      guard still matters, because `compact` is explicitly out of M7d's scope.
12. **Coverage and mutation** — `./scripts/coverage.sh --fail-under-regions 95` passes at
    **95.05%** regions, 96.79% functions, 96.92% lines. Mutation, two sweeps:
    `scripts/mutants.sh --file crates/pstore-engine/src/head.rs crates/pstore-server/src/lib.rs`
    — **39 of 39 viable caught**, 45 unviable, no survivors; and
    `scripts/mutants.sh --check "implied|against|remember_schemas|cached_schema|schemas_unseen|flush_inner"`
    over the engine's new schema functions — **33 caught**, 10 missed, **none of them in code
    this milestone wrote**. Seven are pre-existing struct-field deletions in `pstore-testkit`
    and `pstore-node`'s wiring. ⚠️ The other three are the memtable generation bump inside the
    flush, visible to this regex only because that function was renamed; they are **equivalent by the
    memtable's own documented reasoning** — its comment already says a flush "moves rows from
    `pending` to `durable` without changing what they are, so it invalidates needlessly", and a
    cached fresh segment rebuilt from either side of that move has identical content. Named
    rather than counted as caught.

## Code review found two, and both were measured rather than argued

⚠️ **The check read only the first row of a batch.** `implied` took the width from
`docs.first()` and both the door and the fold used it, so a wrong-width row anywhere else went
straight into the segment — and a fold merges every lane's rows for an index into one batch
whose order is *lane* order, so "first" is not even the caller's first. The result is a
**mixed-width segment**, which nothing downstream can refuse: a segment declares one width and
scores every row it holds against it, which is precisely the silent wrongness this milestone
claims to have turned into a refusal. Recording a schema still uses the first row — that is
what an index's creation *means* — and validating now looks at every row.
`a_wrong_row_anywhere_in_the_batch_is_caught`, observed red against the shipped `first()`.

⚠️ **The fold dropped the whole index's rows, not the contradicting ones.** One wrong row from
a cold process discarded every correct row any other writer had flushed for that index in the
span — rows acknowledged by writers that passed both the door and the flush — and the watermark
advances past their bundles, so a later fold never sees them and GC reaps them. The spec's
claim that the loss is bounded to "the racing writer's rows" was false as implemented.
`only_the_contradicting_rows_are_dropped` pins one dropped row against two kept, observed red
against the whole-index clear. ⚠️ `an_all_dropped_fold_still_commits_what_it_learned` closes
the case the spec singled out and the first round of tests did not exercise: every row dropped,
and the fold still commits the advanced watermark and the count.

⚠️ **And one the reviewer noticed beside them, closed rather than filed**: a brand-new index
created by a *single* mixed-width batch had nothing to compare against — no schema yet, no rows
yet — so it was accepted, the schema recorded the first row's width, and the rest read back at a
width they never had. It predates M7d, it is one line, and it is in the function this milestone
was already rewriting. `a_brand_new_index_cannot_be_created_mixed_width`.

## What this milestone found on its way

⚠️ **`lanes::register` retried 16 times with no backoff**, and under a loaded full-suite run at
100 writers registering at once it spent all 16 and returned `Lost` to a caller that had done
nothing wrong — `every_acknowledged_document_survives_contention`, which passes in isolation and
failed under load. The budget is **unchanged at 16**; what was added is the delay between
attempts that the commit path has always had. A bound stops an unbounded retry; it does not stop
the retries from being simultaneous, and a contended registry that synchronises its losers is a
registry that fails them together.

## Stated non-properties

- **The cached schema can be stale.** Another process may have folded since. The flush re-reads
  once per process and the fold is the final authority; the cache is an early refusal.
- **Rows acknowledged `durable` can be discarded at a fold.** Bounded to a race between two
  processes that have both never read HEAD, counted in HEAD, reported by the API, and
  recoverable by hand for as long as the bundle survives GC. The alternative was measured and
  it wedges the tenant.
- **`compact` does not consult the schema.** Correct today, named so a later change has to face
  the line.
- **One dense field per index.** `dims` is a single number because the segment layout stores one
  dense vector per row; a second field needs a schema per field, which is a rename away.
