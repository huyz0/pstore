# M178: public sparse vectors

## Serves
D-72, OQ-126 (revisited) and workstream D of [the plan](../../comparisons/turbopuffer-next-plan.md):
"public sparse search", the `Sparse vectors` and `SparseKNN` rows of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md). M177 is the engine half.

## Delta
Today the engine holds string-keyed sparse fields (M177) and no request can write or query
one. After, HTTP does both, over one field a document names `sparse`.
- **Write**: a document's `sparse` is an object of key to weight, `{"cat": 0.7}`: 1 to 1,024
  keys of 1 to 256 bytes, each weight a JSON number with `|w| ≤ 65,504` (f16's largest;
  zero and negatives included). Anything else, `sparse` not an object included, is 400
  naming the document and the bound; nothing is buffered. A key spelled twice in one
  object is its last weight, as every JSON object here is read.
- **The name is reserved**: `named_field` refuses `sparse`, so `vectors` and a `field` or
  `multi` query cannot reach it. `upsert_columns` reads a `sparse` column as this field (a
  cell an object or `null`), as it reads `vector`; `patch_columns` and a patch refuse it.
  An attribute named `sparse` stays an attribute, as one named `vector` does.
- A patch and `patch_by_filter` keep a row's sparse field, as they keep its vectors; an
  upsert, conditional or not, replaces it with the incoming document's. Neither results nor
  exports return it: stored-vector projection is later.
- **Query**: `"sparse": {"key": weight, ...}`, the same bounds, is a leg after the text
  legs (so `WeightedRrf`, `Sum` and `Max` weights name it last) and counts toward
  `MAX_LEGS`. Alone, rows score the dot product over shared keys, descending; ties in
  engine order (segment, then row; in a sparse-only segment, the fold's input order). A key no row names contributes nothing. Its limit is
  the query's. Fused with others it scores as every leg does.
- **Stored F16** for every sparse field a fold writes (`DEFAULT_ENCODING`), not U8: U8
  scales a weight by its key's largest in the segment, so a row's stored weight moves when a
  compaction merges it. F16's does not: a weight answers its nearest f16 (subnormals kept,
  below about 2^-25 a signed zero), and a ranking is an exact dot product over those. OQ-126's measurement
  and floors stay; its conclusion gets a correction banner.
- **No segment bound**: a refusal at fold would block every later fold of the index, so
  none is added; M177's dictionary holds `u64` key offsets, so no vocabulary overflows it.
- A sparse-only index (M170) works; a dense query on it stays `schema_conflict`. M141's
  footprint counts a pair as key + 4, so a 1,024-key row is about 270 KB of the bound.

## Acceptance criteria
1. A sparse-only index of 300 rows over 2,000 keys, written as rows and as columns, folded
   once and restarted: 50 queries' top 20 equal an exhaustive f32 dot product, summed in
   key byte order over f16-rounded weights, ordered by `total_cmp`, ties in write order;
   with zero and negative
   weights and keys no row names; both batches answer the same ids and scores.
2. `vector` + `sparse` and `text` + `sparse` answer rows from each leg, the sparse leg's
   weight last in `WeightedRrf`; a filter admitting 40 of the rows answers a full `top_k`
   of admitted rows only.
3. 400 naming the document or query and the rule broken: `{}`; 1,025 keys; a 257-byte and
   an empty key; a `"1"`, `null`, `65505` and `1e39` weight; `sparse` an array; a
   `vectors.sparse` beside a `vector`; a `sparse` patch column or patch field; an empty query `sparse`; a sparse leg beside
   `rank_by` or `aggregate_by`, or under `sum` or `max` beside `text`; `facets` beside a
   sparse leg alone answer 200, counting the filtered set. A
   key written twice answers its last weight. A patch keeps the field.
4. A query naming no written key answers no rows, 200; over an unfolded write it answers
   that write's rows; a dense query on a sparse-only index is `schema_conflict`.
5. A cold sparse query over one folded segment takes at most 4 sequential blob rounds,
   three to rank and one for the rows (D-34 as M47 restates it), and exactly the requests
   measured, pinned in the test, on the depth-counting store.

## Test plan
Server tests in `crates/pstore-server/tests/it/sparse.rs`.
Red first: on the parent `sparse` is an unknown field, so 1, 2, 4 and 5 fail, and 3's
asserted messages (the bound, the document) do not match today's unknown-field one.
`impact_encodings_are_measured` keeps its floors and asserts F16. Mutations, each to be
killed: U8 stored (1); the leg's keys or weights dropped (1); the `sparse` column read as an
attribute (1); a bound off by one (3); `sparse` not reserved in `named_field` (3); a patch
dropping the field (3); the filter not applied to the leg (2); the leg placed first (2);
unfolded rows left out (4); a second round for the dictionary (5).

## RA budget
Unchanged: a sparse leg is the dictionary beside the footer and one batch of postings
ranges, no request added. Postings grow from 1 to 2 bytes a weight.

## Risks
- A write holding `sparse` makes its index's run version 3 (M177): a rollback, or a replica
  or branch read by a build before M177, refuses that run until it rolls forward.
- F16 doubles postings bytes against U8; `scripts/depth.sh` builds U8 explicitly and is
  unchanged, while the engine's tests read the default.
- A big vocabulary is a big pinned sidecar, bounded by nothing but the rows a segment
  holds; the depth tests read its bytes.
- Key and nonzero bounds are pstore's own; turbopuffer's may differ, as the row records.

## Tasks
- M178.1: the field, column and query leg, the reservation, F16, docs.

## Review
Spec review round 1 (approve with changes, seven majors, all taken): a repeated JSON key is
its last; weights bounded by f16's range; criterion 3 red by its messages; ties in engine
order; `sparse` reserved; the measured default test named; key offsets. Round 2 (approve
with changes, one major, taken): no refusal at fold, `u64` key offsets in M177 instead;
minors taken.
Amended in implementation: 1,025 keys are refused first by the body's shape bound (M-era
`ingress/shape.rs`, an object of at most 1,024 members), 413 naming it, not 400.
Code review round 1: `sum` and `max` take a sparse leg only alone, since a dot product can
be negative and those fusions add text legs' non-negative scores (as they refuse a dense
leg); `rank_by` and `aggregate_by` refuse it, as they refuse every ranking leg; criterion 5
names the rows' round D-34 as M47 restates it allows, which the test already counted.
Round 2 (changes, one major, taken): `facets` take a sparse leg as their ranking, tested.
