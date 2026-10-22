# M120 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores. Tests in the debug profile
with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`, as M119's.

1. `pins_are_checked_in_one_round` passes: 2 rounds and 7 requests for a column over three
   objects, and a moved pin is still `Changed`. `a_multi_field_scan_reads_its_fields_together`
   passes: 2 rounds. **Red first** with the serial code restored: `left: 7, right: 2` and
   `a three-field scan took 4 rounds`.
2. `the_fast_parse_is_the_tree_parse` and `the_fast_part_is_the_tree_part` pass: every body
   of the corpus -- 45 shapes and every truncation of a valid write -- gets the tree's answer
   from the write, query and part parsers and the part's dispatch, error text included, and
   the fast path is taken. M120.2 widened it at review: `-0`, integers past `u64` and `i64`,
   untagged `text` and `include_attributes`, `multi` with and without an unknown field, a
   `null` option, an escaped key repeated, and a protocol of each wrong type.
   **Red first** with the validation pass skipped: `query: {"documents": [{"id": "a",
   "vector": [1e999]}]}` was accepted where the tree said `number out of range`.
   ⚠️ **Drift from the test plan:** it named the duplicate-key check as the mutation. That
   mutation survives: a typed parse of a repeated field fails, and falls back to the tree,
   so the check is defence, not the guard. Validation is the guard, and was the mutation run.
3. `the_table_in_place_is_the_decoded_one` passes: probes, dots and spans to the bit, and a
   refusal at every truncation exactly where `Centroids::decode` refuses. **Red first** with
   the stride a component long.
4. `the_needle_is_the_strings` passes: 16,000 (pair, `k`) cases against the old distance,
   both answers exercised. **Red first** with the row not reset between characters.
5. `./scripts/query-bench.sh`: every digest equals M119's and the baseline's. Medians of three
   runs alternating with M119's binary, M120 / M119: dense 0.99, text 0.87, hybrid 0.99,
   dense+filter 1.00, text+filter 1.01, dense k=100 1.02, text k=100 1.01. One run alone read
   dense at 1.19 and hybrid at 1.40 of M119's numbers from another session: noise, which is
   why the criterion is a median of alternating runs. NOT-RUN on macOS and Windows.
6. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.

## Every finding

[FINDINGS.md](FINDINGS.md) lists every finding of M119's five reviews with its commit and
evidence, or why it was rejected. M120.3's: `the_draws_kept_are_the_draws_made` and
`a_row_asked_twice_comes_back_twice` pass, each **red first** -- the cache keyed without
its seed, and every row moved out at its first mention. M120.5's:
`a_cached_range_owns_its_bytes` and `the_metered_slices_are_the_stores_bytes` pass, each
**red first** -- the fetch's slice admitted, and a slice cut at the wrong offset (which
passed until the test read an object whose bytes differ from each other: the fixture's
are all `7`). M120.6's: `a_scan_names_its_field_once` and `a_field_is_its_name` pass; the
first **red first** with the scan naming its field a row. The bench's digests are unchanged
after it. `the_fast_part_is_the_tree_part`, not run against a mutation at M120, was:
red with the dispatch's phase dropped, and with a typed failure not falling back.
M120.9's: every finding row's fix broken once on purpose (FINDINGS.md's last table); 37
of 39 went red, and the two that survived now have
`the_set_beside_the_list_keeps_what_the_list_kept` and `a_scoped_key_is_the_byte_by_byte_one`,
each **red first** against that same break -- a trigram kept twice, a name's hex unpadded.

## M119's open findings

Every one is closed here: the two depth chains (Delta 1), the double parse of a write, a
part and a single query (Delta 2), the centroid table (Delta 3), `scope`'s copy (Delta 4),
the fuzzy filter (Delta 5), and the spikes (Delta 6). M119's ledger names them.
