# M266 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.
One change from the spec's first draft, now stated in the spec: AC1 compares with M159's
`$highlights` only where the two rankings cannot differ. M159 ranks fragments by their
count of marks and `rank_fragments_by` by BM25, so a row with two fragments marked once each
can order them differently, as AC2 shows.

The pinned SDK, `turbopuffer` 3.1.0 (wheel sha256 `f5e44b54…4389`), fixed the shapes:
`types/custom.py` (`ExprHighlight`, `ExprHighlightWithConfig`) and
`types/highlight_config_params.py` (`fragment_limit` defaults to 3; `rank_fragments_by`
defaults to the query's ranking).

How to run the tests: `cargo test -p pstore-server --test it computed::`.

Red runs. Against the parent's sources with these tests, `computed_highlight_matches_m159`,
`computed_highlight_ranks_fragments`, `computed_highlight_refusals`,
`computed_attributes_add_no_round` and `computed_attributes_never_change_the_page` (which
now also carries a `Highlight`) fail: the parent refuses `"Highlight"` as an expression.
Each test was also seen red by the mutations below. A script applied each mutation, ran the
tests and reverted it. No `cargo mutants` run is claimed. All five were killed.

1. `computed_highlight_matches_m159`: green for `utf-8`, `utf-16` and `codepoints`, with
   `fragment_limit` 1 and 2, on the two rows a `gem plain` query returns; the nothing-marked
   row takes its first sentence; with no `fragment_limit`, three fragments equal to M159's
   `fragment_limit: 3`; a row without `text` reports `null`. Killed: the default limit
   left at 1 (H1).
2. `computed_highlight_ranks_fragments`: green. `rare common` takes the rare term's
   sentence, `common` the common one's, and a `Product` of 1000 on `common` flips it back. A
   vector query naming its ranking marks `gem`. Killed: every fragment given the same score,
   so the earliest wins (H2); the fragment scored as the row's whole value (H3); the wrong
   expression's statistics used (H5).
3. `computed_attributes_add_no_round`: green with a `Highlight` naming
   `rank_fragments_by`, for a vector query and a text query.
4. `computed_highlight_refusals`: green for twelve refusals, none reading a blob, and two
   from the snapshot: a `BM25` over a non-text field and a `pre_tokenized_array` field.
   Killed: the missing-`BM25`-over-`attr` refusal dropped (H4).
5. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. The workspace suite, `cargo test --workspace --no-fail-fast -j 3`,
   over this milestone merged with main's M236: 2,907 passed, 1 failed, 15 ignored. The
   failure was `openapi::the_contract_uses_only_keywords_the_validator_checks`: the widened
   `$computed` used `anyOf`, which the contract's validator does not check. It now uses
   `oneOf` (a number or `null`, or an array, which no value is both), and
   `cargo test -p pstore-server --test it -- openapi:: computed::` passes, 15 tests.
   `scripts/gen-clients.py --check` passes after regenerating the clients, and
   `scripts/clients.sh` passes. The two IPv6 tests were NOT-RUN, as in M265. `cargo deny
   check`, coverage and the mutation sweep were NOT-RUN here; CI runs them on the PR.

## After CI's mutation shard (PR #65)
Shard 8: `at += 1` for an expression in `computed_rows` replaced by `*=` was missed, since no
test named an expression before a highlight. `computed_highlight_ranks_fragments` now asks
for `a` (BM25 `common`) and `h` (a highlight ranked by BM25 `rare common`): the highlight
keeps the `rare` sentence, which it would lose ranked by `a`. The mutant, applied by hand,
turns it red.
