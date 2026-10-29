# M176: typed document ids

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "persistent
typed attributes/IDs". Closes the `IDs` row of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md) for `uint64` and `uuid`.

## Delta
Today an id is a JSON string. After, an index's ids are of **one kind**: `string` (today's),
`uint64` (a JSON integer) or `uuid` (8-4-4-4-12 hex, any case, answered lowercase).
- **Declared or inferred.** `schema: {"id": "uint64" | "uuid" | "string"}`, nothing else
  (no `regex`, no full-text). Undeclared, a new index's first write infers `uint64` from
  integer ids, `string` from strings; `uuid` is only declared.
- **Held** as strings naming the kind: NUL, `u`, 16 lowercase hex digits; NUL, `x`, 32. Each
  orders as its values do, so `rank_by: id`, id ranges and id tie-breaks are unchanged, and
  two spellings of a uuid are one row. A NUL-led string id is refused, 400. No codec changes.
- **The door** reads every id and id literal a write carries (rows, columns, patches,
  deletes, `delete_by_filter`, patch filters, `upsert_condition`) by the known kind, as M173
  reads types: recorded (under the name `id`), else this process's unfolded kind, `string`
  included, else the request's; a recorded schema with no kind is `string`. M35's one HEAD
  read per process supplies it. Another kind is 400 `schema_conflict` after M173's one
  re-read; an id not of its kind, or a batch mixing kinds, is 400; nothing buffered.
- **A query** reads `id` literals by the schema of the HEAD it ranks from (`as_of`, replicas),
  plus this process's unfolded kind when unpinned: an integer is the `uint64` form; a string
  the `uuid` form under `uuid` (400 if malformed), verbatim under `string`, both as an `Or`
  when no kind is known. A literal of the other typed kind (in an `In` too), or a pattern
  operator on a typed `id`, is 400. `$ref_new: "id"` is the held form
  against `id`, the typed value against an attribute.
- **Recorded** only by the fold that records the index's schema: the first upsert's kind in
  lane order, superseded and deleted ones counted. Upserts of another kind, then or later,
  are quarantined as M173's are; patches and deletes of another kind match nothing. A branch
  inherits it; M147's empty index (no schema taken) is `string`.
- **Answered** from the form at serialization only: results, `affected_ids`, quarantine
  exports and errors give `uint64` as an integer, `uuid` lowercase hyphenated. The summary's
  `schema.id` is the recorded kind, else `"string"`, never in `schema.attributes`.
  Unchanged: string ids, external indexes (M86), replication (copies rows as held).

## Acceptance criteria
1. Ids `2`, `10`, `2^53+1`, `u64::MAX` as integers (rows, columns), unfolded, folded and
   after a restart: answered exactly; ordered numerically by `rank_by: id` and a fusion tie;
   `Eq`, `In`, `Gt`, `Lt` admit a reference's rows; row and column patches, a delete,
   `delete_by_filter` and `upsert_condition` act on their row; `affected_ids` are integers.
2. `schema: {"id": "uuid"}`, ids uppercase: answered lowercase; an undeclared lowercase write
   overwrites the row; an uppercase `Eq` admits it, also from another process before a fold;
   `rank_by: id` is byte order; paging by `Gt` visits every row once; `$ref_new: "id"`
   matches a uuid attribute holding the row's id.
3. 400, nothing buffered, naming the id as written: under `uint64` (recorded, restarted) an
   id `"5"`, `-1`, `1.5`, `2^64`, a query's or `delete_by_filter`'s string literal; under
   `uuid` a malformed id or literal, an integer id; a `Glob` under each; an `In` or a batch
   mixing kinds; a NUL-led string;
   `id: datetime`; `schema_conflict` for `id: uint64` over a schema, and for an integer id
   after this process's unfolded string write.
4. Two APIs, one store, a new index, no fold between, integer ids and string ids in each
   order: one fold records the first's kind, quarantines the other's rows (exported in their
   spelling); the loser's next write is `schema_conflict`; a later fold records nothing new.
5. `schema.id` answers each kind, `string` for a string index, an unfolded one, a re-created
   one and an M147 empty one, before and after a restart; a branch's is its parent's.
   Existing tests pass; an integer `id` literal on a string index admits nothing.

## Test plan
Server tests in `crates/pstore-server/tests/it/typed_ids.rs`; the form unit-tested beside it;
the fold's recording in the engine. Red before: 1 to 5 on the parent (an integer id is
refused, `schema.id` absent); 5's last clauses are regression guards. Mutations, each to be
killed: the form unpadded or decimal (1's order); decoded before fusion (1's tie); a uuid kept
in its case (2); `$ref_new` giving the held form against an attribute (2); the `Or` dropped (2's other process); a mismatched kind or a NUL-led string
admitted (3); the kind recorded by a later fold, from the last upsert, or with no quarantine
(4); an answer not decoded (1, 2, 4); `id` in `schema.attributes` (5).

## RA budget
Unchanged: a write reads the kind from the HEAD M35 already reads, a query from the HEAD it
already ranks from; no request is added.

## Risks
- Ids hold bytes a client never sent: a path printing an id undecoded shows NUL. Criterion 3's
  messages and 4's export check the answer paths beyond results.
- Write metering counts the held form (18 or 34 bytes), not the JSON spelling.
- A string id stored before M176 that began with NUL then `u` or `x` and a valid tail would
  answer as typed. None can be written after M176; accepted as negligible.
- A rollback to a build before M176 answers typed ids in their held form.

## Tasks
- M176.1: the held form, door and query reading, answers, summary, recording and quarantine,
  docs.

## Review
Spec review round 1 (approve with changes, four majors, all taken): caps; a query reads id
literals by the schema of the HEAD it ranks from, so no request is added; ids decode only at
serialization, as fusion and `limit_per` order by them; every Delta claim has a criterion.
Minors taken: `$ref_new`, superseded upserts counted, only the creating fold records, M147,
pattern operators, metering, `In` mixing kinds, `id` takes no `regex`. Round 2 (approve, five
minors, all taken).
