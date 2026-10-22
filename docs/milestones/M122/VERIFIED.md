# M122 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores, the debug profile with
`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`; the bench in
release. `provisional` (AGENTS.md). NOT-RUN on macOS and Windows.

1. `a_dictionary_is_decoded_once_a_buffer` and `a_sparse_dictionary_is_decoded_once_a_buffer`
   pass: one `Bytes` is one decoded dictionary, another buffer of the same bytes another that
   answers every lookup the same, a truncation refused each time. **Red** with the key the
   length alone. The cache's own `a_buffer_is_decoded_once_and_another_again`,
   `a_refusal_is_never_kept` and `the_budget_evicts_the_least_recently_used` pass.
2. `the_one_pass_dedupe_is_the_sorted_one` passes: 180 random candidate sets, NaN and ties
   among them, keep what the sort-and-retain kept. **Red** with a tie kept by the higher row.
3. `two_queries_share_the_unfolded_rows` passes. **Red first** with the view copying the rows.
4. `the_fast_parse_is_the_tree_parse` passes comparing the key count too, **red** with the
   count one high; `the_fast_part_is_the_tree_part` passes with the probe gone, **red** with
   the heading's integer not kept.
5. `./scripts/query-bench.sh`: every digest equals the baseline's -- `47539a4b450befe3`,
   `64d349ed03f0a2f6`, `65559801f84a1412`, `b06b396d0aa759fb`, `60cf6bfe1f9fc517`,
   `d559aff4b3101b72`, `dd1bf0796e2ba34f`. One run: text p50 1.16 ms and hybrid 2.78 against
   M121's single run of 1.51 and 3.51, where the dictionary is now decoded once. One run on
   this host moves ±15%, so no gain is claimed from it.
6. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit. Before it, `cargo
   test --workspace --all-features`: 255 test binaries, all ok.
