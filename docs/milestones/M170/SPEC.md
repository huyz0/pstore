# M170: documents with no vector

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md), its first
step: "remove mandatory dummy vectors and define vector-free schema", and the target "A
vector-free document can be written, filtered, BM25-ranked, projected, patched and exported
across restart. Dense/multi-vector queries against an incompatible schema fail explicitly."
Closes the "Row layout" row of [the comparison](../../comparisons/turbopuffer-2026-10-25.md)
for documents without vectors. [api-design.md](../../research/11-design/api-design.md)'s
"all vector dimensions in a field must match" is unchanged.

## Delta
Today every document needs `vector`: a document without one is refused by serde, and
`"vector": []` with "a document with no vector".

After, `vector` may be absent (or `null`, which means the same) on every document of a
write. Such a document is stored with a vector of width 0, the width the engine's schema
already records for an index whose rows have none, so an index whose first fold sealed
vector-free rows records width 0 and keeps it: the schema is the existing `IndexSchema`,
and no codec or HEAD format changes.

Refused, `schema_conflict`, at the door: a batch in which some documents carry a vector
and some do not ("document y has no vector and the batch has 2 dimensions"), as a batch
of two widths is today. Refused, `schema_conflict`, by the engine's existing width check:
a vector-free document into an index whose schema records a width, and a document with a
vector into an index whose schema records 0.

Refused, `bad_request`, at the door: `"vector": []` (an empty vector is not "no vector");
`vectors` (named fields) on a document with no `vector`; `distance_metric` on a write
whose documents carry no vector.

A query with `vector` or `multi` against an index whose schema records width 0 is
refused with `schema_conflict` ("index t has no vectors"), whatever else it carries,
rather than answered by an internal error or an empty list. A query against an index
with no schema yet (nothing folded) is unchanged.

Unchanged: every dense index's IDs, distances, recall and refusals; text, filters,
`rank_by`, projection, patches and aggregations, which never read a vector; the peer
protocol, which carries what the engine stores.

## Acceptance criteria
1. Three vector-free documents, durable and folded: BM25 `text` ranks the two that match;
   a filter narrows that to one; a filtered `rank_by` returns their attributes in order.
2. After a durable patch, a fold and a restart on the same store, paging the index by
   `rank_by: id` with an `id Gt` cursor returns all three rows with the patched value and
   their attributes, and BM25 still ranks the matching row.
3. Against that index, a query with `vector`, with `vector` and `text`, and with `multi`
   each answer `400 schema_conflict`.
4. A vector into the vector-free index, a vector-free document into a folded dense index,
   and a mixed batch each answer `400 schema_conflict`.
5. `"vector": []`, `vectors` without `vector`, and `distance_metric` on a vector-free
   write each answer `400`.
6. Existing dense tests pass unedited, except `errors::` whose refusal of `"vector": []`
   still answers 400.

## Test plan
`crates/pstore-server/tests/it/text_only.rs`, one test a criterion 1 to 5. Red before:
1, 2, 3 and 4's vector-free writes are refused by serde on the parent (`missing field
vector`), and 5 passes on the parent, which refuses every case, so its red is the
mutations below. Mutations, each to be killed:
- the door's mixed-batch check removed (4); the empty-vector check removed (5);
- the named-fields check removed, the metric check removed (5);
- the engine's width-0 query refusal removed (3);
- the width-0 schema recorded as "no schema" (2, 4).
6 is a guard: the workspace suite.

## RA budget
Unchanged: no request is added or removed. A vector-free row's segment has no vector
section, which the writer already skips for a width of 0.

## Risks
- Code below the engine that divides by or indexes with the width; a vector-free fold,
  compaction and restart are exercised by criteria 1 and 2, and a panic would show there.
- A dense query against a vector-free segment reaching the format layer's "unknown field"
  is answered `internal` today; criterion 3 reveals whether the engine's refusal runs first.

## Tasks
- M170.1: the optional `vector`, the door's checks, the engine's query refusal, docs.
