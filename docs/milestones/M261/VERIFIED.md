# M261 verification

The spec was not sent to a separate reviewer: it adds one tokenizer to M181's enum and
line grammar, with no new encoding. The code review of the PR covers it.

How to run the tests: `cargo test -p pstore-format --test it tokenizers::` and
`cargo test -p pstore-server --test it tokenizers::`.

Red runs. Not run against the parent. Each test was seen red by the mutations below,
applied by a script, the tests run, and the mutation reverted. No `cargo mutants` run is
claimed for them. All were killed.

1. `whitespace_tokens`: green over seven golden strings (ASCII punctuation, tab, newline,
   U+3000, U+00A0, emoji, leading and trailing spaces, empty), spans, case, folding and a
   limit of 3. Killed: split on ASCII space only (W1); ASCII punctuation split off (W2).
2. `whitespace_lines_round_trip`: green, with and without a limit; and the existing
   `every_tokenizer_and_limit_round_trips` now walks `whitespace` too. Killed: `whitespace`
   written as a `v1` line (W3).
3. `whitespace_tokenizer_over_http`: green, unfolded and folded; a French `language`
   accepted. Killed: the stemming refusal dropped (W4).
4. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. The workspace suite: NOT-RUN at this commit; it is run once over the
   PR's last commit and recorded in that milestone's ledger. `cargo deny check`, coverage
   and the mutation sweep are NOT-RUN here; they are CI's on the PR.
