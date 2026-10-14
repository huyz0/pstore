# M83 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store. Byte counts are exact;
nothing here is a latency or a hard-corpus recall figure.

Commands: `cargo test -p pstore-index --test fde_clusters`: 8 tests; `cargo test -p pstore-engine
--test named_dense_clusters`: 5 tests; `cargo test -p pstore-server --test named_dense`: 2 tests.

1. `a_vector_query_scores_the_probed_lists_exactly`: over 9,000 rows in 12 topics, every list gives
   the brute-force top 10 with scores equal to the bit; one list gives true dot products, best
   first, and contains the overall best row; a blob of the wrong size is an error; a field's
   `base` of 64 bytes reads the same answer.
2. `a_dense_field_never_shares_a_name_with_a_multi_vector_one`: `dense_name("tag")` is
   `"\0dense\0tag"`.
3. `a_clustered_named_field_reads_a_fraction_of_its_bytes`: the set holds `tag` and not the
   two-vector field `pair`; one probe moves 72,000 bytes of `.fdev`, every list moves all
   144,000 (9,000 rows x 16), the exact scan moves none, and at every list ids and scores equal the
   exact scan's to the bit. ⚠️ 9,000 four-dimensional rows make only two lists at
   `target_list_size` 4,000, so "a fraction" here is one half; the saving is real only at scale.
4. `recall_of_a_named_field_at_one_probe`: recall@10 against the exact scan at `p = 1` over twelve
   queries is **1.0000** (floor 0.9). ⚠️ Twelve well-separated topics: an easy corpus, not evidence
   about hard data, and nothing was measured on one.
5. `a_field_some_row_lacks_keeps_the_exact_scan`: with every seventh document lacking the field it
   is not clustered, no sidecar byte is read, and none of the 10 ranked documents lacks it. Before
   the correction to M77's layout check this query was a `500` (seen red).
6. `a_compaction_rebuilds_the_clusters`: after a second 3,000-document segment is folded and both
   are compacted, the one merged segment has `tag`'s clusters.
7. `a_nul_in_a_field_name_is_refused` and `the_api_searches_a_named_field`: a NUL in a vector
   field's name is refused by the engine on write and `400` in a query's `field`.
8. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`, changed lines): index 16 mutants, 16 caught; engine first
   sweep 20 mutants, 17 caught, 2 unviable, **1 missed** (a field of two vectors being treated as
   dense), killed by adding such a field to the test and re-sweeping every `dense_rows` mutant:
   **17 of 17 caught**; query 13 mutants, 8 caught, 3 unviable, 2 missed (the `fde` arm and a
   `&&` in which segments the sidecar is requested for), which the existing
   `the_clusters_are_asked_for_only_by_an_fde_leg_of_a_flagged_segment` kills when run with these
   tests (re-run of those lines: all caught). ⚠️ `cargo deny check advisories` still fails on
   `paste` (via `foyer`), as before.

**Not covered:** quantised codes for a named field; a hard, high-dimensional corpus; a dedicated
`probes` knob (the dense leg's `p` and `exact` are the knobs).
