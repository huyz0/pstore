# M173: persistent attribute types

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "add
persistent typed attributes ... and schema GET", and "No ... inconsistent request-local
typing; schema enforcement covers concurrent first writes". Closes "persistent
declarations" in the "Scalar attributes" row of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md). Builds on the
request-declared types of [M9h](../M9h/SPEC.md), [M171](../M171/SPEC.md) and
[M172](../M172/SPEC.md).

## Delta
Today a write's `schema` types an attribute for that request alone, and the index summary
answers no attribute types. After, a type is **recorded in the index's schema** and holds
for every later write:
- **The door.** A write's attributes are read by its *known* types, the union of: the
  types the schema this process last read records (as for width, fetched first if it has
  read none, M35); this process's own unfolded declarations, kept in memory per index; and
  the request's own. The recorded type wins: a map entry leaves once the schema this
  process reads records that name, whatever kind it records. A declaration that differs from a known type is refused, 400
  `schema_conflict`, naming the attribute and both types; nothing is buffered. Every full
  row and patch carries, in a reserved `$types` attribute, the kind of each known-typed
  name it holds, declared or not; it is stripped before a seal and never answered.
- **The fold.** A name's type is recorded from the first `$types` naming it among every
  operation the fold read, in lane order, superseded or deleted ones included (as the
  analyzer is, M14). Then every upsert the fold seals (plain or conditional, not a row
  built by merging a patch onto a stored one) is judged against the recorded types,
  wherever it sits in the fold: a `$types` naming another kind, or a value under a
  recorded name that is not of its kind (scalar, or any array element), quarantines the row,
  counted in `quarantined_rows`, as a wrong-width row is today. This covers concurrent first
  writes: two processes that type one name two ways, or one that types it and one that has
  not yet seen the type, never both reach the index through upserts.
- **Not judged:** patches (typed at the door only), and rows already sealed, which keep
  their values when a later fold, compaction or GC rewrites their segment. A later write
  may type a new name; its first fold adds it. A branch inherits its parent's types; an
  index created after a drop starts with none.
- **One name, one type.** A name's values of another type are rows sealed before its type
  was recorded, which keep them. M171's and M172's tests that wrote several types under one
  name now write the untyped rows first and fold them before the declaring write, or, for
  two declared types, write them as a build before M173 did: by an engine, with no
  declaration. None loses a case.
- **When the door reads.** The known types are read after a write's request-level checks,
  before its values are read: a value refused costs, once per process, the HEAD read M35
  already makes. A refusal by a type the request did not declare re-reads HEAD once before
  it stands, as a width's does (M9f.2), and a drop this process makes, or reads after its
  declaration, takes its unfolded declarations for that index.
- **Answering.** The index summary's `schema` gains `attributes`: each recorded name to
  `"datetime"`, `"uuid"`, `"bytes"` or `"uint64"`; `{}` when none.

HEAD gains a trailing section after `keys_since`, the recorded types per live and dropped
schema, written only when some schema records one, so a HEAD with none keeps its bytes. It
forces `keys_since`, written as `u64::MAX` when absent; that value with no section after it
is refused. ⚠️ **Merged after M152**, which landed first with its own trailing section (the
further text fields) and reads `u64::MAX` as absent wherever it appears: the types follow
the text fields' section, which they force (empty when there are none), and the sentinel
rule is M152's. A reader before M173 refuses such a HEAD, and once one is committed an older
build cannot read the tenant: deploy readers first, and no rollback after.

## Acceptance criteria
1. A write declaring `owner: uuid`, `seq: uint64`, `at: datetime` and `blob: bytes`,
   folded, then through a restarted API a write declaring nothing: its values are stored as
   those kinds (answered lowercase, exactly, formatted); a malformed one is refused, 400,
   nothing buffered.
2. Before any fold, a second undeclared write in the same process, to an index with a
   recorded schema and to a new one, is read by the first's declarations.
3. Declaring a recorded or unfolded name as another kind is refused, 400
   `schema_conflict`, naming the attribute and both kinds; nothing buffered.
4. Two APIs over one store, no fold between: (a) one declares `owner: uuid`, the other
   `owner: bytes`; (b) one writes `owner` as a plain string, then the other declares it
   `uuid`. After one fold, the kind of the first `$types` in lane order is recorded, and
   the rows contradicting it are quarantined (counted, not returned) whichever came first;
   in (b) the string row is quarantined. Then the API whose declaration lost writes
   `owner` undeclared: it is read as the recorded kind, and accepted. A patch of another
   attribute onto a row holding an untyped `owner` is applied, not quarantined.
5. The summary answers `schema.attributes`, exactly the recorded kinds, before and after a
   restart; `{}` for an index with none, and for one re-created after a drop.
6. HEAD: types round-trip for live and dropped schemas; a HEAD with none encodes as before;
   `keys_since` absent beside types round-trips as absent; bytes after the types are
   refused. (Before the merge after M152: also `u64::MAX` with no section; see the Delta.)
7. No sealed row carries `$types`; existing tests pass; those extended lose no case.

## Test plan
Server tests in `crates/pstore-server/tests/it/persistent_types.rs`; HEAD tests beside
`keys_since_round_trips_after_the_m131_bytes`. Red before: 1 to 5 on the parent.
Mutations, each to be killed: the recorded types not read at the door (1); the unfolded
map not read (2); a conflicting declaration accepted (3); the last `$types` recorded, or
a contradicting value not quarantined, or only rows after the declaring one judged, or the
losing map entry kept, or a patched row judged (4);
`attributes` omitted (5); the dropped schemas' types not written; the section written with
no types; the sentinel read as an epoch (6); `$types` not stripped (7).

## RA budget
Unchanged: no request is added. A write reads the schema the process holds (M35 already
fetches it once); the section rides the HEAD every reader fetches.

## Risks
- A quarantined row was acknowledged to its writer, as a wrong-width row is. Reached by a
  race, or by a process whose first HEAD read failed or whose cached schema predates the
  type: bounded by when it next reads HEAD (any fold, query or write that refreshes it).
- An untyped patch value from such a process is stored; filters still read it by its own
  type. Judging patches is a later milestone's.
- Another thread adding a HEAD section conflicts in `head.rs`: the later rebases.

## Tasks
- M173.1: the kind in `pstore-format`, the schema's field, the HEAD section, `$types`, the
  door's known types and the fold's record and judgement.
- M173.2: the server's reading by known types, the summary's `attributes`, docs.

## Review
Spec review round 1 (six majors, all taken): the type comes from the fold's first `$types`,
and every row is judged against it; superseded declarations count; every typed value
carries its kind; the unfolded map, not a walk of held rows; patches and sealed rows are
not judged; the risk is bounded by HEAD reads, not only races. Minors taken: the sentinel's
refusal and no rollback; branches and drops; `$types` stripped; the Delta shortened.
Round 2 (two majors, both taken): rows merged from a patch are not judged; the recorded
type evicts the unfolded map's entry, with a criterion for the process that lost. Minor
taken: "through upserts". Round 3 (verify): resolved, nothing new. The "One name, one
type" and "When the door reads" bullets were added during implementation, and were
reviewed with the code.
