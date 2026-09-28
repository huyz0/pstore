# M13 — Patches and conditional writes

**Serves:** the `patch_rows` / `patch_columns`, `patch_by_filter`, `delete_by_filter` and
`*_condition` rows of
[`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md). They were
deferred as "a read-modify-write at fold time, priced after M9c". This spec prices them.

⚠️ **Split before starting**, as M9i and M11 were:
- **M13.1** — per-id operations: `patch_rows`, `patch_columns`, and `upsert_condition`,
  `patch_condition` and `delete_condition`.
- **M13.2** — `delete_by_filter` and `patch_by_filter`, which touch rows no request names.
  Specified here, delivered second.

## Where the read happens, and why it is the fold

A patch, a condition and a by-filter operation each need the row's **current** version.
There are two places to read it.

**At the write.**
- Each request would read HEAD, open every segment and fetch the rows it names: a
  data-dependent chain of reads on the write path, which costs 1 W today (D-34).
- Two writers could read the same version and each write a merge of it, so one patch would be
  lost. Nothing but a lock could order them, and AGENTS.md forbids a lock.

**At the fold.** The fold is already the serialization point: it commits by CAS, it resolves
the newest operation per id in lane-then-sequence order (M9c.2), and it already reads the ids
of every existing segment of each index it folds (`supersede`).

**So these operations are deferred to the fold.**
- **Deterministic.** Each is applied against the version the fold's order puts before it.
- **Costless on the write path.** A write still costs exactly its one bundle PUT.
- **Visible after the fold.** Until the fold that applies a deferred operation, every read
  (this process's included) answers as if it had not been made. The rest of this spec
  follows from that.

## M13.1 — per-id operations

### Delta

**Wire.** A write request may carry, beside `documents` and `deletes`:
- `patch_rows: [{"id": .., "attributes": {..}}]`, or its columnar form
  `patch_columns: {"id": [..], "<attr>": [..]}`. A patch sets the attributes it names; an
  attribute set to `null` is removed. A patch carries no vector, so `vector` is refused.
- `upsert_condition`, `patch_condition` and `delete_condition`: each is a filter in
  `filters`' syntax, applying to that request's `documents`, patches or `deletes`.

Order within a request: `documents`, then patches, then `deletes`. A later operation on an id
wins, as in M9c.2. A request with no operation at all is refused, as today.

**Semantics at the fold**, each operation against the id's current version in fold order:

| Operation | Current version exists | No current version |
|---|---|---|
| patch | the version, with the named attributes set or removed | ignored |
| upsert with condition | applied if the condition admits the version | applied |
| patch with condition | applied if the condition admits the version | ignored |
| delete with condition | applied if the condition admits the version | nothing to delete |

- A skipped conditional operation, or a patch of nothing, **touches nothing**: the id's
  segment row stays and its delete vector is unchanged.
- A patched result is checked against the index's schema, as any row is. A result that
  contradicts it is dropped and counted in `rejected_rows`, and the old version stands
  (M7d's rung three).
- The response's `documents_patched` counts ids requested, as `documents_deleted` does.

**Representation.** A deferred operation is a row in the lane bundle, carrying reserved
attributes that the write door refuses from clients, as a tombstone does (M9c.2):
- `$op`: the kind;
- `$cond`: a condition, as its canonical JSON.

A bundle written before M13 holds none, and reads as it did.

**The fold.**
- For each index with a deferred operation in the span, the fold first reads the base
  versions it needs: every live row of the index's existing segments whose id a deferred
  operation names.
  - Delete vectors and the rejected are respected, as a query respects them.
  - The read is one pass over the index's blocks, in parallel per segment: **the index's
    bytes, once per such fold**. Ids cannot be pruned by a zone map, which is `supersede`'s
    price too.
- It then applies every operation in order, over a map of the versions this fold has
  produced.
- Every id whose version changed is touched: superseded in its segment, and, if it still
  exists, sealed.
- Where no deferred operation is present, the fold is unchanged: no extra read.

**Reads before the fold.**
- The fresh view ignores deferred operations. They neither shadow a segment row nor add one.
  An upsert followed by a patch of the same id shows the upsert until the fold.
- The own-lane shortcuts treat an unfolded bundle holding a deferred operation as **not** in
  memory. That covers `settled` (M9i.2, `strong`), `covers` and `unfolded_next` (M11.1,
  `session`).
- The engine remembers the next sequence past its last such bundle. Until HEAD's watermark
  reaches it:
  - `strong` probes its own lane at the watermark;
  - a `session` entry for this lane is covered only by the watermark.

**Does not change:** a write's cost, any read's cost, a fold without deferred operations,
or the segment format.

### Acceptance criteria

1. **Patch.**
   - A patch of a folded row sets and removes attributes. After a fold:
     - the row has the merged attributes and its original vector;
     - a filter on the new value finds it, and one on the old value does not;
     - the count is unchanged.
   - A patch of a missing id changes nothing.
   - `patch_columns` equals `patch_rows`.
2. **Order.** In one fold, in lane order: upsert, then patch, then patch the same id again.
   The two patches compose. A patch before an upsert of the same id is overwritten by it.
3. **Conditions.**
   - Each conditional kind, admitted and refused, over an existing and a missing id, gives the
     table's result after a fold.
   - A refused operation leaves the segment's delete vector untouched.
4. **Deferred visibility.**
   - Before the fold, `eventual`, and every read of the writing process, answer the
     pre-patch row.
   - A `session` read with the patch's token is refused, on this process and on another.
   - A `strong` read through this process is refused too.
5. **Schema.** A patch that would contradict the schema is dropped, counted in
   `rejected_rows`, and the old version stands.
6. **Cost.**
   - A write carrying patches costs exactly 1 PUT, as any write does.
   - A fold with no deferred operation issues exactly the reads it issued before M13.
7. **Refusals.** Each of these is `400`:
   - a patch with `vector`;
   - `patch_columns` of unequal lengths;
   - a condition that is not a filter;
   - a condition with no operations of its kind;
   - a client attribute named `$op` or `$cond`.
8. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

### Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `patch_rows` refused | a patch replacing rather than merging; `null` kept |
| 2 | as 1 | operations applied out of fold order |
| 3 | as 1 | a condition ignored; a skipped operation touching the id |
| 4 | as 1 | a deferred operation shown or shadowing before the fold; an own-lane shortcut trusting it |
| 5 | as 1 | the schema check skipped on a merged row |
| 6 | as 1 | a base read taken with no deferred operation |

## M13.2 — `delete_by_filter` and `patch_by_filter`

### Delta

- **Wire.** `delete_by_filter: <filter>` and `patch_by_filter: {"filters": .., "attributes":
  {..}}`. Each is one deferred operation, after the request's other operations.
- **At the fold**, the operation applies to every row whose current version the filter
  admits. That includes:
  - folded rows, from the base read;
  - rows this fold's earlier operations produced.

  The base read for an index with a by-filter operation also takes every live row that
  predicate admits, pruned by zone maps as a query is. So the base is what the filters admit,
  plus the ids named.
- The response counts nothing it cannot know: `documents_deleted` and `documents_patched`
  stay the per-id request counts.

### Acceptance criteria

1. `delete_by_filter` over 2,000 folded rows deletes exactly the rows brute force says, and
   none else. So does `patch_by_filter`, with merged attributes.
2. A row an earlier operation in the same fold made match is affected; one it made stop
   matching is not.
3. Before the fold, reads are unaffected, as in M13.1 criterion 4.
4. A by-filter operation's fold reads only blocks its filter's zone maps admit, plus the
   named ids' segments.
5. Refusals: a by-filter operation whose filter or attributes are malformed is `400`.
6. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

## RA budget

- **Write:** unchanged, 1 PUT.
- **Query:** unchanged.
- **Fold** with a deferred operation: one parallel pass over the index's live blocks that the
  named ids or the filters need, bounded by the index's bytes. A fold without one is
  unchanged.

## Risks

- A fold with a deferred operation holds the base rows it needs in memory: the named ids'
  rows, plus every row a by-filter operation admits. A by-filter operation over a huge match
  set is that many rows.
- Deferred operations are invisible until a fold, so a client must fold, or wait for the
  scheduled one (M9i.1), to see its own patch. That is stated, and `session` refuses rather
  than serves stale.

## Tasks

- **M13.1** — per-id patches and conditions; criteria 1–8.
- **M13.2** — by-filter operations; criteria 1–6.
