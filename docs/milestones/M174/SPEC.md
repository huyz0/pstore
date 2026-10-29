# M174: column upserts and null in an upsert

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md), its
row/column ingestion and null targets. Closes the `upsert_columns` row of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md); patches have columns (M13).

## Delta
Today upserts are `documents`, one object per row, and a `null` attribute in one is
refused. After:
- **`upsert_columns: {"id": [..], "vector": [..], "vectors": [..], "text": [..], "<attr>":
  [..]}`**, each column an array of one cell per id. `id` is required. `vector`, `vectors`
  and `text` are those document fields, each cell spelled as the row form spells it (an
  array or base64 vector, M9d); every other column, `attributes` included, is an attribute.
- **Normalization.** Row *i* is `{"id": id[i], <field>: cell[i], "attributes": {<attr>:
  cell[i]}}` with every `null` cell left out, read as a `documents` entry is: the same
  deserializer, known types (M173) and refusals. **Right after the body is parsed**, the
  rows are appended to `documents`, after its entries (as `patch_columns` follows
  `patch_rows`), so a later duplicate id wins and every rule reading `documents` reads
  both; `documents_written` counts both. `upsert_columns: null` is absent; `{"id": []}`
  holds no rows, so alone it is the empty write, refused.
- **Null in an upsert** (a row or a cell) means **the row does not hold that attribute**,
  as omitting it does: removed after the name checks (empty, `$`, `id`), before any other
  rule reads the row. So a declared attribute only `null` is "declared and no document
  carries it", and a null `attributes.text` beside `text` is not given twice. `"vectors":
  null`, refused today, is absent as `"vector": null` and `"text": null` are. A patch is
  unchanged (M13): `null` removes, an omitted name is kept. A `null` array element is still
  refused (nullable arrays are later). Two existing tests change: the `null` cases of
  `attributes::a_value_the_format_cannot_store_is_refused_not_coerced` and
  `typed_values::what_still_has_no_type_is_refused` move to criterion 3 as acceptances.
- **Refused, 400, nothing buffered:** `upsert_columns` not an object; no `id` array; a
  non-string id; a column not an array, or of another length than `id`; a column named
  empty or `$`-prefixed, whatever its cells; a cell the row form refuses, named by its id.
  A `vector` column with null cells beside others is mixed widths, `schema_conflict` at the
  door; a vector-free batch into a folded dense index is `schema_conflict` by the engine
  (M170).

## Acceptance criteria
1. A batch as `documents` and as `upsert_columns` (vectors base64 and array, a named vector
   field, text, integers, floats, bools, strings, arrays, a declared uuid and datetime, a
   null cell) normalizes to equal `Document`s, the row form spelling the null as `null`.
2. Through the API, that batch written to two indexes, one per form, then folded: a vector
   query answers the same ids, distances and attributes from both; a BM25 query, the same
   ids and scores.
3. An upsert's `null` attribute or `vectors` is accepted and stored absent; `{"a": [1,
   null]}`, `{"$x": null}` and a declared attribute only `null` are 400; a later patch's
   `null` removes an attribute, and an omitted one is kept.
4. Each refusal of the Delta's list answers 400, a valid `documents` row before the
   columns; the vector-free case is columns alone into an index made dense and folded,
   `schema_conflict`. After each, an unfolded query and a fold find nothing of it.
5. `documents` and `upsert_columns` with one duplicate id write the column row's version;
   `documents_written` counts every row of both.
6. A column-only applied write declaring a uuid and `full_text_search`, with an
   `upsert_condition` and `affected_ids` (`PSTORE_APPLIED_WRITES=2`), over one id the
   condition admits and one it refuses: the first is written and named in `affected_ids`;
   the second keeps its version and is not named.
7. Existing tests pass; none is weakened beyond the two changes the Delta names.

## Test plan
A unit test beside `patch_rows_of`'s for 1; server tests in
`crates/pstore-server/tests/it/upsert_columns.rs` for 2 to 6. Red before: 2 to 6 on the
parent (`upsert_columns` is an unknown field, and a null attribute is refused). Mutations,
each to be killed: a null cell kept rather than omitted (1); the columns placed before
`documents` (5); a length mismatch tolerated by truncation (4); a column cell read by a
path other than the row form's (1, by the base64 and declared-type cases); null in an
upsert refused, or in a patch read as absent, or a null array element dropped (3); the
column rows appended after the request-level rules read `documents` (6).

## RA budget
Unchanged: the columns become documents before the engine sees any; no request is added
and no format changes.

## Risks
- A column write holds its rows in memory up to three times (columns, documents, and the
  copy the HEAD re-read keeps); bounded by the body limit (M128).
- Accepting `null` in an upsert relaxes a refusal: a client that relied on the 400 to catch
  a bug now stores the row without the attribute. Recorded in `docs/api-migrations.md`.

## Tasks
- M174.1: `upsert_columns` in `WriteRequest`, its normalization, null in an upsert, docs.

## Review
Spec review round 1 (block, three majors, all taken): the one existing test the null rule
changes is named; normalization happens right after parsing, so every reader of
`documents` sees the column rows, with criterion 6 for the readers 2 to 5 would miss; the
two width cases are split, the vector-free one against a folded dense index. Minors taken:
`"vectors": null` means absent in rows too; a declared attribute only `null` is refused;
`upsert_columns: null`, a column named `attributes` and `{"id": []}` are stated; the
reader mutation; up to twice in memory; a null array element still refused. Round 2
(approve, four minors, all taken): names checked before nulls are dropped; criterion 6
seeds a row its condition refuses; three copies; sections trimmed. During implementation a
second existing test asserting `null` refused was found and is named beside the first.
