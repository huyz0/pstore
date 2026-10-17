# M96 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. Nothing here is a latency; row sets and
byte counts are exact, recall is provisional (measured on the first run).

Commands:
- `cargo test -p pstore-index --test external`
- `cargo test -p pstore-index --lib external`

1. **Blob round trip and damage.** `a_clustered_blob_round_trips_and_refuses_damage`: size is
   the flat blob plus `4 + nlist·dims·4 + rows·4`; a reopened index answers identically; every row
   is in exactly one list; an unknown magic, an out-of-range list id, a truncated section, a
   trailing byte and a hostile list count (0, `u32::MAX`, one too many) are refused; a `PXI6`
   blob opens with no clusters.
2. **Every list is the flat search.** `probing_every_list_is_the_flat_search`.
3. **A few probes.** `a_few_probes_find_the_neighbours_and_score_a_fraction_of_the_rows`
   (recall@10 at 4 probes at least 0.9, no worse than at 1; at most a quarter of rows scored).
4. **A filter composes.** `a_filter_composes_with_the_probes`.
5. **Refusals and nulls.** `probes_of_zero_and_an_unclustered_index_are_refused`;
   `a_column_with_nulls_is_clustered`. The build's sample is strided over the present rows, so
   nulls on the stride cannot empty it: `nulls_on_the_stride_cannot_empty_the_sample`.
6. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `cargo mutants --no-config --in-diff` over the changed source lines: 84
     mutants, 6 missed, 74 caught, 4 unviable. Four killed by hand: the two probed rows
     constants and the MAGIC_CLUSTERED guard (every row in exactly one list; an unknown magic
     is refused), re-checked caught.
   - ⚠️ Two survivors, **equivalent**: deleting replicas / boundary from the Params of
     build clustered. Only the centroids of that clustering are used and the rows are
     re-assigned afterwards, so replication changes work done, never the result.
   - ⚠️ One more, **equivalent**: division as multiplication in the flat scan's 0..codes.len() / step; the
     extra row numbers have no code, so score returns None for them.

**Not covered**, as the spec states: reading one list's codes by range; replication; any engine
or API surface (row 76).
