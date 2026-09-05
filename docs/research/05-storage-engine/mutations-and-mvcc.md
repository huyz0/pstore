# Deletes, Updates, MVCC, Branching

**Answers:** Q18
**Status:** Complete (v1)

## The rule

Segments are immutable. Therefore **nothing is ever modified — only superseded.**

## Updates

An update is an insert with the same document id. Resolution happens at read time:
- Documents are located by `doc_id → (segment, block, offset)`.
- When the same id appears in multiple segments, the **highest `(level_rank, epoch, lane_seq)`
  wins**, where newer levels/epochs are more recent.
- The loser is *garbage* and is dropped at the next compaction that spans both segments.

Partial updates (patch a single attribute) require a read-modify-write of the document at
fold time. Supported, but documented as more expensive than a full replace, because the
indexer must fetch the prior version.

> **Client-supplied versions.** For correct last-write-wins under concurrent lanes, clients
> may supply an optional monotonic `version` per document. Without it, ties break on
> `(commit_time, lane_id, seq)` — deterministic but arbitrary. **Recommendation:** document
> the no-version behaviour as "arbitrary but deterministic", and encourage `version` for
> workloads with concurrent writers to the same id.

## Deletes

Two mechanisms, for two different scales:

### 1. Tombstones (point deletes)
A delete is a WAL record like any other. Folded into segments as a tombstone entry. Costs one
lookup during merge.

### 2. Delete vectors (bulk / post-seal deletes)
When documents in an already-sealed segment are deleted, we cannot rewrite the segment. So we
keep a **delete vector**: a roaring bitmap of deleted row ids, per `(segment, epoch)`:

```
{h}/idx/{id}/s{shard}/dv/{segment_id}/{epoch:020}.dv
```

- Immutable per epoch; the manifest names the current one for each segment.
- Roaring bitmaps are compact (a few KB for millions of rows) and support fast AND with
  filter bitmaps — they compose directly with the filtering path.
- **This is the one sanctioned "sidecar"** (see `file-format-and-layout.md`), because it must
  change after the segment is sealed. It costs one extra small GET per segment per query,
  which is cached aggressively and is usually free.
- Delete vectors are folded away at compaction.
- Compaction trigger: rewrite a segment when its delete vector exceeds ~20% of rows.

This is the Delta Lake / Iceberg v2 "deletion vector" design, and it is the right one.

## MVCC and snapshots

MVCC falls out of the manifest design for free:
- An **epoch is a snapshot.** The manifest at epoch E names an immutable, complete set of
  objects.
- A query pins an epoch at start and reads only objects named by it. No locks, no read
  timestamps, no version chains.
- **Time travel**: `?as_of=epoch` or `?as_of=timestamp` (epoch↔time index kept in HEAD's
  recent history) is a full-fidelity feature costing nothing extra.
- **Retention** is a policy on how far back epochs (and their objects) are kept, default
  ~1 hour for correctness plus whatever the user buys for time travel.

## Branching (copy-on-write forks)

Creating a branch of an index is:
```
PUT {h}/idx/{new_id}/HEAD  If-None-Match: *   ← { epoch: E, manifest_ref: <same object> }
```
**One PUT. Zero bytes copied.** Both indexes now reference the same immutable segments; each
diverges by writing its own lanes and segments. Refcounting for GC becomes the only
complication.

turbopuffer lists "copy-on-write namespace branching" as a feature; for us it is not a
feature we build, it is a consequence of the architecture. Use cases: staging/prod forks,
per-PR test indexes, snapshot-and-experiment, cheap tenant cloning.

**Refcounting for GC.** With branches, "objects unreferenced by *this* HEAD" is no longer
sufficient. Options:
1. **Ref-by-epoch-lineage**: a branch records its parent `(index_id, epoch)`; GC on the parent
   must not reap objects below any child's fork point. Requires knowing the children →
   a small `{h}/idx/{id}/branches` object maintained by CAS at branch creation.
2. **Grace + mark-sweep**: periodic offline sweep computing the union of live object sets
   across the catalog.

> **Recommendation:** (1) for the common case (cheap, exact), with (2) as the weekly
> backstop. Branch creation is rare, so a CAS on the parent's branch list is affordable.

## TTL

Per-index or per-document TTL is a filter at read time plus a compaction trigger. TTL'd rows
enter the delete vector during the compaction that notices them; they are not eagerly deleted.
This keeps TTL from generating write traffic.

## Hazards

| Hazard | Mitigation |
|---|---|
| Query reads an epoch whose objects were GC'd | Epoch retention window ≫ max query duration; queries carry a deadline and fail fast if they exceed it. |
| Delete vector grows unbounded | Compaction trigger at ~20% garbage. |
| Branch keeps a huge parent alive forever | Surface it: branch storage is billed to the branch owner via lineage accounting; expose "bytes retained by branches". |
| Same doc id written concurrently via two lanes | Deterministic tie-break; optional client `version`. |
| Update of a doc that also changes its shard | Shard is `hash(doc_id)`, which never changes. Safe by construction. |

## Cost summary

| Operation | RA |
|---|---|
| Update | same as insert: amortized 1/batch W |
| Point delete | same as insert |
| Bulk delete of 1M rows | 1 W (one delete vector) + 1 commit |
| Branch an index | **1 W** |
| Time-travel read | same as a normal read at that epoch |

## Open questions raised

- OQ-32: Roaring vs. Ribbon/BitMagic for delete vectors at our sizes.
- OQ-33: Branch refcounting under deep branch trees (branch of a branch of a branch).
- OQ-34: Should we expose epochs as public API (real time travel) or keep them internal?
  Leaning public — it is a differentiator and costs nothing.

## Sources

- [Exploring the Architecture of Apache Iceberg, Delta Lake, and Apache Hudi — Dremio](https://www.dremio.com/blog/exploring-the-architecture-of-apache-iceberg-delta-lake-and-apache-hudi/)
- [Writing to an Apache Iceberg Table: How Commits and ACID Actually Work](https://amdatalakehouse.substack.com/p/writing-to-an-apache-iceberg-table)
- [turbopuffer — Database of Databases (copy-on-write namespace branching)](https://dbdb.io/db/turbopuffer)
- [Architecture — LanceDB docs](https://docs.lancedb.com/enterprise/architecture)
