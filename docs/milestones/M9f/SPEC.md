# M9f — Index lifecycle: delete an index, list them a page at a time

**Serves:** the namespace rows of
[`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md):
`DELETE` an index, a paginated list, and the metadata a client plans by. Sixth of M9.
⚠️ Revised at spec review (block): the drop holds this lane's flush lock, because an in-flight
flush otherwise loses acknowledged writes; and a dropped schema is kept in HEAD until GC reaps
the segments, because `as_of` from before the drop needs the old metric.
⚠️ **Split after round 2** (block on a write-only process's stale schema cache), per the
review skill's rule: **M9f.1** is the list, the metadata and the list's prune (criteria 6-8),
none of which round 2 faulted; **M9f.2** is the delete (criteria 1-5), amended below with
round 2's findings, and reviewed again as its own task.

## Delta

**Delete.** `DELETE /v1/indexes/{index}` removes the index. It answers `200` with the epoch
that removed it, or `404` if the index does not exist. It exists if any of these holds:
- HEAD names it in `indexes` or `schema_rejects` (a schema is only recorded beside a segment
  list, so `schemas` adds nothing -- amended at the mutation sweep);
- a bundle the drop read holds its rows;
- this process has pending rows for it.

Existence is decided again on every CAS retry, after reading the bundles and before anything is
sealed, so a `404` decided on the first attempt writes nothing (a retry after a lost CAS may
leave the first attempt's objects unnamed, as any fold's lost CAS does). Rows that are all
deletes do not make an index exist, as `ordered` and queries decide it. A `404` commits nothing, on any attempt. Of two
concurrent DELETEs, one answers `200` and the other `404`.

`Engine::delete_index` is **a fold that drops the index**, under this lane's `flushing` lock
from before it reads the lane tails until its commit:
0. It never calls `flush` (the lock is not reentrant) and holds no memtable guard across an
   `.await`.
1. It reads every lane's unfolded bundles to its tail, as `fold` does. It removes the dropped
   index's rows **before** the reject pass, so none are counted as rejects, and folds every
   other index as usual. It commits even when there is nothing else to fold, which `fold` does
   not (`fold` returns early when there are no bundles or no rows).
2. Every lane's watermark advances past the bundles it read, so no later fold brings those
   rows back.
3. In the same commit, HEAD loses the index's segment list, schema, reject count and its
   segments' delete vectors. Those segments and vectors are buried at this epoch, so GC reaps
   them after its window. The schema moves to a new trailing HEAD section, `dropped`: (index,
   epoch, schema) with the schema's dims, text field and metric, the last trailing section --
   removed when GC's horizon reaches that epoch (`horizon >= epoch`), and not before: `as_of`
   the epoch before the drop is answerable until then. Recorded only when HEAD had a schema
   for the index; the drop always writes `graveyard[epoch]`, possibly empty, so the GC that
   prunes it is never skipped for having nothing due.
4. On a successful commit, this process's `pending` rows of the index are dropped and the
   memtable generation bumps, so no cached fresh view keeps serving them. Because the lock is
   held, every durable batch of this lane has a sequence below the new watermark, and `prune`
   removes it.

⚠️ **The delete is linearized at its commit.** A write that lands in a bundle the drop did not
read is a write *after* the delete: flushed by another process after the drop read that lane's
tail, or still in another process's memory. Its fold creates the index again, holding only
that write. The new index's schema may differ from the old one: dims and metric are recorded
afresh. ⚠️ The write door refuses against a **cached** schema and a flush reads HEAD only once
per process, so a write-only process would refuse the new schema forever (round 2). So when
the door is about to refuse -- on the cached schema **or** on this process's unfolded rows,
whose durable batches a write-only process never prunes (M9f.2 review) -- it re-reads HEAD
once, remembers its schemas, prunes by its watermark, and re-checks both: a read on the refusal
path only, never per write, taken with the memtable lock **released** (it is a std mutex, and
held across an `.await` it would block).

**`as_of`** an epoch before the drop answers over the old segments and vectors. `Head::as_of`
already rebuilds them from the graveyard. It uses the **dropped** schema's metric: the entry
for that name whose epoch is the smallest one after the queried epoch; otherwise the present
schema. Before M9f, the present schema was used unconditionally. `query_as_of_filtered` is the
one reader of it: `ordered` never scores.

**Other processes.** `GET /v1/indexes` and `GET /v1/indexes/{index}` prune this process's
durable batches by the watermark of the HEAD they already read, as queries do. A process whose
rows another process folded, or dropped, stops reporting them without an extra request.

**List.** `GET /v1/indexes?prefix=&cursor=&page_size=` returns names in byte order:
- only names starting with `prefix`;
- only names strictly after `cursor` (an empty cursor is the start);
- at most `page_size` of them (default 100, 1 to 1000, anything else `400`).

It returns them in the existing `indexes` field, plus `next_cursor`: the last name returned
when more remain, `null` otherwise. The names are HEAD's plus this process's unfolded ones, so
a name held only in one process's memory can appear on one page and not on the next when
another process serves it. It is **one HEAD read**, never a LIST.

**Metadata.** `GET /v1/indexes/{index}` adds:
- `updated_epoch`: the newest key epoch among its segments and delete vectors, or `null` when
  it has no segment. That is the epoch of the last commit that rewrote them: a fold into it, a
  fold deleting from it, or a compaction.
- `approx_row_count`: `documents`, under turbopuffer's name.

**Does not change:** writes, queries without `as_of`, and the segment format. HEAD gains the
`dropped` section only.

## Acceptance criteria

1. After `DELETE`, the index is absent from queries (`404`), the list and `GET`, and every
   other index answers as before. That holds with its rows folded, flushed but unfolded
   (in this process and in another process's bundle), and pending here.
2. A bundle holding the index's rows that was flushed before the delete never brings it back.
   Rows written after the delete create it again, holding only those rows, even with a
   different metric and width. A second engine that had cached the old schema accepts the new
   width once it has read HEAD.
3. `as_of` an epoch before the delete of a `cosine_distance` index answers as it did then, with
   the old metric, even after the name is recreated as `dot_product`. After `gc` with retention
   0 run directly after the delete (so its horizon is the delete's epoch), the old
   segments, sidecars and delete vectors are absent from the store and the `dropped` entry is
   gone; with a retention leaving the horizon one epoch short of the drop, `as_of`
   the epoch before it still answers with the old metric.
4. `DELETE` of a missing index is `404`, and HEAD's epoch is unchanged. Of two deletes by two
   engines forced to read the same HEAD (an interference hook, as compaction's test has), with
   no pending rows of the index in either, one answers `200` and the other `404` on its retry.
5. A flush in flight during a delete loses no write made after the delete. This is forced with
   a held bundle PUT, as M9c.1's tests do: hold the PUT, spawn the delete, assert it has not
   completed while the PUT is held, release, and a write after the delete is served and
   survives a fold, and the held bundle's pre-delete row is absent after it. A write-only
   second engine that cached the old schema **and flushed old-width rows** before the delete
   accepts the recreated index's new width.
6. Another process that flushed rows of an index stops reporting them unfolded, on its next
   `GET`, once a fold elsewhere has folded them (M9f.1) or a delete has dropped them (M9f.2).
7. Listing 250 indexes with `page_size=100` returns three pages, no name repeated or missing,
   and `next_cursor` `null` on the last. A prefix narrows the result, and a `page_size` of 0 or
   1001 is `400`. One read, zero LISTs.
8. `updated_epoch` equals the epoch of the fold that last changed the index, and after a
   compaction the compaction's epoch (`null` if every row was deleted and it left no segment). It does not move when a fold changes another index, and
   it is `null` for an index with only unfolded rows.
9. `./scripts/gates.sh` passes; `./scripts/mutants.sh` over the diff misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | no route (`405`) | a HEAD section left behind; this process's rows kept |
| 2 | no `delete_index` | watermarks not advanced; the index dropped from `by_index` after the reject pass |
| 3 | no `dropped` section: the present (missing) metric | the dropped entry never pruned; the wrong entry chosen |
| 4 | no route | an empty commit for a missing index |
| 5 | no `delete_index` | the pending drop before the commit; the lock not held; only the cached-schema rung re-checked; no prune on the refusal re-read |
| 6 | the list reads `durable` unpruned | no prune on list |
| 7 | unpaginated | the cursor as `>=`; the prefix ignored; the bound off by one |
| 8 | field absent | the epoch taken from the whole HEAD |

## RA budget

Delete: a fold's cost, then one commit. List, metadata: one HEAD read. `as_of`: unchanged.
A write: unchanged, except a refused write, +1 GET (one sequential round) for its re-read.

## Risks

- The recreation window above.
- A delete folds every other index. That is a fold's cost, and it is what makes the drop final.

## Tasks

- **M9f.1** — the paginated list, the metadata, and the prune on list and summary (criteria
  6-8, 9).
- **M9f.2** — the delete (criteria 1-5, 9), after its own spec review.
