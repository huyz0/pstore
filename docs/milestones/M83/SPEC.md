# M83 — An approximate index for a named dense field (PLANNED, not built)

**Serves:** backlog row 65's remainder, after [M77](../M77/SPEC.md) (exact scan) and
[M78](../M78/SPEC.md) (it splits). **Status: planned.** The design choice the backlog left open is
made here so the work can start without a further decision.

## Decision

Per-field clustering by a **permutation sidecar**, mirroring [M71](../M71/SPEC.md)'s
`FdeClusters` rather than reordering the segment (a segment has one row order, which `vector`'s
clustering decides and the postings share). Reusing M71 means the lookups, replication, peer
splitting and compaction paths already have a worked example; a per-field reorder would not.

- A segment with a named dense field of one vector a row and at least `exact_scan_threshold` rows
  gets `<segment>.dcc.<field>` (centroids + per-list segment row numbers) and `<segment>.dcv.<field>`
  (the field's vectors in list order), written at flush/compaction beside the existing sidecars.
- A `Target` carries which fields are clustered (as `fde_clusters` does today); a dense leg over a
  named field reads the best `probes` lists' rows by computed range, and scores exactly. A segment
  without the sidecar falls back to M77's exact scan.
- The API's dense query accepts `probes` for a named field only (as `multi` does for `fde`).

## Acceptance criteria

1. `a_clustered_named_field_reads_a_fraction_of_its_bytes`: over a segment large enough to be
   clustered, a dense query on a named field moves fewer bytes than the exact scan, and at all
   lists returns the exact scan's ranking.
2. `recall_of_a_named_field_at_default_probes_meets_the_floor`: recall@10 against the exact scan is
   at or above the repository's floor (D-35) on the recall corpus's named-field analogue.
3. `a_compaction_and_a_replica_carry_the_sidecar`: the sidecar is rebuilt on compaction, copied by
   replication, and a split query over it equals the unsplit one.
4. `probes_are_refused_for_the_wrong_field_or_mode`: `probes` on `vector` or on an exact-scan field
   is `400` naming `probes`.
5. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Size:** L. Touches `pstore-index` (a `dcc` module beside `fde_clusters.rs`), `pstore-query`
(`Target`, `run.rs`), `pstore-engine` (flush, compaction, replica) and `pstore-server` (`peers.rs`,
the dense leg's `probes`).

**Not covered:** several vectors a document in a named field (that is `multi`); quantised codes
for a named field.
