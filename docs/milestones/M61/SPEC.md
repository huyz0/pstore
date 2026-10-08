# M61 — Multi-vector documents, searched by MaxSim

**Serves:** D-28 ("support multi-vector as a document-level concept from the schema up … even
if the MaxSim scorer lands later"), and the project owner's request of 2026-10-07 to plan and
deliver multivec support. OQ-65 and OQ-128 (the storage layout) were answered by M3b: a
section per field, with a row-offset table. This milestone uses that layout.

## The plan

Multi-vector (late-interaction, ColBERT-style) search, in four pieces:

| Piece | What it adds | Where |
|---|---|---|
| 0. Compaction keeps vectors | A segment with deletes lost every vector at compaction | [M60](../M60/VERIFIED.md), done |
| **1. Exact MaxSim, end to end** | Named multi-vector fields through the API; a `multi` leg scoring every row by MaxSim, fused by RRF | **M61, this spec** |
| 2. Candidates from quantized codes | Per-vector 1-bit codes, so a query reads codes, not every float, and scores only candidates exactly | backlog row 63 |
| 3. Split across servers | A `multi` leg in M54's parts | backlog row 64 |
| 4. Dense search over a named field | Per-field clustering; today a dense leg naming any field searches `vector`'s codes | backlog row 65 |

## What is true today

Read from the tree at M60, nothing measured:

- **The format stores multi-vector fields** (M3b): `VectorField::Dense(Vec<Vec<f32>>)`, and a
  variable-width section with a `rows + 1` offset table. `Segment::field_vectors` reads a
  field whole, in one ranged read; its span is known from the meta region the open reads.
- **The engine stores every field of every document,** and clusters only `vector`.
- **The API cannot write one,** and no query scores more than one vector per row.
- ⚠️ **Three format hazards** (spec review):
  - **Field order.** The writer numbers dense fields in name order, and field 0 takes the
    legacy fixed-width `Vectors` section. `late` < `vector`, so a `late` field would become
    field 0, and every dense query would decode its offset table as vectors.
  - **Shared ids.** Every field after field 0 takes section id `FieldVectors`. The reader
    keeps one span per id, so in a segment of three dense fields, one field reads another's
    vectors.
  - **Silent padding.** The writer pads or truncates every vector of a field to the first
    row's width.
- ⚠️ **A dense leg naming a field other than `vector` searches `vector`'s codes**
  (`VecIndex::search_field`). It is unreachable through the API today, and reachable once
  named fields are writable.

## Delta

**Named multi-vector fields can be written, and searched by an exact MaxSim `multi` leg that
fuses with dense and text legs by RRF.**

1. **Format.**
   - `vector` is always field 0.
   - Fields 0 and 1 keep their section ids. Field `k ≥ 2` takes id `FIELD_SECTIONS + k`
     (`0x1000 + k`), through the raw directory, and is found through the `Fields` table as
     every field is.
   - A segment of one or two dense fields is written byte for byte as before.
2. **Write.** `DocumentIn` gains `vectors: {name: [vector, …]}`. Each vector is a JSON array
   or base64, as `vector` is.
   - **Refused `400`:**
     - a name that is empty, over 64 bytes, or `vector`;
     - no vectors, or more than 1,024 in one field;
     - an empty, non-finite or (under cosine) zero vector;
     - more than 8 named fields in one document.
   - **Refused `schema_conflict`:** one field's vectors of differing length within the
     batch, or against the rows this process holds. This is checked whether or not the index
     has a schema; the schema records `vector`'s width only.
   - Each field goes through the metric transform, as `vector` does.
   - **In a fold** (implementation review): a row whose named field disagrees in width with
     the fold's first row of that field is quarantined, as a row contradicting the schema
     is (M35). Two processes' rows first meet there, past both doors. **And again over the
     rows a patch resolves to** (code review): a patch merges into its folded base row after
     that pass, so a resolved row that disagrees with the fold's first of its field, in the
     order operations arrived, is quarantined and its id left untouched. Its version
     already folded stands: the operation that came second is what it costs, a patch or a
     write, never the fold.
   - **The writer refuses a field of mixed widths** (`try_finish`), where it padded. No path
     seals one silently. A compaction over two segments holding one field at different
     widths therefore fails, loudly, and leaves them uncompacted until piece 4's per-field
     schema.
3. **Query.** `QueryRequest` gains `multi: {"field": name, "vectors": [vector, …]}`, one leg,
   `Prefetch::Multi { field, query, limit: top_k }`. Its vectors get the dense query's
   transform.
   - Refused `400`, as the write refuses them: bad vectors, a missing or `vector` field, and
     more than 1,024 vectors.
   - Refused `400`: `rank_by` or `aggregate_by` beside it.
   - Refused `400`: more than `MAX_LEGS` legs in all.
   - RRF weights run dense first, then `multi`, then the text legs.
4. **Score, exact.** `MaxSim(q, d) = Σᵢ maxⱼ ⟨qᵢ, dⱼ⟩` over the transformed vectors:
   - order: i in query order, j in document order;
   - each dot is a sum over dimensions in order, `max` folds from −∞, and the outer sum runs
     in i order.

   Under euclidean, each `⟨q′ᵢ, d′ⱼ⟩` is `(‖qᵢ‖² − ‖qᵢ − dⱼ‖²)/2`, so the ranking is the
   Chamfer distance `Σᵢ minⱼ ‖qᵢ − dⱼ‖²`, ascending.
   - A row with no vectors in the field is no candidate.
   - The leg is cut, widened or made exhaustive like any leg. Exhaustive means the
     segment's index row count, which is never fewer than its data rows.
   - **One blob round:** the leg round, with one ranged read of the field per segment.
     Depth stays D-34's.
   - ⚠️ **Bytes are every vector of the field.** Piece 2 is the fix.
   - The response has no `$dist` for it.
5. **Errors.**
   - `UnknownField` when no searched segment carries the field. This is decided in `run`
     from the opened footers, at no request. A segment without the field answers nothing.
     The engine types it (`EngineError::UnknownField`), and the API answers `400
     unknown_field` (implementation: as a string it was `500 internal`, retryable).
   - `DimensionMismatch` on a segment whose width is not the query's: `400 schema_conflict`,
     as a dense query's is. That is loud where piece 4's schema would refuse the write.
   - A dense leg over a field of several vectors a row is refused (`Unimplemented`). Only the
     engine API can ask for one: rule 6.
6. **The API's dense leg covers `vector` only.** Another `field` is refused `400`, naming
   `multi` (piece 4).
7. **Fusion.** RRF and weighted RRF fuse a `multi` leg. `sum` and `max` refuse it with `400`,
   as they refuse a dense leg.
8. **Not split.** `shares` returns no share when any leg is `multi`, with or without a text
   leg (piece 3).

**Not changed:** HEAD, the dense, sparse and text legs, and every answer to a query without
`multi`.

**Not covered, and the ledger says so:**
- pieces 2–4;
- `include_vectors`;
- a schema-wide width per field: two segments of one field at different widths are a loud
  `schema_conflict` on the query, and their compaction fails, never silent padding within a
  segment.

## Acceptance criteria

1. **MaxSim is MaxSim.** `multi_scores_by_maxsim`, in `crates/pstore-query/tests/multi.rs`:
   hand-written query and document vectors, one document with none. The leg's ranking and
   raw scores equal the test's own MaxSim, summed in rule 4's order, bit for bit, and its
   limit cuts it.
2. **The format holds every field.**
   `every_field_reads_its_own_vectors_and_vector_is_field_zero`, in
   `crates/pstore-format/tests/fields.rs`: four dense fields, `late` among them, each read
   back whole, `vector` first, every span distinct. `the_default_field_is_field_zero` in
   `multi.rs`: a dense leg over a segment holding `late` answers as without it.
   `a_field_of_mixed_widths_is_refused`, in `fields.rs`: `try_finish` refuses a field whose
   vectors differ in width, in one row or across two.
   `a_segment_of_one_or_two_fields_is_written_as_before`, in `fields.rs`: the bytes of a
   segment of `vector` alone, and of `vector` with a field named after it, fingerprint as
   the M60 writer's.
3. **Errors in the query layer.** `multi_errors_are_loud`, in `multi.rs`:
   - an unknown field is `UnknownField`;
   - a short query vector is `DimensionMismatch`;
   - a dense leg over `late` is refused.

   `a_multi_leg_is_widened_past_deleted_rows`: with its best rows deleted, a leg still answers
   its limit in live rows.
4. **Through the API.** `multi_vector_documents_are_searched_by_maxsim`, in
   `crates/pstore-server/tests/multi.rs`:
   - Documents carry `vector` and `late` with 0 to 5 vectors each. Query vectors have
     unequal norms, and the model's scores are distinct.
   - Under cosine and under euclidean, a `multi` query's id order equals the test's own
     ranking (cosine MaxSim, and Chamfer ascending).
   - This holds unfolded, folded, after deletes and an upsert that changes `late`, and after
     a compaction.
   - With a filter, and fused with a dense leg and a text leg, the ids equal the test's own
     filter and RRF.
5. **Refusals.** `multi_vectors_are_refused_at_the_door`: every `400` of rules 2, 3, 5, 6 and
   7. `a_named_fields_width_is_one_an_index`: `schema_conflict` for a width that changes
   across two batches, with a schema and without one, and within a batch or a document; and,
   once every row is folded, a new width accepted and the query meeting both refused. `two_writers_widths_meet_in_the_fold`, in
   `crates/pstore-engine/tests/multi.rs`: two processes write `late` at two widths, each past
   its own door; the fold quarantines the second lane's row and the first's is searched.
   `a_patch_beside_a_new_width_never_stops_the_fold`, in the same file: a patch of a row
   folded at one width, beside a new row at another, is quarantined; the folded version
   stands, and the tenant still folds.
6. **Depth.** `a_multi_query_keeps_the_depth`: a cold `multi` query, and a hybrid one with
   it, keep a dense one's depth of four, at one segment and at four. The field is wider than
   the open round's suffix read, so its read is a request of its own.
7. **Not split.** `a_multi_query_is_not_split`, in `crates/pstore-server/tests/peers.rs`:
   on M54's fixture, `multi` alone, with a text leg and with a dense leg send no part and
   equal the unpeered server.
8. **Gates:** `./scripts/gates.sh` passes; the mutation sweep misses 0, or each miss is killed
   by a test named in the ledger.

## Test plan

| AC | Seen red first under |
|---|---|
| 1 | `Prefetch::Multi` absent (compile), then max as sum |
| 2 | the writer's name order and shared ids restored |
| 3 | the field check aggregated away |
| 4 | the query vectors untransformed |
| 5 | each refusal removed; the fold's width partition removed |
| 6 | the field read in a round of its own |
| 7 | `shares` not refusing `multi` |

## RA budget

Depth unchanged. One ranged read a segment beside the other legs' (Rpar). Bytes: the field's
whole section a segment.

## Risks

- **Bytes:** 100 vectors a document at 128 dimensions is 51 KB a document per query.
  `docs/deploy.md` says so; piece 2 fixes it.
- **CPU:** `m × n × d` per row, spread by M59.
- **`Segment::scan` reads each field in a round of its own,** so once a segment has `late`, a
  fold's compaction or an export costs a round more. Queries never use it.

## Tasks

- **M61.1** The format's field order, ids and width refusal; `Prefetch::Multi` and MaxSim:
  AC1–AC3.
- **M61.2** The engine's transform, width check and `shares`; the API; docs; backlog rows
  63–65: AC4–AC8.
