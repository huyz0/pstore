# M176: typed document ids

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "persistent
typed attributes/IDs". Closes the `IDs` row of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md) for `uint64` and `uuid`.

## Delta
Today an id is a JSON string. After, an index's ids are of **one kind**: `string` (today's,
the default), `uint64` (a JSON integer in `0..=u64::MAX`) or `uuid` (a string of 8-4-4-4-12
hex digits, any case, answered lowercase).
- **Declared or inferred.** `schema: {"id": "uint64" | "uuid" | "string"}`; with none, an
  index's first write of JSON-integer ids infers `uint64`, and of strings `string`. `uuid` is
  only declared, so no string id is ever reread as one. Other kinds for `id` are refused.
- **Held.** Ids stay strings inside the engine, in a form that names its kind: `uint64` is
  NUL, `u`, then 16 lowercase hex digits; `uuid` is NUL, `x`, then 32 lowercase hex digits.
  Each orders as its values do, so `rank_by: id` and id ranges need no change, and two
  spellings of one uuid are one row. A `string` id beginning with NUL is refused (400), so
  no string id takes either form. Nothing in a segment, bundle or HEAD codec changes.
- **Read at the door** by the index's known id kind, as M173 reads attribute types: the
  kind its schema records (under the name `id`, which no attribute can take), else this
  process's unfolded writes' kind, `string` included, else the request's. An index with a
  recorded schema and no recorded id kind is `string`. Every id a write carries is read so:
  `documents`, `upsert_columns`, `patch_rows`, `patch_columns`, `deletes`, and every id
  literal of a filter (`delete_by_filter`, patch filters, `upsert_condition`). A kind that
  differs from the known one is refused, 400 `schema_conflict`, naming both kinds, with
  M173's one HEAD re-read before it stands; an id not of its kind is 400; nothing buffered.
- **Read in a query** the same way: an integer literal on `id` is always a `uint64`'s form
  (matching nothing in a string index, as today); a string literal is a `uuid`'s form when
  the known kind is `uuid`, refused 400 if malformed then, and verbatim otherwise. An
  integer literal against a known `uuid` kind, or a string one against `uint64`, is 400.
- **Recorded** by the fold that records the index's schema (its first, or the first after a
  drop): the kind of the first upsert in lane order, by its id's form. Every upsert that fold
  or a later one seals whose id is of another kind is quarantined, counted in
  `quarantined_rows`, as M173 quarantines a wrong-kind value. Patches and deletes of another
  kind match no row. A branch inherits the kind; M147's empty index is `string`.
- **Answered** from the form, needing no kind: query results, `affected_ids`, quarantined
  rows and error messages spell a `uint64` id as a JSON integer and a `uuid` id lowercase,
  hyphenated. The index summary's `schema` gains `id`: the recorded kind, else `"string"`;
  `schema.attributes` never lists it.
- **Unchanged:** string ids, their order and answers; external indexes (M86), whose ids are
  the customer object's strings; replication, which copies rows as stored.

## Acceptance criteria
1. Ids `2`, `10`, `2^53 + 1` and `u64::MAX` written as JSON integers (rows and columns):
   unfolded, folded and through a restarted API, results answer them as exact JSON integers;
   `rank_by: id` asc orders them `2, 10, 2^53+1, max`; `Eq`, `In`, `Gt` and `Lt` integer
   literals admit what a reference over the values does; a patch by row and by column, and a
   delete, by integer id act on that row; `affected_ids` answers integers.
2. `schema: {"id": "uuid"}`, ids written uppercase: answered lowercase; a later undeclared
   write of one in lowercase overwrites the same row; `Eq` with an uppercase literal admits
   it; `rank_by: id` follows byte order; paging by `Gt` the last id answered visits every row
   once.
3. Refused, 400, nothing buffered: once `uint64` is known (recorded, and after a restart) an
   id `"5"`, `-1`, `1.5` or `2^64`, and a filter's string literal on `id`; once `uuid` is
   known a malformed id or literal and an integer id; one batch mixing integer and string ids;
   a string id beginning with NUL; `id` declared `datetime`; `id` declared `uint64` on an index
   with a recorded schema, and an integer id after this process's unfolded string write, as
   `schema_conflict`.
4. Two APIs over one store, a new index, no fold between: one writes integer ids, the other
   string ids, in each order. After one fold the first upsert's kind is recorded, the other's
   rows are quarantined (counted, answered in their own spelling by the quarantine export) and
   the summary answers the kind; the losing API's next write of its kind is `schema_conflict`.
5. The summary answers `schema.id` (`uint64`, `uuid`, or `string` for a string index and one
   with no schema), before and after a restart; `string` again after a drop and re-create.
6. Existing tests pass unchanged; an integer `id` literal on a string index admits nothing.

## Test plan
Server tests in `crates/pstore-server/tests/it/typed_ids.rs`; the form's codec unit-tested
beside it; the fold's quarantine in the engine. Red before: 1 to 5 on the parent (an integer
id is refused, `schema.id` absent); 6 is a regression guard. Mutations, each to be killed:
the form's hex unpadded or decimal (1's order); a uuid kept in its written case (2); a
mismatched kind accepted at the door, or a NUL-led string admitted (3); the kind recorded by
a fold other than the creating one, or from the last upsert, or no quarantine (4); an answer
not decoded from the form (1, 2, 4); `id` listed in `schema.attributes` (5).

## RA budget
Writes unchanged: the door reads as M173's does. A query with a string literal on `id`, for
an index whose kind this process does not know and that has no schema it has read, re-reads
HEAD once (1 GET, sequential) before ranking; any other query adds nothing.

## Risks
- Ids now hold bytes a client never sent: anything printing an id unanswered (a log, a
  quarantine export) shows NUL. Every answer path is named above and decoded.
- A stored string id that already began with NUL, `u` or `x` and a valid tail would answer as
  a typed id. None can be written after M176; before it, accepted as negligible.
- A rollback to a build before M176 answers typed ids in their held form.

## Tasks
- M176.1: the id form, door and query reading, answers, summary, fold recording and
  quarantine, docs.
