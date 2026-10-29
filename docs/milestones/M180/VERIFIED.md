# M180 verification

The spec was reviewed in two rounds before any code; round 2 passed and its minors were
applied.

How to run the tests: `cargo test -p pstore-query --test it prefix::` and
`cargo test -p pstore-server --test it text_prefix:: a_prefix_query_is_not_split`

Red run: the option was first added to `Prefetch::Text` and ignored. All eight
`prefix::` tests then failed. The HTTP and split tests were seen red through the mutations
"the option dropped at the door" and "a prefix query split" (below), the first of which is
the parent's behaviour short of the refusal.

Each named mutation was applied by a script, the tests above run, and the mutation reverted.
No `cargo mutants` run is claimed for them.

1. `a_prefix_leg_matches_the_reference`: green after. Three segments; analyzers default,
   English with stemming and stopwords, case-sensitive, ASCII folding and
   `pre_tokenized_array`; 42 queries of 1 to 3 raw tokens. The reference computes BM25 from
   global statistics and adds 1.0 for any row term starting with either form. Killed with the
   tests of 2: the prefix scored by BM25 too; the prefix kept among the BM25 terms; 1.0 added
   per matching term; 1.0 added per form; the walk stopping after the first matching term; the
   stem form not walked; the plain form not walked; the early return kept.
2. `a_prefix_scores_one_once`, `the_other_terms_are_bm25s`,
   `a_prefix_matches_as_typed_and_as_stemmed`, `a_prefix_is_folded_as_the_analyzer_folds`,
   `a_prefix_only_segment_is_still_searched`, `a_prefix_is_weighted_in_a_sum`: green after.
   Killed: case folding skipped on the prefix. That one survived the first sweep, because
   under the default analyzer the stem form is already folded; `The` under English stopwords
   (no stem form) was added to the folding test and to the oracle's queries, and it is now
   killed. "The prefix's 1.0 left out of the weighted sum" was NOT applied: the 1.0 is part
   of the leg's score, so the code has no separate value that could be left out of the sum
   without new plumbing. `a_prefix_is_weighted_in_a_sum` pins the doubled 2.0 instead.
3. `a_prefix_leg_costs_no_round`: green after. `coalesce_gap` 0, a prefix matching ten
   terms: equal depth, and one to two more GETs. Killed: the prefix's postings fetched in a
   second round; a range per term.
4. `text_legs_take_last_as_prefix` (unfolded and folded) and `a_prefix_query_is_not_split`
   (in `peers.rs`): green after. Killed: the option dropped at the door; a prefix query split.
5. Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -D
   warnings` pass. `cargo test --no-fail-fast` over pstore-format, -index, -query, -engine and
   -server reported no failure, and pstore-server's suite was rerun after clippy's type-alias
   fix: 510 integration tests and 106 unit tests passed. The two IPv6
   `peer_http::response_tests` are NOT-RUN in this container (no IPv6), as on every parent.
   Coverage and the mutation sweep are CI's on the PR.

Code review, round 1: pass. Its one minor (this ledger was missing, so `DELIVERY.md`'s link
to it dangled) is fixed by this file. It noted that `shares`' statistics terms in
pstore-engine still analyze the whole query; that is unreachable for a prefix leg, which
`shareable_fields` never splits.
