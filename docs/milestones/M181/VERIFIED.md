# M181 verification

The spec was reviewed in two rounds before any code. Round 1 raised five majors and round 2
one (an emoji pair splits under UAX #29, so AC3 uses a single `👩‍💻`); all were applied
before the spec was committed.

How to run the tests: `cargo test -p pstore-format --test it tokenizers::`,
`cargo test -p pstore-query --test it prefix::over_limit` and
`cargo test -p pstore-server --test it tokenizers::`

Red run. The variants and the field were added first with the new tokenizers splitting as
`word_v1`, the limit ignored, `encode` writing the `v1` line and `decode` unchanged:
six of the seven `tokenizers::` tests in pstore-format failed (`every_tokenizer_runs_one_pipeline`
passed, as it should on a `word_v1` split; it is seen red by the AC2 mutations below).
`over_limit_query_tokens_match_nothing` failed on `ContainsAllTokens "abcde abcdefg"`
admitting the row. `tokenizers_over_http` failed on the door refusing `word_v2`.

Each named mutation was applied by a script, the crate's tests above run, and the mutation
reverted; the tree was compared with its state before the run. No `cargo mutants` run is
claimed for them. All were killed.

1. `word_tokenizers_split_as_specified` and `ideographs_before_an_emoji_are_split`: green
   after. Killed: `WordV2` treating ideographs as alphanumeric; emoji dropped under
   `WordV2`; a grapheme split inside an emoji; `WordV3` keeping punctuation segments; `WordV3`
   dropping emoji. The first survived the first sweep where it applied only to the text
   **before** an emoji, which no golden row has; the second test (`東京👍x`) was added for it.
2. `every_tokenizer_runs_one_pipeline`: green after. Killed: case skipped under `WordV3`;
   stemming skipped under `WordV2`.
3. `a_token_over_the_limit_has_no_term`, `a_dropped_token_is_not_in_the_fieldnorm` and
   `over_limit_query_tokens_match_nothing`: green after. Killed: the limit counted in bytes;
   in graphemes; `>` as `>=`; the dropped token counted in the fieldnorm; an over-limit
   prefix read as no constraint; an over-limit `All` token ignored; `plain` dropping the
   limit. The last survived the first sweep: an over-limit prefix is refused by its own
   check, and the BM25 prefix walks a dictionary that never held the long term. What `plain`
   changes is the row side of a prefix filter, so `ContainsAnyTokenPrefix "q abc"` over
   `"abcdefgh y"` at a limit of 5 (no match: `abcdefgh` was dropped) was added, with a
   control over `"abcde y"`.
4. `every_tokenizer_and_limit_round_trips` and `a_second_encoding_is_refused`: green after.
   Killed: `v3` written without the limit; a `v2` line for `WordV1`; `decode` accepting a
   `v3` line for the pre-tokenized tokenizer; `decode` accepting a non-canonical limit. The
   parent's refusal, observed: in a worktree of the spec commit, a probe test ran the
   parent's `FullText::decode` on `v2;english;0000;3f99999a;3f400000;word_v3` and
   `v3;english;0000;3f99999a;3f400000;word_v3;5`, the shapes the new `encode` writes. Both
   returned `None`.
5. `tokenizers_over_http`: green after. Killed: the option ignored at the door; the tokenizer
   not stamped.
6. `tokenizer_tables_are_unicode_16`: green after. No mutation: the assertion is the check.
7. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. `cargo test --no-fail-fast` over pstore-format, -index, -query, -engine
   and -server reported no failure. The two IPv6 `peer_http::response_tests` are NOT-RUN in
   this container (no IPv6), as on every parent. `cargo deny check` is NOT-RUN here: the tool
   is not installed in this container, and CI's `check` job runs it on the PR. Coverage and
   the mutation sweep are CI's on the PR.


Code review, round 1: pass. Its one minor is fixed in `docs/deploy.md`: an older build
refuses a new schema line in a HEAD, but reads a row's undecodable stamp as no declaration,
so the stated reason for the mixed-fleet rule was wrong (the rule itself was right). Two
earlier invocations of `scripts/review.sh context M181` aborted before any reviewer saw a
packet (the disk filled, then a timeout while it ran the gates); their packets were deleted
so the counter counts reviews, and the packet was rebuilt with `REVIEW_SKIP_GATES=1` after the
gates above were run by hand.

CI on the PR: every job green except `mutants-shard (7)`, which missed two mutants in
`raw_spans` (`+` to `-` on each bound of the offset that places the runs between two
emoji). No test had text after a first emoji and before a second, where the offset is
nonzero. `ideographs_before_an_emoji_are_split` gained `x👍東京y👍z`; both mutants, applied by hand,
now fail it. `cargo deny check` passed in CI's `check` job.
