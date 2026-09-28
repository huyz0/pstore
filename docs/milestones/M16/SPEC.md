# M16 — `branch_from_namespace` and `copy_from_namespace`

## Serves

- OQ-33, branch refcounting under deep branch trees, which this milestone **answers**.
- [`mutations-and-mvcc.md`](../../research/05-storage-engine/mutations-and-mvcc.md) §
  Branching: "one PUT, zero bytes copied".
- The parity row for `copy_from_namespace` and `branch_from_namespace`.

## What the corpus did not know

The corpus designed a branch as a new HEAD per index, and therefore needed refcounting across
HEADs. It offered lineage objects, or a mark-sweep. In this code **one HEAD per tenant names
every index**, and GC already refuses to reap any key HEAD still names (`live`, M6). So a
segment shared by any number of indexes, at any branch depth, is reclaimed exactly when the
last index naming it buries it.
- **OQ-33 needs no mechanism.** "Named by HEAD" is an exact reference count.
- What the corpus missed instead is **delete vectors**. HEAD keys them by segment, so two
  indexes sharing a segment would share their deletes.

## Delta

- **Wire.** A write to index `dest` may carry `{"branch_from_namespace": "src"}`. Two refusals
  apply, each a `400`:
  - it is alone in the request: no other operation;
  - `dest` must not exist, and `src` must.

  `copy_from_namespace` is the same operation, because a physical copy of immutable segments
  buys nothing. The difference from turbopuffer is stated in the parity doc.
- **What a branch holds.** `src`'s folded state at the branch's commit.
  - The server folds the tenant first, so every durable write acknowledged before the request
    is in it.
  - A batched write not yet flushed is not, and neither is anything written after.
  - `dest` takes `src`'s schema whole: dims, metric, analyzer, `k1`, `b`.
- **The commit.** One HEAD CAS that:
  - adds `dest`'s segment refs, the same keys as `src`'s;
  - adds `dest`'s schema;
  - records `borrowed[dest]`, the set of those keys;
  - records `branched[dest] = epoch`;
  - retries on contention against a fresh HEAD.

  For each of `src`'s segments carrying a delete vector, it reads that vector (1 GET) and
  writes a copy under `dest`'s scoped key (1 PUT) before the commit.
- **Scoped delete vectors.** For an index and a segment it borrows, the delete-vector entry's
  key is the segment key followed by `.br-` and the index name in hex. It never contains a dot,
  so `dv_of` still parses the object key. An index uses the scoped key exactly when the segment
  is in its `borrowed` set, and the plain key otherwise.
  - That lookup, `dv_ref(head, index, segment)`, replaces `deletes.get(key)` at every site: a
    query's targets, the fold's `prepare` and `supersede`, compaction, `index_stats`, the count
    fast path, and `as_of`.
  - Deletes in `dest` never touch `src`, and the other way round.
- **Leaving.** When a branch compacts, drops, or otherwise buries a borrowed segment, it
  leaves `borrowed`, and the segment's key goes to the graveyard as any other.
  - GC reaps it only when no index names it.
  - A scoped delete vector is buried like any other delete vector.
- **HEAD** gains two trailing sections, `borrowed` and `branched`, with M9d's mechanism. A HEAD
  without branches is byte-for-byte what M15 wrote.
- **`as_of` below `branched[dest]`** answers `dest` as not existing then (`404`). The
  reconstruction would otherwise show the parent's segments, and only those still alive at
  the branch.

**Does not change:** any other operation's requests. A query of `dest` costs exactly what the
same query of `src` costs.

## Acceptance criteria

1. **Equal at birth.** After a branch, every query of `dest` equals the same query of `src`:
   relevance, ordered, filtered, aggregated, including rows `src` had deleted.
2. **Independent after.** Upserts, deletes, patches and by-filter operations in either index
   change only that index. That includes a delete of a row in a shared segment, both ways.
3. **GC-safe.** In each case below `dest` answers as before, and every object it names is
   still in the store:
   - `src` is compacted, then `gc(0)`;
   - `src` is dropped, then `gc(0)`.

   Then `dest` is dropped, `gc(0)` runs, and the shared segments are gone from the store.
4. **Deep.** A branch of a branch of a branch, with deletes at each level, satisfies 1–3.
5. **Cost.** A branch of an index with `s` segments, `d` of them carrying deletes, costs:
   - one fold;
   - one HEAD read;
   - `d` GETs and `d` PUTs;
   - one HEAD CAS.

   That is independent of row count and asserted by the request counter.
6. **History.** `as_of` below the branch epoch is `404` for `dest`, and unchanged for `src`.
7. **Refusals,** each `400` naming the rule:
   - `dest` exists;
   - `src` does not;
   - the branch with another operation;
   - `src == dest`.
8. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the field is refused | a delete vector not copied; the schema not copied |
| 2 | as 1 | `dv_ref` returning the plain key for a borrowed segment, so deletes leak between indexes |
| 3 | as 1 | GC's `live` set missing a borrowed segment or a scoped vector; a scoped vector not buried on drop |
| 4 | as 1 | `borrowed` not carried from a borrowing source |
| 5 | as 1 | a per-segment GET added; the fold skipped |
| 6 | as 1 | `branched` not written, or not read |
| 7 | as 1 | a refusal accepted |

## RA budget

A branch costs 1 fold, 1 HEAD read, `2d` object requests and 1 CAS. It scales with segments
carrying deletes, which compaction bounds, never with rows. Everything else is unchanged.

## Risks

- **A branch keeps a large parent's segments alive** after the parent drops them. That is
  correct and costs storage. Nothing reports the bytes a branch retains, which is the
  corpus's "surface it" hazard and stays open.
- **HEAD grows** by one entry per borrowed segment per branch. That is bounded by segment
  count, but a tenant with many branches of a many-segment index carries it on every HEAD
  read.
- **Mixed versions:** a pre-M16 process rewriting HEAD drops `borrowed`, and its folds then
  write the branch's deletes into the shared vector. An upgrade must reach every process
  before any branch exists, and no gate enforces it.

## Tasks

- **M16.1** — scoped delete vectors, `borrowed` and `branched` in HEAD, `Engine::branch`, and
  the wire.
