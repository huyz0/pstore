# M7d — The schema: who sets it, whether it may change, and what a disagreement does

**Serves:** **D-34** (`11-design/api-design.md` §Schema — types inferred by default, a schema
change needing a reindex must *say so*), **M6c**'s three unanswered policy questions, and the
`06-indexing/modalities-and-sequencing.md` §3 property that an index's vectors share a width.
Narrows **BACKLOG row 27** — it does not close it, see criterion 6 — and closes the last
**Blocked** row that was blocked on a caller.

**Depends on** [M6c](../M6c/SPEC.md), which recorded the text field per *segment* and refused
per segment, and [M7c](../M7c/SPEC.md), which built the caller these questions needed.

## ⚠️ The three questions, and the answers this milestone commits to

M6c ended with: *who sets the field, may it change on an index that already has segments, and
what does a fan-out do when a tenant's segments disagree.* M7c added a fourth from the other
end — an index's **vector width**, which after a fold no process could know without a read, so
a wrong-width write was accepted and refused only at the next query.

1. **Who sets it: the first fold, by inference.** `api-design.md` says types are inferred by
   default and creation is implicit on first write. So the first fold of an index records what
   its rows actually are, and no request, flag or provisioning step precedes it.
2. **May it change: no, not in place.** A fold whose rows contradict HEAD's schema is
   **refused before it writes a segment**, and the API answers `400 schema_immutable` naming
   the migration path rather than pretending a `PATCH` will work.
3. **A disagreement is refused at the last moment it can be refused *safely*, and after that
   it is dropped and counted — never allowed to stop the tenant.** The first draft of this
   spec refused at the fold, and spec review measured the consequence: a fold is all-or-nothing
   across every index in the bundle set, so one acknowledged wrong-width row would make every
   later fold, by every process, for that whole tenant, fail forever. Watermarks would never
   advance again. **One accepted API call would brick a tenant** — strictly worse than the
   per-index outage M7c had, which is the opposite of the intended direction.

## Delta

**Adds `Head.schemas: BTreeMap<String, IndexSchema>`**, `IndexSchema { dims: u32, text_field:
String }`, encoded as a **new trailing section** of HEAD.

⚠️ **An absent section is "no schemas recorded", never "a schema of zero".** This is M6c's own
trap, one layer up: reading absence as empty there would have turned off the text index of
every segment written before the section existed. `decode` treats end-of-buffer at the section
boundary as an empty map, and a HEAD written before this milestone stays readable — asserted
against bytes from the old encoder, not against a round trip through the new one.

**Changes, and the refusal ladder is the whole design**

1. **`Engine::write` refuses at the door**, against the schema cached from any HEAD read this
   process has made — `fold`, `query`, `indexes`, `index_stats` all read it already — so the
   check costs **no request**. M7c's memtable-local check stays underneath as the cold case.
2. **`Engine::flush` refuses before the rows become durable**, which is the rung that makes
   the rest safe. A flush already reads the lane set on a lane's first flush; the schema is
   read **once per process**, on that same first flush, and cached. So the cost is one read
   per process, never per write, and a wrong-width row cannot reach a bundle through a
   correctly behaving writer at all.
3. **`Engine::fold` drops and counts.** It validates before it seals — the rows are in hand
   and `at.head` is already read, so a refusal costs **zero write-class requests** and cannot
   orphan a segment. But a contradiction here **does not fail the fold**: the offending rows
   are excluded from the segment, `Head.schema_rejects` records `(index, count)`, the
   watermark advances, and every other index folds normally. ⚠️ Those rows were acknowledged
   `durable` and are now **discarded** — that is a real cost, stated plainly. It is the lesser
   of two, and the greater one was measured: the alternative wedges the tenant permanently.
   The bundle objects still hold the rows until GC reaps them, and the count is reported by
   the API, so a discard is visible rather than silent.
- A fold that finds **no** schema for an index records one. That is the only way a schema is
  created.
- ⚠️ **The text field is compared only when the fold's rows actually carry it.** `seal` builds
  a text index only for rows with a non-empty value in the field, so an index of vectors with
  no text records whatever the folding process was configured with and must not then refuse a
  process configured differently — a conflict on a field neither segment has postings for.
- ⚠️ **`compact` is explicitly out**: it derives its text field from its inputs and writes a
  segment without consulting the schema. Correct today, and named here so a later change that
  makes a merged segment contradict HEAD has to face this line.
- `Head::decode` gains an end-of-buffer check at the section boundary (`Cur::at_end`).
- `Engine::index_stats` reports the schema alongside the counts, so the API can.
- The server: `GET /v1/indexes/{id}` gains `schema`; a conflicting write is `400
  schema_conflict` **at the door** once this process has read HEAD once; `PATCH
  /v1/indexes/{id}/schema` is `400 schema_immutable` whose message names branch-based
  migration as the path, per `api-design.md`.

⚠️ **Accepted trade in the decoder, stated rather than discovered**: once end-of-buffer at the
schema boundary means "no schemas", a HEAD truncated *exactly* at the end of the graveyard
section decodes as valid instead of `CorruptHead`. That is the unavoidable price of an optional
trailing section, and the section is the only way to stay readable for every HEAD already
written.

**Does not add** branching itself, a `bm25`/`trigram`/`indexed` per-attribute declaration, type
inference for attributes, or a reindex. The schema recorded here is the two facts the engine
already depends on and currently cannot state: **the width, and the text field**. Everything
else in `api-design.md` §Schema needs attribute types, which needs a filter language the query
path does not yet expose — naming it here so it is a scope line rather than an omission.

⚠️ **`with_text_field` stays a process knob**, and that is now *safe* rather than merely
unchecked: a process configured differently from the index's recorded schema is refused at its
first fold instead of silently writing a segment whose text nobody can query.

## Acceptance criteria

1. The first fold of an index records `{dims, text_field}` in HEAD, read back by **decoding the
   committed object** — not by asking the engine that wrote it.
2. A fold whose rows contradict the recorded width **seals no segment for that index**, and
   issues **zero write-class requests for it**: asserted on `OpClass::Write` per fold, not on
   the epoch, which is local until the commit and therefore cannot show the difference.
3. ⚠️ **A contradiction does not stop the tenant.** ⚠️ And when **every** row in the set is
   dropped, the fold still commits an epoch carrying the advanced watermarks and the reject
   counts — `fold`'s existing "nothing to do" early return would otherwise leave the count
   uncommitted and the watermark where it was, which is the one case where "visible rather
   than silent" would quietly fail. With one wrong-width row durable in a lane
   bundle: the fold **succeeds**, the watermark advances, a second index in the *same bundle*
   folds and is queryable, `Head.schema_rejects` counts the dropped rows, and a **subsequent**
   fold of correct rows commits normally. Asserted as a sequence, because the failure it
   replaces is "every later fold, forever".
4. `Engine::flush` refuses a wrong-width batch **before it writes the bundle** — zero
   write-class requests — and the schema read that makes this possible happens **once per
   process**: two flushes of an index, one read of HEAD attributable to the schema.
5. `Engine::write` refuses a wrong-width batch at **zero requests** once this process has read
   HEAD, and still refuses a self-inconsistent batch when it never has.
6. ⚠️ **BACKLOG row 27 is narrowed, not closed, and the ledger says which part.** A
   **write-only process that has never read HEAD** still accepts a wrong width at the door; it
   is refused at the flush (criterion 4), so nothing wrong becomes durable. The residue is that
   the refusal is at flush rather than at write — one API call later than ideal — and the row
   stays open with that scope.
7. A process whose `with_text_field` differs from the recorded one is refused when its rows
   **carry text**, and **not refused** when they do not — both halves asserted, because a
   conflict on a field with no postings is a false refusal on the path that wedges hardest.
8. **A HEAD written by the old encoder decodes**, with no schemas and no error, and a fold over
   it records the schema rather than refusing — asserted against a byte string built without
   the new section, so it cannot pass by round-tripping the new encoder.
9. Through the API: `GET /v1/indexes/{id}` reports `schema.dims`, `schema.text_field` and
   `rejected_rows`; a wrong-width write after a fold is `400 schema_conflict`; and
   `EngineError::SchemaConflict` has an **arm in the error table** rather than falling through
   to `500 internal`, which is the exact defect M7c's typed `DimensionMismatch` was added to fix.
10. `PATCH /v1/indexes/{id}/schema` is `400 schema_immutable` and its message names the
    migration path — not a `404`, because the route existing and refusing is the answer.
11. The whole existing suite is green: every existing fixture folds against a HEAD with no
    schema, so criterion 1 must not make a first fold a refusal.
12. Region coverage ≥95% on the changed crates; every mutant in the new code caught or named
    equivalent with the reason on the line.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_fold_records_the_schema_it_inferred` | a schema written to the in-memory HEAD and not to the committed bytes; `dims` read from anywhere but the rows |
| 2 | `a_contradicting_index_seals_nothing` | the check placed after `seal`, which orphans an object no HEAD names — killable only by the request-class counter, which is why the criterion names it |
| 3 | `a_wrong_width_row_does_not_stop_the_tenant` | the first draft's behaviour: a fold that returns `SchemaConflict` and advances nothing, which bricks every later fold for that tenant; and a drop that forgets to count, which makes the discard silent |
| 4 | `a_flush_refuses_before_the_bundle_is_written`, `the_schema_is_read_once_per_process` | a flush that writes first and validates after — the row is durable and the tenant is one fold away from the wedge; and a schema read per flush, which is a request per write |
| 5 | `the_write_door_uses_the_schema_the_process_has_read`, `a_cold_process_still_refuses_a_self_inconsistent_batch` | a door check that reads HEAD; one that passes silently when the cache is cold |
| 7 | `a_text_field_conflict_needs_text`, `a_text_field_that_contradicts_the_schema_is_refused` | comparing the field on rows that have none — a false refusal on an index of pure vectors |
| 8 | `a_head_without_a_schema_section_decodes_as_no_schemas` | end-of-buffer read as a decode error, which makes every HEAD written before this milestone unreadable; absence read as `dims: 0`, which refuses every later fold |
| 9,10 | `the_api_reports_the_schema_and_refuses_a_width_change`, `patching_a_schema_is_refused_with_the_migration_path` | a missing error-table arm, so a client-caused conflict answers `500`; a `PATCH` that 404s, telling a client the resource does not exist rather than that the operation is not permitted |
| 11 | the existing workspace suite | a first fold that refuses because no schema is recorded yet |

⚠️ **Criterion 8 breaks a deployment rather than a test.** Every HEAD in every existing store
was written without this section. ⚠️ **Criterion 3 is the one this spec exists in its second
draft for.**

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `write` with a warm schema cache | 0 | **0** | 0 | 0 |
| `fold` | unchanged — the validation reads the HEAD it already read | unchanged | 0 | 0 |
| `GET /v1/indexes/{id}` | 0 | **1**, unchanged — the schema rides in the HEAD it already reads | 0 | 0 |
| `PATCH schema` | **0** — it refuses | 0 | 0 | 0 |
| A query | unchanged | unchanged (4, M7c) | — | 0 |

⚠️ **HEAD grows by one entry per index**, roughly the name again plus eight bytes. M6i measured
an open at ~106 bytes per index and [M7b](../M7b/VERIFIED.md) priced that against a round trip:
the crossover was ~29,677 indexes against a ceiling of 50. This adds to the same term and the
same argument covers it, which is what a measured trade is *for*.

## Risks

- **Every existing store has a HEAD without the section.** Criterion 8 is the defence and it is
  asserted against bytes, not against a round trip.
- **The cached schema can be stale**: another process may have folded since. A stale *width* is
  still refused correctly at the fold, which is the authority; the door check is an early
  refusal, never the last word. Stated so nobody reads the cache as a source of truth.
- ⚠️ **Rows acknowledged `durable` can be discarded at the fold.** That is the price of not
  wedging, and it is the gravest thing in this milestone. Three things bound it: the door and
  the flush refuse every case a single correctly behaving writer can produce, so reaching the
  fold needs a **race between two cold processes writing different widths**; the count is in
  HEAD and reported by the API, so it is visible; and the bundle objects still hold the rows
  until GC reaps them, so a discard is recoverable by hand for as long as retention lasts.
- **The cached schema can be stale** — another process may have folded since. A stale width is
  still refused at the flush against a schema read at this process's first flush, and the fold
  is the final authority. The cache is an early refusal, never a source of truth.
- **`dims: u32` assumes one dense field per index**, which is what the segment layout stores
  today (M3b's `Fields` table notwithstanding). A second field needs a schema per field; the
  name `IndexSchema` is chosen so that growth is an added field rather than a rename.

## Tasks

| Id | Commit |
|---|---|
| M7d.1 | `IndexSchema` and `schema_rejects` in HEAD, encoded, decoded, absent-section-safe |
| M7d.2 | The fold records, validates before sealing, and **drops rather than stops** |
| M7d.3 | The flush refuses, reading the schema once per process; the door uses the cache |
| M7d.4 | The API: the schema, the reject count, the error-table arm, the refused `PATCH` |
| M7d.5 | The ledger, the backlog rows, and the roadmap |
