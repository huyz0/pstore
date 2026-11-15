# M262: an empty index created with a declared schema

## Serves
The [turbopuffer gap list](../../comparisons/turbopuffer-gaps-2026-11-15.md), "Shipped by the
vendor after the pinned spec": `create_namespace: true` with a declared schema. Workstream G
of [the delivery plan](../../comparisons/turbopuffer-next-plan.md). No D- or OQ- id covers it.

Today `PUT /v1/indexes/{index}` ([M147](../M147/SPEC.md)) takes `dims` (required, at least 1)
and `distance_metric`, and records every other schema field at its default. A schema
(analyzers, further text fields, regex attributes, attribute types, the id kind) is
recorded only by the first write that declares it. And a text-only index (width 0,
[M170](../M170/SPEC.md)) cannot be created empty at all.

## Delta
1. **`schema`** is accepted in the create body: the map a write's `schema` takes, with the
   same parsers (`declared_types`, `declared_texts`, `declared_trigram`), so a declaration is
   refused at create exactly as at a write. The write-only rule "full_text_search/regex is
   declared and the write has no documents" does not apply to a create.
2. **The created schema holds it**: the primary text field's `FullText`, the further text
   fields, the regex set, and each declared attribute type and the id kind, as the first
   fold of a write declaring the same map would record them. The commit is the one HEAD
   CAS of M147; nothing else changes about the create (refusals, retries, caches, answer).
3. **`dims` becomes optional.** Absent or `0` creates a text-only index of width 0, which
   M170's rules then govern (a vector write or query refused). `distance_metric` with no
   width is refused, as it can mean nothing.
4. **Later writes are checked against it** as against any recorded schema: a write declaring
   another analyzer, regex set or type is refused `schema_conflict`; one declaring the same
   or nothing is accepted.
5. **The summary reads it back** (`GET /v1/indexes/{index}`) before any write.

**No setting.** An optional field of an existing request; a body without it creates what
M147 creates, byte for byte the same HEAD.

**Not changed:** the HEAD encoding (every field already encodes, empty or not), M147's
refusals, write and query paths.

## Acceptance criteria
1. **Recorded.** `created_schema_is_recorded`, in `crates/pstore-server/tests/it/create.rs`:
   a create with a primary analyzer (`k1`, `language`, `stemming`), a further text field, a
   regex attribute, a `uint64` attribute and `"id": "uuid"` answers 200; the summary, before
   any write and after a restart, reports each; a later write that declares nothing is
   folded and its text query uses the declared analyzer (a stemmed form matches).
2. **Conflicts.** `created_schema_refuses_other_declarations`: after that create, a write
   declaring another `k1`, another type for the `uint64` attribute, or another regex set is
   refused `schema_conflict` and nothing is folded; one declaring the same map is accepted.
3. **Text only.** `created_text_only_index`: a create with no `dims` and a primary text
   declaration gives an index of width 0 that answers a BM25 query with no rows, refuses a
   vector query and a vector write as M170 does, and accepts a text write; `dims: 0` with
   `distance_metric` is refused 400.
4. **Unchanged without schema.** M147's `create.rs` tests unchanged, and
   `create_without_schema_is_m147s`: a create body without `schema` commits a HEAD whose
   schema equals the one the parent commits.
5. **Refused as a write refuses.** `created_schema_refusals_match_writes`: an unknown key,
   a `regex` on `id`, `full_text_search` on a `uint64`, and `stemming` under
   `pre_tokenized_array` are each refused 400 with the write's message, and nothing is
   committed (the store's PUT count is unchanged).
6. **Gates:** fmt, clippy, the workspace tests (the two IPv6 tests NOT-RUN in this
   container), and the named mutations below observed red.

## Test plan
Each test is seen red first against the parent, where `schema` is an unknown create field.

| AC | Mutation |
|---|---|
| 1 | the further text fields not copied into the created schema; types not copied |
| 2 | the created `fts` left at its default |
| 3 | width 0 refused; the metric refusal dropped |
| 4 | a schema-less create recording a non-default field |
| 5 | the parsers' errors swallowed |

## RA budget
Unchanged: one HEAD read and one HEAD CAS per create (retries as M147). The HEAD grows by the
declared schema's bytes, which the first write would have added anyway.

## Risks
- **Two creates racing** with different schemas: the CAS lets one win, and the loser is
  refused `index_exists`, as M147's are.

## Tasks
- **M262.1** The spec.
- **M262.2** The create body, the engine's create taking a declaration, tests, OpenAPI and
  clients regenerated, docs.
