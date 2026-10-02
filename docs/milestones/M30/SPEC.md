# M30 — The schema's text field decides where a segment's text is indexed

**Serves:** [BACKLOG](../BACKLOG.md) row 29, which [M7d](../M7d/VERIFIED.md) opened: `compact`
derives its text field from its inputs and never asks HEAD what the index is. Measured while
planning this milestone, the gap is not only unpinned: it can block an index's compaction for
good.

## What is true today

- An index's schema is created by its first fold and is never changed afterwards (M7d).
  - Its `text_field` is the folding process's field when that fold's rows carry text in it.
    Otherwise it is **empty**, meaning that fold saw no text.
- The door, the flush and the fold refuse a row only when the schema's text field is
  **non-empty**, differs from the process's, and the row carries text in the process's field.
- A fold seals every index over the **folding process's** text field (`self.text_field`), not
  the schema's.
- A compaction seals over the one field its inputs name. It refuses inputs that name two, with
  `Format("cannot merge segments whose text indexes name different attributes ...")`.
- **Measured** in a probe while planning (`cargo test`, not committed):
  1. A vector-only fold records `text_field: ""`.
  2. A process configured `body` folds a row with `body` text. The schema stays `""`.
  3. A process configured `title` folds a row with `title` text. It is accepted, and the
     schema stays `""`.
  4. Every compaction of the index then fails with that `Format` error, so its segment count
     only grows.
- **Who can reach it:** `pstore-server` always uses the default text field, so a server fleet
  cannot. An embedder calling `Engine::with_text_field` can, and the engine's contract (M7d)
  says the schema prevents exactly this.

## Delta

**The schema's text field is filled once, and every seal uses it.**
1. **Filled by the first fold with text.** When a fold finds an index whose schema's text field
   is empty, and the **resolved rows it seals** carry text in the folding process's field, the
   commit records that field.
   - This is the predicate `seal`'s `wants_text` uses, not `implied`'s pre-resolution one
     (spec review B2). So a patch (M13) that adds text to a vector-only row fills it too.
   - An empty field means "no text yet", not a choice. Filling it is not a change, and every
     existing guard then applies.
2. **A fold seals over the schema's field** when one is recorded, else over its own, as today.
   A process configured otherwise, folding rows of an index that has one, still writes that
   index's postings.
3. **The fresh segment builds over the schema's field** when the process's cached schema
   records one (spec review B3), as its `fts` and `trigram` already do. So a query's answer
   does not change at the fold.
4. **A compaction seals over the schema's field** when one is recorded. It refuses, with the
   same `Format` error, any input whose text index names another. With none recorded, it
   derives the field from its inputs, as today.

**Not changed:** the door and flush checks; the reject pass (M25); the format; request
counts; an index whose schema already records a field; branches and replicas, which copy the
schema whole; and the server, which uses one field.
A compaction under a `""` schema whose inputs name a field (an index from before M30) merges
over that field and fills nothing: only a fold fills (spec review round 2).

## Acceptance criteria

1. **The probe's sequence ends compactable.** After step 2 the schema records `body`. The
   `title` process reads HEAD, then step 3's write is refused at the door with
   `SchemaConflict`, and a compaction commits. Parent: schema `""`, accepted, compaction fails.
2. **A patch that adds text fills.** A vector-only index (schema `""`) has a row patched with
   `body` text and folded by a `body` process: the schema records `body`. Parent: `""`.
3. **A fold over another field indexes the schema's.** In an index recording `body`, a `title`
   process folds a row with `body` text, and a `body` query finds it. Parent: it does not.
4. **The fresh segment agrees.** In 3's index, before the fold, the `title` process's query
   for that unfolded row finds it, as after. Parent: it does not.
5. **A compaction rebuilds over the schema's field.** The inputs name no text field, the schema
   records `body` (set with `commit_head_for_test`), and the rows carry `body` text. After the
   compaction, a `body` query finds them. Parent: the merge is over the default, so it does not.
6. **A compaction refuses an input of another field.** Inputs naming only `title`, in an index
   recording `body`: refused with the `Format` error, HEAD unchanged. Parent: it commits.
7. **Vector-only folds fill nothing.** A `title` process folds vector-only rows into a `""`
   index: the schema stays `""`.
8. **Gates.** `./scripts/gates.sh` is green. The sweep over M30's source diff misses 0, or
   names each miss as equivalent. Test 7's red is shown by "fill on any fold", recorded.

## Test plan

New tests in `crates/pstore-engine/tests/schema.rs`.

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1 | `an_empty_text_field_is_filled_by_the_first_fold_with_text` | schema stays `""`; fill dropped |
| 2 | `a_patch_that_adds_text_fills_the_text_field` | `""`; fill decided before resolution |
| 3 | `a_fold_indexes_the_schemas_text_field_not_its_own` | `title` postings; seal over `self.text_field` |
| 4 | `the_fresh_segment_indexes_the_schemas_text_field` | fresh build over `self.text_field` |
| 5 | `a_compaction_rebuilds_over_the_schemas_text_field` | merged over the default field |
| 6 | `a_compaction_refuses_an_input_of_another_field` | the merge commits; the refusal dropped |
| 7 | `a_vector_only_fold_fills_no_text_field` | a guard: fill on any fold |

Tests 5 and 6 build their HEAD with `commit_head_for_test`, since M30 makes it unreachable.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Fold, compaction | unchanged | unchanged | unchanged | 0 | unchanged |

The fill is a field in a HEAD the fold already commits.

## Risks

- **Indexes already holding segments of two fields stay uncompactable.** M30 stops new ones.
  An existing one needs a rebuild, which this milestone does not add. Named in the ledger,
  and recorded as a BACKLOG row if any test fixture or deployment is known to hold one.
- **A fold that fills, racing a fold by a process with another field.** Both rebase on CAS, so
  one fills and the other, re-reading HEAD, finds the field recorded. Its rows carrying its own
  field's text are then rejected at its reject pass and quarantined (M25), which is what a
  non-empty schema already does.

- **The fill is observable.** `GET /v1/indexes/{id}/schema` reports a text field where it
  reported none (spec review m6). M7d's "immutable afterwards" holds for every recorded field.
  `""` was never a recording, and `head.rs`'s doc comment on `text_field` says so after M30.

## Tasks

- **M30.1** — Fill, the fold's seal and the compaction's seal and refusal, and the fresh segment, with tests 1–7.
- **M30.2** — The ledger, `BACKLOG.md` row 29 closed, and the roadmap row.
