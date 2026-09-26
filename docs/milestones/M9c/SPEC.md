# M9c — Upsert and delete by id, on a memtable that no longer loses or doubles rows

**Serves:** Q18 / [`mutations-and-mvcc.md`](../../research/05-storage-engine/mutations-and-mvcc.md)
(an update is an insert with the same id; deletes as delete vectors) and D-34. Third of M9.
⚠️ **Split at spec review** into two tasks: upsert semantics turn the memtable's known races
(BACKLOG rows 35–38) from hidden rows into *deleted or overwritten rows served back*, so the
races are fixed first, as M9c.1.

## Delta — M9c.1: the memtable's races (rows 35, 36, 37, 38)

- **Row 35.** A flush **snapshots** `pending` and moves the rows only after the bundle PUT
  succeeds (the first `n` of each index: flushes are serialized, writes only append). A query
  during the PUT still sees them; a failed PUT or a schema refusal leaves them untouched, so
  `restore` is deleted.
- **Rows 36 and 38.** `durable` becomes batches tagged with their bundle's lane sequence. A
  batch is dropped when a HEAD's watermark for this lane is past its sequence — at this
  engine's own fold commit (only what that fold folded, not "everything"), and whenever a query
  or scan reads a HEAD that shows another process folded it.
- **The reverse race.** `pruned` records the highest watermark pruned to; a query holding an
  older HEAD would pair it with rows missing what it lacks, so it re-reads HEAD (at most 4
  times, then `Lost`).
- **Row 37.** The fresh view (target, rows, store) is returned from under its lock; the query no
  longer re-locks it, so another index's query cannot swap it in between.

**Does not change:** what a flush writes, what a fold folds, the bundle or HEAD format, or any
query's requests; only *which in-memory rows* a query pairs with the HEAD it read.

## Delta — M9c.2: upsert and delete (revised at spec review)

- **Wire.** A write is an upsert; `deletes: [id]` joins `documents`, either may be empty but
  not both; a later duplicate in a request wins; `deletes` apply after `documents`.
  `documents_deleted` counts ids **requested**, not ids found (that would need a read).
- **Order.** The newest operation on an id decides it: within a process, arrival; within a
  fold, lane then sequence; across folds, fold order. ⚠️ So a process can see its own write,
  then lose it at a fold to a lower-sequence write of a higher lane — stated, not hidden.
- **Tombstones** are operations with the reserved empty-name attribute (`Engine::write` refuses
  it). Exempt from every width and schema check; the fold's order is **reject pass → newest per
  id → drop tombstones → infer schema**, so a rejected newest operation leaves the older
  version standing.
- **Fold** reads the ids of each existing segment of each folded index (+1 Rpar each, and its
  current delete vector), and writes affected segments a cumulative delete vector at
  `{segment key}.{epoch:020}-{lane:016x}.dv` — **with the lane**, like every key two folders
  could both write (review B2). HEAD's trailing section maps segment → (vector key, count).
  `key_index` reads only keys ending `.seg`; `gc` treats HEAD's vector keys as live.
- **Query.** Vectors ride the open round. **Shadowing** (ids with unfolded operations) is checked
  on the **legs' candidate ids** in the id round the query already has — not by reading every
  block (review M3) — so depth and block reads are unchanged. Each leg asks for `limit +
  |deleted| + |shadow|`; a clustered dense leg also scales `p` by `rows / live rows` (review M4).
- **Compaction** snapshots its inputs' vectors and abandons if any changed before its commit
  (review B1), drops deleted rows, and buries the inputs' vectors.
- **Rollout.** Tombstones and the HEAD section are unreadable to pre-M9c code; no fleet may mix
  versions across it. There is no deployed data.

## Acceptance criteria

1. (M9c.1) A query during a flush's bundle PUT sees the flushing rows.
2. (M9c.1) A flush landing between a fold's lane read and its commit stays visible after it.
3. (M9c.1) Rows another engine folded are returned once; later rows still served.
4. (M9c.1) A prune drops exactly the folded batches; a HEAD behind a prune is reported stale; a
   refused flush leaves its rows pending.
5. (M9c.2) The newest version of an id wins, unfolded or folded, and a deleted id is absent.
6. (M9c.2) A fold racing a compaction resurrects nothing; `as_of` sees the version current then.
7. (M9c.2) 90% superseded, a `top_k` query still returns `top_k`, on exact and clustered segments.
8. `./scripts/gates.sh` passes; `./scripts/mutants.sh` over each task's diff misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | old flush (take, then put back) | rows moved before the PUT lands |
| 2 | old fold (`durable.clear()`) | pruning past the committed watermark |
| 3 | old engine (never prunes on read) | another process's fold ignored |
| 4 | no `prune` | an off-by-one on the watermark; a stale HEAD accepted |
| 5–7 | M9c.2's own tests, written then | — |

## RA budget

M9c.1: unchanged, except a query or scan that meets a stale HEAD re-reads it (+1 Rseq, rare).
M9c.2: as above — fold +1 Rpar per existing segment, +1 W per changed vector; query +1 Rpar per
segment with a vector, in the open round.

## Risks

- Row 37 cannot be forced deterministically; its fix is structural (no re-lock), not tested.
- M9c.2's fold reads every existing segment's data blocks (whole rows, not only ids -- code
  review) on each fold that touches the index, inserts included: bytes grow with the index until
  compaction. Revealed by the fold's own `cost`; a per-segment id filter is the fix if it bites.
- A fold attempt that loses its CAS leaves the vectors it wrote unburied, for M6e's orphan
  sweeper, as it already did its segment (code review, minor).

## Tasks

- **M9c.1** — the memtable races, their tests, this spec.
- **M9c.2** — upsert and delete, per the revised delta.
