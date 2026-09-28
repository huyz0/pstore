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

**Deferred operations are durable only** (spec review, M4). A request carrying one with
`durability: "batched"` is `400`.
- A batched one would sit in this process's memory, where no fold reads it and no probe finds
  it.
- So a `strong` read here could be served without it.

**Semantics at the fold**, each operation against the id's current version in fold order:

| Operation | Current version exists | No current version |
|---|---|---|
| patch | the version, with the named attributes set or removed | ignored |
| upsert with condition | applied if the condition admits the version | applied |
| patch with condition | applied if the condition admits the version | ignored |
| delete with condition | applied if the condition admits the version | nothing to delete |

- A skipped conditional operation, or a patch of nothing, **touches nothing**: the id's
  segment row stays and its delete vector is unchanged.
- **Patches and conditional deletes are exempt from the reject pass and from schema
  inference**, as tombstones are (spec review, M1).
  - A **conditional upsert is not exempt** (spec review round 2): it is a full row, with its
    own vector and `$metric`, and is checked and inferred from as any upsert is.
  - A patch carries no vector and no `$metric`, so today's pass would drop every one.
  - A merged row takes its vector (dense and sparse) from the base version, already stored
    under the index's metric, and it is sealed without transforming it again.
  - A patch can change neither width nor metric. The text-field check depends on the
    folding process's configuration (spec review, M3), so it is not applied to a merged
    row, and a non-string value in the text field is indexed as no text, as a write's is.
  - So a merged row is never rejected.
- The response's `documents_patched` counts ids requested, as `documents_deleted` does.

**Representation.** A deferred operation is a row in the lane bundle, carrying reserved
attributes that the write door refuses from clients, as a tombstone does (M9c.2):
- `$op`: the kind;
- `$cond`: a condition, as its canonical JSON.

A bundle written before M13 holds none, and reads as it did.

**The fold.**
- For each index with a deferred operation in the span, the fold first reads the base
  versions it needs, with `Segment::scan` over every existing segment of the index.
  - `scan` returns ids, attributes and vectors, dense and sparse, in one coalesced round per
    segment, all segments in parallel (spec review, B1: blocks alone carry no vectors).
  - Delete vectors are respected, as a query respects them.
  - Only the rows a deferred operation names, or that a by-filter operation admits, are
    kept.
  - ⚠️ `scan` is called with **no filter**, and that is load-bearing (spec review round 2).
    It returns rows without their positions, so list order is row position only when every
    block is read. `supersede` needs positions for the delete vectors. A filtered `scan`
    here would silently shift them.
  - For that index, `supersede` takes its ids from this same pass instead of its own read. So
    the cost is **the index's bytes, once per such fold**, not twice.
  - Like everything in the fold, the base read is repeated on every commit attempt, against
    the HEAD that attempt read, and never cached across attempts (spec review, m1).
- It then applies every operation in order, over a map from id to this fold's version of it.
  A delete leaves an entry meaning "no version": a patch after it in the same fold is
  ignored rather than read from the base, which would bring the row back (spec review, M2).
- Every id whose version changed is touched: superseded in its segment, and, if it still
  exists, sealed.
- Where no deferred operation is present, the fold is unchanged: no extra read.

**Reads before the fold.**
- The fresh view ignores deferred operations. They neither shadow a segment row nor add one.
  An upsert followed by a patch of the same id shows the upsert until the fold.
- The own-lane shortcuts treat an unfolded bundle holding a deferred operation as **not** in
  memory. That covers `settled` (M9i.2, `strong`), `covers` and `unfolded_next` (M11.1,
  `session`).
- While this process holds any unfolded durable batch carrying a deferred operation:
  - `strong` probes its own lane at the watermark, so it finds that bundle and refuses;
  - a `session` entry for this lane is covered only by the watermark;
  - `unfolded_next` names the lane.

  Deferred operations are durable only, so every one is in such a batch.

⚠️ **Deterministic within a fold, not across fold boundaries** (spec review, m2). As M9c.2
states for upserts, operations are ordered lane then sequence within a fold, and folds in
commit order. So a patch on a lower lane, causally after an upsert on a higher lane, is
overwritten by it if both land in one fold. Stated, not hidden.

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
2. **Order.** In one fold, in lane order:
   - upsert, then patch, then patch the same id again: the two patches compose;
   - a patch before an upsert of the same id is overwritten by it;
   - a delete, then a patch of the same id: the row stays deleted (spec review, M2).
3. **Conditions.**
   - Each conditional kind, admitted and refused, over an existing and a missing id, gives the
     table's result after a fold.
   - A refused operation leaves the segment's delete vector untouched.
4. **Deferred visibility.**
   - Before the fold, `eventual`, and every read of the writing process, answer the
     pre-patch row.
   - A `session` read with the patch's token is refused, on this process and on another.
   - A `strong` read through this process is refused too.
5. **Vectors and schema.**
   - A conditional upsert of the wrong width is dropped and counted in `rejected_rows`, as
     an unconditional one is (spec review round 2).
   - A patched row keeps its dense vector under cosine and euclidean indexes, not transformed
     again: a query for it finds it at the same distance as before the patch.
   - It keeps its sparse vector too.
   - Neither patches nor conditions add to `rejected_rows` (spec review, M1).
6. **Cost.**
   - A write carrying patches costs exactly 1 PUT, as any write does.
   - A fold with no deferred operation issues exactly the reads it issued before M13.
7. **Refusals.** Each of these is `400`:
   - a patch with `vector`;
   - `patch_columns` of unequal lengths;
   - a condition that is not a filter;
   - a condition with no operations of its kind;
   - a deferred operation with `durability: "batched"`.

   (`$op` and `$cond` need nothing new: names beginning `$` are already refused, spec
   review m3.)
8. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

### Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `patch_rows` refused | a patch replacing rather than merging; `null` kept |
| 2 | as 1 | operations applied out of fold order |
| 3 | as 1 | a condition ignored; a skipped operation touching the id |
| 4 | as 1 | a deferred operation shown or shadowing before the fold; an own-lane shortcut trusting it |
| 5 | as 1 | a stored vector transformed twice; a sparse vector dropped |
| 6 | as 1 | a base read taken with no deferred operation |

## M13.2 — `delete_by_filter` and `patch_by_filter`

### Delta

- **Wire.** `delete_by_filter: <filter>` and `patch_by_filter: {"filters": .., "attributes":
  {..}}`. Each is one deferred operation, after the request's other operations.
- **At the fold**, the operation applies to every row whose current version the filter
  admits. That includes:
  - folded rows, from the base read;
  - rows this fold's earlier operations produced.

  The base read is the same `scan` of every segment: it already reads the whole index, so a
  by-filter operation adds no read. It keeps every row a by-filter predicate admits.
- The response counts nothing it cannot know: `documents_deleted` and `documents_patched`
  stay the per-id request counts.

### Acceptance criteria

1. `delete_by_filter` over 2,000 folded rows deletes exactly the rows brute force says, and
   none else. So does `patch_by_filter`, with merged attributes.
2. A row an earlier operation in the same fold made match is affected; one it made stop
   matching is not.
3. Before the fold, reads are unaffected, as in M13.1 criterion 4.
4. Refusals: a by-filter operation whose filter or attributes are malformed is `400`, as is
   one with `durability: "batched"`.
5. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

(Spec review, M5: a criterion that the fold reads only the blocks a filter's zone maps admit
is gone. `supersede` reads every block of the index anyway, so there is nothing to save.)

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
- While this process holds an unfolded deferred operation, `strong` is refused for **every**
  index on it until a fold, because a lane probe cannot tell which index a bundle touches.
  That is an availability cost, accepted and stated (spec review round 2).

## Tasks

- **M13.1** — per-id patches and conditions; criteria 1–8.
- **M13.2** — by-filter operations; criteria 1–5.
