# M174: column upserts and null in an upsert

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md):
"Row/column batches normalize to identical documents. Missing/null vector columns and
column-length mismatches are refused atomically. Null unset and patch behavior are
specified independently from absent attributes." Closes the `upsert_columns` row of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md). Patches already have a
column form (`patch_columns`, [M13](../M13/SPEC.md)); upserts do not.

## Delta
Today a write's upserts are `documents`, one object per row, and a `null` attribute value in
one is refused. After:
- **`upsert_columns`.** A write may carry `upsert_columns: {"id": [..], "vector": [..],
  "vectors": [..], "text": [..], "<attr>": [..]}`: an object of columns, each an array with
  one cell per id. `id` is required; every other column is optional. `vector`, `vectors`
  and `text` are the document fields of those names, each cell spelled as the row form
  spells that field (a vector as a JSON array or base64, M9d); every other column is an
  attribute.
- **Normalization.** Row *i* is the document `{"id": id[i], <field>: cell[i], "attributes":
  {<attr>: cell[i]}}` with every `null` cell left out, read exactly as a `documents` entry
  is: the same deserializer, the same conversion by known types (M173), the same refusals.
  The column rows follow `documents` in the request's order, as `patch_columns` follows
  `patch_rows`, so a later duplicate id wins as it does within `documents`. Every rule that
  reads `documents` (widths, `upsert_condition`, `affected_ids`, the empty-write refusal)
  reads both. The response's `documents_written` counts both.
- **Null in an upsert.** An upsert replaces the whole row, so a `null` attribute value in a
  row (`documents`) or a cell (`upsert_columns`) means **the row does not hold that
  attribute**, exactly as omitting it does. A patch is unchanged (M13): `null` removes the
  attribute, and a name the patch omits is left as it is. `null` as an array element is
  still refused (nullable arrays are a later slice).
- **Refused, 400, the whole request, nothing buffered:** `upsert_columns` not an object; no
  `id` column, or one that is not an array; an id that is not a string; a column that is not
  an array, or whose length differs from `id`'s; a column named with an empty name or `$`
  prefix, as an attribute is; any cell the row form would refuse, named by its id. A
  `vector` column with some null cells beside non-null ones is a batch of mixed widths, and
  a batch with no `vector` column written to a dense index is a vector-free batch: each is
  refused `schema_conflict` by the rule that refuses the row form (M170).

## Acceptance criteria
1. A batch given as `documents` and as `upsert_columns` (vectors, a named vector field,
   text, integers, floats, bools, strings, arrays, a declared uuid and datetime, a null
   cell, base64 and array vectors) normalizes to equal `Document`s, compared field by field,
   with the row form leaving the null cell's attribute out.
2. Through the API, the same batch written to two indexes, one per form, then folded: a
   query returns the same ids, distances and attributes from both, and a BM25 query the
   same ids and scores.
3. A `documents` entry whose attribute is `null` is accepted and stored without it; a later
   patch setting `null` removes an attribute, and one omitting it leaves it.
4. Each refusal of the Delta's list answers 400 (`schema_conflict` for the two width cases,
   with a valid row before it in the same request); afterwards an unfolded query and a
   fold find nothing of that request.
5. A request carrying both `documents` and `upsert_columns` with one duplicate id writes the
   column row's version; `documents_written` counts every row of both forms.
6. Existing tests pass; none is weakened.

## Test plan
A unit test beside `patch_rows_of`'s for 1; server tests in
`crates/pstore-server/tests/it/upsert_columns.rs` for 2 to 5. Red before: 2 to 5 on the
parent (`upsert_columns` is an unknown field, and a null attribute is refused). Mutations,
each to be killed: a null cell kept rather than omitted (1); the columns placed before
`documents` (5); a length mismatch tolerated by truncation (4); a column cell read by a
path other than the row form's (1, by the base64 and declared-type cases); null in an
upsert refused, or in a patch read as absent (3).

## RA budget
Unchanged: the columns become documents before the engine sees any; no request is added
and no format changes.

## Risks
- A column write duplicates its rows in memory once while normalizing; it is bounded by the
  body limit (M128) that already bounds the request.
- Accepting `null` in an upsert relaxes a refusal: a client that relied on the 400 to catch
  a bug now stores the row without the attribute. Recorded in `docs/api-migrations.md`.

## Tasks
- M174.1: `upsert_columns` in `WriteRequest`, its normalization, null in an upsert, docs.
