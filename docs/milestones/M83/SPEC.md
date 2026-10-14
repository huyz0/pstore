# M83 — An approximate index for a named dense field

**Serves:** backlog row 65's remainder, after [M77](../M77/SPEC.md) (exact scan) and
[M78](../M78/SPEC.md) (it splits).

## Decision

Per-field clustering by a **permutation sidecar**, in [M71](../M71/SPEC.md)'s `.fdec` / `.fdev`
pair, not a reordering of the segment (a segment has one row order, which `vector`'s clustering
decides and the postings share). The pair is already one object a segment, already derived by GC
and replication, already read by one request in the leg round: so a named dense field is one more
entry in the same `FdeSet`, and **no new sidecar key, HEAD flag, replication path or GC path is
added**.

## Delta

- `FdeClusters::search_vector_at(.., q, probes, limit)` scores the rows of the `probes` best lists
  by dot product with `q`, already in the clustered space. `search_at` encodes its multi-vector
  query and calls it.
- At seal (flush and compaction both go through it) the engine adds each named dense field of one
  vector a document to the set, named `fde_clusters::dense_name(field)` = `"\0dense\0" + field`.
  ⚠️ **Only a field every row of the segment carries**: a missing row would be a zero vector, which
  scores 0 and outranks real negative scores. Such a field keeps the exact scan.
- A field name may not contain a NUL (the engine refuses it at the door; the API says `400`): the
  NUL is what keeps `dense_name` from ever being a multi-vector field's name.
- A dense leg over a named field reads the best `tune.p` lists when the segment has the field's
  clusters and `tune.exact` is false; `exact` (or a segment without clusters) scans the field
  exactly, as M77 does. The sidecar is asked for by such a leg and no other dense leg.
- ⚠️ **A correction to M77, found while testing this:** a field some documents lack is stored with
  an offset table, as a field of several vectors is, and M77's check on that layout refused it as
  "several vectors a document" (`500`, retryable). The check now reads the rows: a row of none is
  skipped, a row of several is `multi`'s.

## Acceptance criteria

1. `a_vector_query_scores_the_probed_lists_exactly` (`pstore-index`): over 9,000 rows, every list
   gives the brute-force top 10 to the bit; one list gives true dot products, best first, with the
   overall best row in it; a vector blob of the wrong size is corrupt; a field's `base` shifts
   where it is read.
2. `a_dense_field_never_shares_a_name_with_a_multi_vector_one`: `dense_name` starts with NUL.
3. `a_clustered_named_field_reads_a_fraction_of_its_bytes` (`pstore-engine`): over a 9,000-document
   segment, `.fdev` holds the field's clusters; one probe moves no more than about half the
   field's bytes, every list moves exactly the field, and at every list the leg's ids and scores
   equal the exact scan's, to the bit; the exact scan reads no sidecar.
4. `recall_of_a_named_field_at_one_probe`: recall@10 against the exact scan at `p = 1`, over 12
   topical queries, is at least 0.9. ⚠️ Measured 1.0000 on a corpus of twelve well-separated
   topics: an easy one, not evidence about hard data.
5. `a_field_some_row_lacks_keeps_the_exact_scan`: a field missing from every seventh document is
   not clustered, nothing is read from a sidecar, and no document lacking it is ranked. (This was
   a `500` before the correction above.)
6. `a_compaction_rebuilds_the_clusters`: after a second segment is folded and both are compacted,
   the merged segment has the field's clusters.
7. `a_nul_in_a_field_name_is_refused` (engine) and `the_api_searches_a_named_field` (server): a
   NUL in a field name is refused, in a write and in a query's `field`.
8. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0, or
   each miss is killed by a named test.

**Not covered:** several vectors a document (that is `multi`); quantised codes for a named field;
a measurement on a hard, high-dimensional corpus; a dedicated `probes` knob (the dense leg's
existing `p` and `exact` are the knobs); and the lists' size: at `target_list_size` 4,000 a
four-dimensional field of 9,000 rows has only a few lists, so the saving is real only at scale.
