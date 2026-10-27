# M138 verification

The spec was reviewed in one round before any code; its findings are folded into the
[spec](SPEC.md).

How to run the tests: `cargo test -p pstore-query --lib` and
`cargo test -p pstore-server --all-features --test it tokens::`.

Red runs:
- Criteria 1, 2 and 5 did not compile against the parent: `TokenOp::AllPrefix` and
  `TokenOp::AnyPrefix` did not exist.
- Criterion 3, with M138.1 in place and the parser not yet taking the option:
  `each_token_predicate_equals_brute_force_under_the_index_analyzer` and
  `before_the_first_fold_the_declared_analyzer_applies` answered `400`, "expected [attr, op,
  value]", on their prefix cases. `string_arrays_and_prefixes_through_the_api` failed first
  on an expectation of mine: `ContainsAnyToken "dog jump"` also admits `"jumping"` under the
  stemming schema. The case became `"dog jum"`, whose answer differs with the option, and
  the mutation "the option is read and ignored" below is its red.
- Criterion 4 is a guard: green on the parent, which refused every fourth element.

The mutations were applied one at a time by a script and then reverted. No `cargo mutants`
run is claimed for them.

1. `token_predicates_over_arrays_and_prefixes_match_the_reference`: green after. Killed: "a
   sequence matches across elements", "`All` per element instead of across elements", "an
   empty array admits `All \"\"`", "the prefix is stemmed", "the prefix is matched as a
   whole token".
2. `a_prefix_is_neither_stemmed_nor_dropped`: green after. Killed: "the prefix is stemmed"
   and "the prefix is matched as a whole token".
3. `string_arrays_and_prefixes_through_the_api`: green after, folded and not, through a
   query and `delete_by_filter`. Killed: "the option is read and ignored" (also killed by
   the two brute-force tests named above).
4. `what_a_prefix_option_cannot_mean_is_refused`: green before and after.
5. `every_predicate_round_trips` and `every_truncation_and_every_unknown_tag_is_none`: green
   after. Killed: "the prefix is not encoded" (`AllPrefix` written as code 1).
