# M71 — The multi-vector encoding, clustered by a permutation sidecar

**Serves:** backlog row 71's unmet half and row 65, after [M67](../M67/SPEC.md) shipped the `fde`
leg as a flat scan of `rows × 2 KB`. The project owner chose the **permutation sidecar** design
on 2026-10-13.

## The constraint, and why a sidecar

A segment has one row order. The dense clustering of `vector` decides it, and the text and
sparse postings address those rows, so the `name$fde` field cannot be physically reordered into
its own lists. The sidecar leaves the segment alone.

⚠️ **A permutation that only names rows does not work, and this was measured.** Reading the
probed lists' rows out of the segment's own `name$fde` field by computed range moved
**3,067,904 of 3,072,000 bytes** at 4 of 12 lists (1,500 documents): the blob layer coalesces
ranges closer than `coalesce_gap` (64 KB in memory, 1 MB against S3) because a hole is cheaper
to read than a request, and rows scattered at 2 KB across a few MB are always within it. So the
sidecar holds the **encoded rows themselves, in list order**: a list is one contiguous range.
That **duplicates the field's bytes** (2,056 per row, times the replication the clustering adds),
which is the price of the lists, and the flat field stays in the segment for the fallback.

Two objects, derived keys like `.cen`, `.tdict` and `.sdict`:
- `<segment>.fdec`: the centroids, and per list the segment **row numbers** (the permutation).
- `<segment>.fdev`: the encoded rows, `dim` floats each, concatenated in list order.

Depth: `.fdec` (round 1), the probed lists' ranges of `.fdev` (round 2), then the opt-in exact
rerank as before.

## Delta

`pstore_index::fde_clusters`:
- `key(segment)`, `vectors_key(segment)`: the two derived keys.
- `FdeClusters::build(rows, params)`: clusters the FDE rows with the repo's own `Clustering`
  (deterministic, balanced, replicating boundary rows, so a row may sit in several lists), and
  returns the clusters and the vectors blob.
- `encode` / `decode`: a magic, the counts, the centroids and the lists; `decode` refuses
  anything truncated, mislabelled, ragged, or with a row number out of range.
- `put` / `open`: write and read both objects.
- `search(&self, store, segment_key, query_vectors, probes, limit)`: encodes the query (sum),
  ranks centroids by **dot product**, reads the best `probes` lists' ranges of `.fdev`, scores
  by dot, deduplicates a replicated row, and returns the top `limit` as `(row, score)`, ties by
  row.

**Not covered here (M71.2):** the engine writing the sidecar at seal and compaction, GC deleting
it, the query coordinator preferring it, and a size threshold below which a segment stays a
flat scan.

## Acceptance criteria

1. `sidecar_round_trips_and_refuses_damage`: encode and decode are inverse; a truncated,
   wrong-magic, ragged-dimension, list-count-mismatch and out-of-range-row blob are each refused.
2. `build_is_deterministic_and_covers_every_row`: the same rows build the same bytes; every row
   appears in at least one list; no list names a row past the end.
3. `clustered_search_finds_what_the_flat_scan_finds`: on a seeded 1,500-document corpus of
   8–24 vectors, at `probes` = 4 of ~12 lists the clustered `fde` ranking's top 10 shares **at
   least a floor measured on the first run (provisional)** of the flat scan's top 10, and a
   control (a sidecar built from another corpus) falls below it.
4. `clustered_search_reads_a_fraction`: bytes moved of `.fdev` are **under half** of probing every
   list at that setting (measured with the accounted store).
5. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the new module misses 0, or
   each miss is killed by a named test.
