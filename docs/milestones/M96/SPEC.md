# M96 — The codes of an external index are clustered, and a query probes some of them

**Serves:** backlog row 72 ("clustering the codes"). [M69](../M69/SPEC.md)'s search scores
**every** code: `rows × code_len` bytes of estimates per query, linear in the column. This groups
the rows by nearest centroid at build time, so a query scores only the lists of the `probes`
centroids nearest to it.

⚠️ **What this does and does not save.** The codes blob is still read whole by `open` (one
request, as M69), so the **bytes moved do not change**; what falls is the codes **scored** per
query, CPU, from `rows` to the rows of `probes` lists. A lazy read of single lists is a
separate piece (below). Nothing here is a latency claim.

## Delta

1. **`ExternalIndex::build_clustered(store, codes_key, column, chunk, lists)`** builds as `build`
   and also clusters: a strided sample of at most `CLUSTER_SAMPLE` (8,192) present rows is read
   and normalized, [`cluster::Clustering`](../../../crates/pstore-index/src/cluster.rs) divides
   it into about `lists` lists (no replication, no exact-scan switch), and every present row is
   assigned the nearest centroid by Euclidean distance on unit vectors. Codes stay **in row
   order**, so a code's position is still its row and M73, M88 and M95 apply unchanged. `lists`
   of 0 is refused; a column with no present row cannot be clustered and is refused.
2. **The blob** is `PXI7`: the `PXI6` blob, then `nlist: u32`, `nlist × dims` centroid `f32`s and
   `rows` list ids (`u32`). `open` accepts both magics; a `PXI6` blob is an index with no
   clusters. A list id of `nlist` or more, a truncated section, or a trailing byte is `Corrupt`.
3. **`search_probes(store, query, k, window, probes)`** and
   **`search_where_probes(.., allow, probes)`** score only the rows of the `probes` lists whose
   centroids are nearest the (normalized) query, then proceed as `search_where`. `probes ≥ nlist`
   scores every row. `probes == 0` and an index with no clusters are refused, not silently
   answered flat. `search` and `search_where` are unchanged (every row).

## Acceptance criteria

1. `a_clustered_blob_round_trips_and_refuses_damage`: the blob is the flat blob plus
   `4 + nlist·dims·4 + rows·4` bytes; a reopened index answers identically; a list id out of
   range, a truncated cluster section and a trailing byte are `Corrupt`; a flat (`PXI6`) blob
   still opens and answers as before.
2. `probing_every_list_is_the_flat_search`: on the 20,000 × 64 fixture, `search_probes` with
   `probes = nlist` returns **exactly** what `search` does, for 20 queries.
3. `a_few_probes_find_the_neighbours_and_score_a_fraction_of_the_rows`: at `window = 200`,
   recall@10 of the exact cosine top 10 at `probes = 4` is at least the floor (measured on the
   first run, provisional), recall at `probes = 1` is no better than at 4, and the rows the
   probes score (`probed_rows`) are at most a quarter of the column.
4. `a_filter_composes_with_the_probes`: `search_where_probes` returns only allowed rows, and at
   `probes = nlist` equals `search_where`.
5. `probes_of_zero_and_an_unclustered_index_are_refused` and `a_column_with_nulls_is_clustered`
   (M88 fixture): no returned row is a null row; full probes equals the flat search.
6. **Gates:** `./scripts/gates.sh` passes; coverage floors hold; the mutation sweep of the
   changed lines misses 0 (an equivalent mutant is argued, never skipped).

## Not covered

- **Reading one list's codes by range** (the blob is read whole): the bytes a query moves are
  M69's. Needs the codes laid out by list, which also gives up codes-in-row-order.
- Replication of boundary rows (`Params::replicas`), re-clustering as a column drifts (a changed
  column is a new blob), and any engine or API surface (row 76).
