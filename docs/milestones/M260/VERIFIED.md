# M260 verification

The spec was reviewed once before code: one blocking finding (an older build reads a row's
`k3:` stamp as no declaration and can record the default analyzer at a first fold, so
"refuses" holds for a HEAD only), applied to Delta 5, Risks and `docs/deploy.md`; two
majors (AC3's named mutation could not turn it red; AC1's "to the bit" did not fix the
order of operations), both applied; two minors (a `last_as_prefix` token's `qtf`; the
conflict message omitting `k3`), both applied.

How to run the tests: `cargo test -p pstore-index --test it k3::`,
`cargo test -p pstore-format --test it k3::` and
`cargo test -p pstore-server --test it analyzer::k3`.

Red runs. The new tests cannot compile against the parent (`FullText` has no `k3`, the
scorers take no `k3`), so each was seen red by the mutations below. Each mutation was
applied by a script, the tests above run, and the mutation reverted. No `cargo mutants` run
is claimed for them. All were killed.

1. `k3_weights_repeated_query_terms`, and `k3_counts_only_terms_not_prefixes`: green, three
   `k3` values, both paths equal to the oracle bit for bit. Killed: weight
   `(k3+1)/(k3+qtf)` (N1); the weight dropped from `search_prefixed` (N2).
2. `k3_unset_scores_each_term_once`: green. Killed: an unset `k3` read as 8 (N3).
3. `k3_without_repeats_is_bit_identical`: green. Killed: weight `(k3+1)/k3` for every term
   (N4); `k3` replacing `k1` in `row_score` (N5).
4. `k3_lines_are_refused_by_older_grammars`: green over the v1, v2, v3 and `v4:` lines, out
   of range values and five second spellings. Killed: the prefix not written (N6); the
   range check dropped from `decode` (N7).
5. `k3_over_http`, `k3_unset_is_reported_absent_and_scores_once` and
   `k3_out_of_range_is_refused`: green, unfolded and folded. Killed: `k3` not compared in
   `FullText`'s equality (N8); the range `>= 0` (N9); `describe` omitting `k3` (N10).
6. `k3_in_rank_by_expressions`: green. Killed: `row_score` passed no `k3` from `rank_by`
   (N11).
7. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. The workspace suite: NOT-RUN at this commit; it is run once over the
   PR's last commit and recorded in that milestone's ledger. `cargo deny check`, coverage
   and the mutation sweep are NOT-RUN here; they are CI's on the PR.
