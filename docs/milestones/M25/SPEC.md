# M25 — A rejected row is set aside, never dropped

**Serves:** [BACKLOG](../BACKLOG.md) row 28, which [M7d](../M7d/VERIFIED.md) opened when it chose
"drop and count" over stopping a tenant's folds.

## What is true today

- A fold drops each row that contradicts its index's schema (`row_conflict`), counts it in
  `Head.schema_rejects`, and advances the watermark past its bundle.
- So a row acknowledged `durable` can be lost. This takes a race between two processes that have
  both never read HEAD; M7d bounded it there.
- The rows themselves survive only in their bundle, until GC reaps it. A bundle is shared across
  indexes, so sparing it would keep other rows too.
- Refusing the fold instead stops every later fold for the tenant, forever. That trade is M7d's,
  and this milestone keeps it.

## Delta

**Rows a fold rejects are quarantined: written aside, named by HEAD, and kept until an operator
discards them, or their index is dropped.**

- **The object.** A fold attempt that rejects rows writes them, per index, to one object:
  `…/idx/{index}/quarantine/{epoch:020}-{lane:016x}.q`. The epoch is the attempt's `next.epoch`.
  - It uses the bundle encoding, with the rows as the write path left them.
  - It is created, never replaced, through M23's `Names`.
  - It is written in the attempt, before its commit. A discarded attempt's object is buried with
    the attempt's other names (M23).
- **HEAD** gains `quarantine: BTreeMap<index, Vec<(key, rows)>>`, a new trailing section. Writing
  it writes every earlier optional section's count, `0` if empty, as M15.2's rule requires. A
  HEAD without the section decodes as empty. A HEAD with an empty quarantine encodes
  byte-for-byte as before.
  - `schema_rejects` is unchanged: rows ever rejected. `quarantine` is what is still kept.
- **The live sets** (spec review M1). A key HEAD's quarantine names is live:
  - in `bury_into`, so no fold, branch or compaction buries it, even when it is the attempt's own
    name;
  - in `bury_abandoned` and in GC.
- **Existence** (spec review M6). An index exists for these routes, and for
  `GET /v1/indexes/{index}`, when HEAD names its segments, counts its rejects, or names its
  quarantine. That is the drop's rule, plus the quarantine.
  - An index known only by its rejects now answers its metadata with `segments`, `documents`
    and `approx_row_count` all `0`, where today it is `404`. `GET /v1/indexes` lists it.
- **Retention.** A quarantined object is kept until one of two things happens. A key that was
  live is buried at the committing epoch, as a drop buries its segments, so it gets GC's
  retention window (spec review M4).
  - **Discarded:** `DELETE /v1/indexes/{index}/quarantine?through={epoch}`.
    - It buries the index's entries whose key epoch is at or below `through`, in one commit. On a
      lost CAS it rebases, and still buries only those entries.
    - `through` is the `epoch` the export reported, so a discard never removes rows the operator
      did not see (spec review M3).
    - With nothing to discard it commits nothing, and answers `{"discarded": 0}`.
    - A missing or unparsable `through` answers `400 invalid_request`, and commits nothing.
  - **Its index is dropped:** a drop buries the quarantine with its segments.
  - Nothing else removes one. There is no age limit, because these are acknowledged writes.
- **Export** (spec review M5). `GET /v1/indexes/{index}/quarantine` reads HEAD, then every
  object it names in one parallel round. It answers `{"epoch", "rows": [...]}`, where each row is
  `{"document", "reserved", "reason"}`:
  - **`document`:** the row as a client writes one. Its vector is in client space: euclidean drops
    the stored norm component and is exact; cosine is the stored unit vector, and the magnitude
    is not recoverable; dot product is exact.
  - **`reserved`:** every attribute beginning with `$`, verbatim, such as `$metric`, `$fts`,
    `$trgm`, `$op` and `$cond`. These are why a row can be rejected, and the door refuses them on
    a write.
  - **`reason`:** `row_conflict` against the schema HEAD records now. It is `null` when no schema is
    recorded or the row no longer conflicts. It depends on the serving process's text field.
  - An object HEAD names but the store lacks answers `503 quarantine_unavailable`, not a partial
    export.
  - Unbounded: a quarantine is bounded by the race that fills it.
- **`GET /v1/indexes/{index}`** adds `quarantined_rows` beside `rejected_rows`.

**Not changed:**
- the reject rule, the door, the flush, branching and `as_of`;
- replication, including a replica's discarded writes (`replica_rows`), which are a separate loss
  path this milestone leaves.

## Acceptance criteria

1. **A rejected row is kept.** With M7d's setup, writes that skip the schema check produce
   rejects, and the fold commits. The export returns every rejected row: its id, its client-space
   vector (exact under euclidean), its attributes, its reserved attributes, and a reason.
   `schema_rejects` counts as before.
2. **It is never buried while named.** After a rejecting fold, the committed graveyard holds no
   quarantine key.
   - **GC's guard:** a quarantine key buried by hand (`commit_head_for_test`) survives `gc(0)`.
3. **It survives GC.** After `gc(0)` reaps the bundles, the export is unchanged.
4. **Discard is exact.** `DELETE …?through=E` buries exactly the entries with key epoch ≤ E, at
   the discard's epoch, and keeps a later entry. `gc` with that retention keeps them until it
   passes, then reaps them. A discard with nothing to discard commits nothing.
5. **A drop buries it**, at the drop's epoch.
6. **Existence.** An index known only by its rejects: the export returns its rows, and the
   metadata answers `quarantined_rows`. An unknown index answers `404 index_not_found` on all
   three routes. A discard without a parsable `through` answers `400`.
7. **Cost.** A fold that rejects nothing makes the same requests as the pre-M25 fold, pinned as
   segment_names.rs `an_uncontended_fold_compaction_and_branch_are_unchanged` pins it. A
   rejecting fold makes one more write per index rejected from, and no read.
8. **HEAD.** An old HEAD decodes with an empty quarantine. A HEAD whose only optional section is a
   quarantine round-trips. A HEAD with an empty quarantine encodes to the same bytes as before.
9. **Gates.** `./scripts/gates.sh` is green, and the mutation sweep over M25's source diff misses
   0, every miss closed by a test or named as equivalent.

## Test plan

Engine tests in `crates/pstore-engine/tests/quarantine.rs`, with schema.rs's setup
(`write_without_schema_check_for_test`, `flush_without_schema_check_for_test`). HEAD tests go
in `head.rs`'s unit tests. Server tests go in `crates/pstore-server/tests/quarantine.rs`, with
analyzer.rs's two writers creating one index under different analyzers.

| # | Test | Must fail first because / mutation it catches |
|---|---|---|
| 1 | `a_rejected_row_is_quarantined_intact` | today: no quarantine; euclidean's extra component kept |
| 2 | `a_quarantine_is_never_buried_while_named`, `gc_spares_a_named_quarantine` | quarantine left out of `bury_into`; out of GC's live set |
| 3 | `a_quarantine_survives_gc` | today: the bundle reaped, the rows gone |
| 4 | `discard_buries_exactly_what_was_exported` | `through` ignored; burial at key epochs; an empty commit |
| 5 | `dropping_an_index_buries_its_quarantine` | the drop skipping it |
| 6 | `an_index_known_only_by_its_rejects_is_found` | existence from segments alone |
| 7 | `a_fold_without_rejects_is_unchanged`, `a_rejecting_fold_costs_one_write_per_index` | an object with no rejects; a read before the create |
| 8 | `a_head_without_a_quarantine_decodes`, `a_quarantine_alone_round_trips`, `an_empty_quarantine_changes_no_byte` | the section required; a missed count; bytes changed |
| 9 | server `quarantine_is_exported_counted_and_discarded`, `quarantine_of_an_unknown_index_is_404` | the routes |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Fold, no rejects | unchanged | unchanged | unchanged | 0 | unchanged |
| Fold, rejects | +1 per index per attempt, +1 per refused name | unchanged | unchanged | 0 | +1 write round (the fold is not user-facing) |
| Export | 0 | 1 (HEAD) | 1 per object | 0 | 2 |
| Discard | 1 (commit), 0 when empty | 1 (HEAD) | 0 | 0 | 2 |

A query never reads a quarantine. HEAD grows by one entry per rejecting fold, until discarded.

## Risks

- **Mixed versions.** A pre-M25 binary decodes HEAD without the section, and its next commit
  drops it. The objects would then be named by nothing and buried nowhere: leaked, with the rows
  unreachable. Every process must run M25 before any rejecting fold, as M16 and M23 state.
- **HEAD growth.** Each rejecting fold adds an entry, bounded by the race M7d bounded.
- **A quarantine nobody looks at** is kept forever. That is the point, and `quarantined_rows`
  makes it visible.

## Tasks

- **M25.1** — The engine: the object, the HEAD section, the live sets, existence, drop, discard,
  and export. Tests 1–8.
- **M25.2** — The server routes and `quarantined_rows`, test 9, and `deploy.md`.
- **M25.3** — The ledger, `BACKLOG.md` row 28 closed, and the roadmap row.
