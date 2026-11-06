# M198: which vector fields an index searches by ANN

## Serves
Workstream D of [the plan](../../comparisons/turbopuffer-next-plan.md): "Named vector widths
and metrics survive restart; advertise ANN only for fields with an actual ANN index". Closes
the "named-field ANN contracts" remainder of D's row in
[DELIVERY](../../comparisons/DELIVERY.md) and corrects the "Multiple dense attributes" row of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md), which still says named dense
search scans exactly although [M83](../M83/SPEC.md) clusters a named field. No `D-` or `OQ-`
decision is reopened; D-34's budget is unchanged.

## What holds today
- Widths survive restart: `vector`'s in `IndexSchema.dims`, each named dense field's in
  `IndexSchema.fields` (M98), answered as `schema.dims` and `schema.vectors`. The metric is the
  index's, one for every field (M9d), answered as `schema.distance_metric`. Nothing to add.
- Whether a query on a field is searched by an ANN index is decided **per segment**, at seal,
  and is recorded nowhere a client can read before it queries:
  - `vector`: the segment has a centroid table (`built.centroids`), which happens only above
    `exact_scan_threshold` rows.
  - a named dense field (M83): the segment's `FdeSet` holds clusters of more than one list for
    it, which happens only when every row of the segment carries the field with one vector,
    and the set is written (`worth_writing`).
  - Otherwise the field is scanned exactly in that segment. Unfolded rows are always scored
    exactly, in memory.
- A query's `explain` (M165) reports `approximate_segments` and `exact_segments` per dense leg
  after the fact. The index summary says nothing, so a client cannot tell an ANN field from an
  exactly scanned one, and the docs advertise ANN for every field.

## Delta
- **HEAD records it.** Each segment ref gains the set of dense fields a query searches by
  clusters in that segment, recorded by what the query itself checks: `vector` when the seal
  wrote a centroid table, and a named field when the seal wrote an `FdeSet` (`dicts.fde`) that
  names `dense_name(field)` (`run.rs`: the leg reads clusters whenever the set has the field,
  whatever its list count). The seal built both, so every caller of `seal` records it with no
  request: the fold, a compaction, and a replica's rebuild (`replica.rs`), which re-seals.
- **Encoding: one trailing HEAD section** after the current last one. HEAD is one object a
  tenant, so the section is the tenant's: written whenever any ref of any index has a known
  set (always, for a HEAD this code writes with a folded segment), forcing the earlier optional
  sections as M15.2's rule does. It holds one entry for each index with a known ref, even when
  every set is empty: the index's name, a table of the field names its refs name, then per
  ref, in ref order, a count byte (`0xFF` unknown) and that many one-byte ordinals into the
  table. An index with no entry, a HEAD written before M198, a ref resurrected for `as_of`, a
  ref re-encoded by a node before M198, and an external index's refs (M86) are unknown.
  Malformed input (a count of refs that disagrees with the index's, an ordinal past the table,
  a duplicate name or a duplicate index, an index HEAD does not name) is `CorruptHead`, as the
  M32 section's is.
- **Refs that move keep it.** A branch, a copy, a replica's sync and a compaction that keeps
  a segment carry the ref's set with it, as they carry `dicts` and `len`.
- **Answer.** `GET /v1/indexes/{id}` answers `vector_search`, one entry per dense field the
  schema records (`vector` when `dims > 0`, and each key of `schema.vectors`):
  `{"approximate_rows": a, "exact_rows": e, "unknown_rows": u}`, where each is the summed
  `rows` of the folded segments whose set names the field, whose set is known and does not,
  and whose set is unknown. `exact_rows` counts a segment's rows even when some of them lack
  the field: what it says is that a query on the field scans that segment exactly. Absent
  before the first fold, like `schema`. Unfolded rows are in none of them, and the docs say
  they are scored exactly. Under a filter every leg is exhaustive and probes every list
  (M9b): a segment still counts as approximate there, as `explain` counts it, and the docs say
  so.
- **Docs.** The API reference says which fields are ANN-searched, how to read
  `vector_search`, and that `exact: true` scans every field exactly. The comparison and
  DELIVERY rows are corrected.
- **Deploy readers first.** A reader before M198 refuses a HEAD holding the new section
  (`CorruptHead`: the decoder refuses bytes after the last section it knows, `head.rs`'s end
  check), as for M173 and M195. No rollback after.
- **Unchanged:** how a query searches, every query's answers, depth and requests, the seal's
  segment bytes and sidecars, and `explain`.

## Acceptance criteria
1. Agreement with what a query does (engine test, `with_index_params` lowering `vector`'s
   threshold; a named field clusters at the default params, so above 4,000 rows a segment):
   an index with `vector` and two named dense fields, `a` carried by every row and `b` lacking
   at least one row of every segment, folded into three segments, two above both thresholds
   and one below, with rows left unfolded, then reopened from the store. For each field, the
   number of segments the summary's sets name equals a default query's `explain`
   `approximate_segments` on that field, the known others its `exact_segments`; `b`'s set is
   empty in every segment and `a`'s and `vector`'s hold in the two large ones. Through the
   server (small segments, default params): `vector_search` answers every field's rows as
   `exact_rows`, the three counts of each field sum to `documents`, and the same after a
   compaction, from this process and a fresh one.
2. Encoding: HEAD round-trips the section beside every other optional section, an index whose
   sets are all empty included; each malformed input above is refused; a HEAD without the
   section decodes every ref as unknown, and `vector_search` answers all rows as
   `unknown_rows`; a ref carried by a branch, a copy and a replica's sync answers the
   source's set, and a replica's rebuild answers what its seal built.
3. Cost: the depth and request counts of a write, a fold, a compaction, a summary and a cold
   query are unchanged (M195's pins and the existing cost tests stay green), and
   `scripts/index-scale.sh` stays green (HEAD within its 256 bytes an index at 5,000 indexes).

## Test plan
Server tests in `crates/pstore-server/tests/it/vector_search.rs`, engine HEAD tests beside
`the_dicts_section_round_trips_and_refuses_malformed_input`. Red first: on the parent the
summary has no `vector_search`, and the engine tests call what the parent does not have.

Mutations, each to be killed:
- `vector` recorded whatever the centroid table (1);
- a named field recorded whatever the set names, or `b` recorded (1);
- the set not carried by a compaction, a branch or a sync (1, 2);
- the count byte or an ordinal decoded off by one, unknown read as empty (2);
- an index whose sets are all empty given no entry (2);
- `exact_rows` and `unknown_rows` swapped (2).

## RA budget
No request is added: the seal already holds what it records, and the summary already reads
HEAD. HEAD grows, a tenant, by the section's count and any earlier optional sections it forces;
an index, by its name, its table's names and count, then one byte a ref plus one a recorded
field. No segment byte changes.

## Out of scope
Declaring per field whether to build an ANN index (turbopuffer's per-attribute `ann`), and
clustering a named field that some rows lack. Both change what is built; this milestone only
reports what is.

## Risks
- A segment sealed before M198 answers `unknown_rows` until a compaction rewrites it.
- `index-scale.sh`'s HEAD bound is the nearest limit: an index's entry is its name plus a
  few bytes.
- The section is keyed by field name, so a table of 8 long names costs up to about 520 bytes
  an index; bounded by the per-index field count.

## Review
Round 1 (a subagent given the spec and the code, no edits): two blockers, both taken. The
section was to be written only for an ANN-searched index, so a known-empty set and an absent
one could not be told apart; and HEAD is one object a tenant, not an index. Now the section is
written whenever a set is known, with an entry for every index with a known ref, and the
"keeps its HEAD bytes" criterion is replaced by `index-scale.sh`'s bound. Majors taken: a named
field is recorded by whether the written set names it (the query's own check), not by its list
count; criterion 1 compares segment counts with `explain`, which counts segments, not rows; the
thresholds (25,000 rows for `vector`, 4,000 a list for a named field) are stated and the
engine test lowers `vector`'s. Minors taken: the replica rebuild and external refs, the HEAD
growth, the filter docs line, and the end-check citation. Declined: a bit in the `Dicts` byte
for `vector`, since a pre-M198 ref with known dicts would then read as exact; the new
section's presence is what says a ref's set is known.
