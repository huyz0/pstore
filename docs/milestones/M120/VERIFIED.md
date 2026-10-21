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

## M119's open findings

Every one is closed here: the two depth chains (Delta 1), the double parse of a write, a
part and a single query (Delta 2), the centroid table (Delta 3), `scope`'s copy (Delta 4),
the fuzzy filter (Delta 5), and the spikes (Delta 6). M119's ledger names them.
