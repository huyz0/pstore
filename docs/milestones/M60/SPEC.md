# M60 — Compaction keeps every vector

**Serves:** D-28's data model, which a compaction must not lose, and the multivec plan
(M61), whose fields this defect would drop. Found on 2026-10-07 in M61's spec
review, and confirmed by a probe before this spec.

## What is true today

Read from the tree at M59 (`d417772`), and one probe run:

- **`Engine::live_rows`** (`pstore-engine/src/lib.rs`) reads a segment's live documents for
  compaction (`compact_inner`) and for `Engine::scan`, the whole-index export.
  - With no delete vector it calls `Segment::scan`, which returns every field.
  - **With one, it calls `Segment::rows_where`**, which decodes the blocks only.
    `decode_rows` returns every document with **empty `vectors`**, as its comment says.
- **So compaction drops every vector of a segment that has deletes:** dense, named, sparse.
  - The probe wrote three folds of 20 rows, deleted three, folded, then compacted.
  - The same exact dense query then failed: `Query("no such vector field")`.
  - The compacted segment holds the documents' ids and attributes and no vector at all.
- **`Engine::scan` returns the same vectorless documents** for a segment with deletes.

## Delta

**A segment with deletes is read with `Segment::scan` (every field), and its deleted rows are
dropped by position. Compaction and `Engine::scan` then keep every vector of every live
document.**

1. With a delete vector, `live_rows` reads the segment with `scan` and no filter, enumerates
   the rows, drops those in the delete vector, then applies the filter exactly, as now.
2. ⚠️ **No zone-map pruning for a filtered `Engine::scan` over a segment with deletes:** it
   reads every block where `rows_where` read the zones' survivors. Bytes, not requests, and
   only on that path. Compaction reads every block either way.

**Not changed:** a segment without deletes, queries, the format, and HEAD.

## Acceptance criteria

1. **Compaction keeps every vector.** `compaction_keeps_every_vector_of_a_segment_with_deletes`,
   in `crates/pstore-engine/tests/compaction.rs`:
   - three folds of documents with `vector` and a second named dense field;
   - deletes folded into delete vectors;
   - after compaction, the exact dense query over `vector` answers as before, ids and
     distances;
   - `Engine::scan` returns every live document with both fields equal to what was written;
   - before the compaction, while the delete vectors exist, a filtered `Engine::scan`
     returns exactly the live rows the filter admits, with their vectors (code review: the
     filter is then all that selects them).

   Seen red on today's code (the probe's failure).
2. **Gates:**
   - `./scripts/gates.sh` passes;
   - the mutation sweep of the changed lines misses 0.

## Test plan

| AC | Seen red first under |
|---|---|
| 1 | today's `rows_where` branch |

## RA budget

Queries: unchanged. Compaction and `Engine::scan` over a segment with deletes now cost what
`scan` costs over one without (code review):
- one ranged read of the blocks and vectors for a segment of one single-vector field;
- for a segment of several fields, that read, then one more per field, one after another, as
  `Segment::scan` already does today.

Bytes: rule 2.

## Risks

A segment read whole costs more bytes on a filtered export, never on a query: AC1 and the
existing export tests reveal a wrong row; rule 2 is the stated cost.

## Tasks

- **M60.1** `live_rows` through `scan`, and AC1's test.
