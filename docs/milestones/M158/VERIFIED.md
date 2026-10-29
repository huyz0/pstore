# M158 verification

The spec was reviewed in two rounds before any code.
- **Round 1** had two majors:
  - An older build reads a row's `v2` stamp as no stamp, so the rolling-deploy risk became
    an operating rule. It is recorded in the comparison's `tokenizer` row.
  - A document's string is one whole token, and U+001F splits only a query operand. So a
    write that does not redeclare the field cannot split a stored token.
- **Round 2** found one major: a leftover bullet in criterion 1 contradicted that. It was
  removed.

One amendment after the code: M14.2's `what_a_token_predicate_cannot_mean_is_refused`
asserted that `["text", "ContainsAllTokens", ["a"]]` is refused, since an operand was a
string. This milestone's Delta makes an array of strings an operand, which the spec did not
list among the tests it reverses. That case now holds `["a", 5]`, an array with a number,
which is still refused with a message naming strings. Its other two cases are unchanged.

How to run the tests:
- `cargo test -p pstore-format --test it pre_tokenized::`
- `cargo test -p pstore-server --test it pre_tokenized::`

Red runs, against the parent with the tests alone:
- Criterion 1, `pstore-format`'s `pre_tokenized.rs`, does not compile: there is no
  `Tokenizer`.
- Criteria 2 to 5 failed: the parent refuses the declaration with "tokenizer
  \"pre_tokenized_array\" is not word_v1, the one tokenizer". Each of the four tests failed
  on it:
  - `a_pre_tokenized_field_ranks_as_its_tokens`;
  - `a_pre_tokenized_field_filters_as_its_tokens`;
  - `what_a_pre_tokenized_field_cannot_be_is_refused`;
  - `the_index_reports_its_tokenizer`.

Each test's power to fail rests on the named mutations below. They were applied one at a
time by a script, the test run, and then reverted. All 18 were observed red. No
`cargo mutants` run is claimed for them.

1. `a_document_string_is_one_token_kept_whole`, `a_query_operand_is_its_pieces` and
   `the_tokenizer_round_trips_and_word_v1_is_unchanged`: green after. Killed:
   - "query pieces are not split at U+001F";
   - "`tokens` splits at U+001F";
   - "a piece is lowercased", applied twice: to a query piece, and to a document's string;
   - "`encode` writes `v1` for both".
2. `a_pre_tokenized_field_ranks_as_its_tokens`: green after. Killed: "the query operand's
   elements are joined by a space".
3. `a_pre_tokenized_field_filters_as_its_tokens`: green after. Killed: "a sequence stays
   within one element".
4. `what_a_pre_tokenized_field_cannot_be_is_refused`: green after. Each refusal removed, one
   at a time, was killed:
   - `language`, `stemming: true`, `remove_stopwords: true`, `case_sensitive: false` and
     `ascii_folding: true`, five mutations;
   - `"type": "string"`. It **survived the first run**: its case's document held `tags` as a
     string, which the document check refuses too. That case now holds no `tags`, so only
     the declaration is judged, and the mutation was observed red;
   - a document's value that is not an array;
   - a document's element holding U+001F;
   - an operand holding U+001F;
   - `highlight`.
5. `the_index_reports_its_tokenizer`: green after. Killed: "`fts_json` always reports
   `word_v1`".

One mutation **survived and changed the code**, not the test: "an element that is not a
string is admitted" by the tokenizer's document check. Such an element is refused before
that check, as in any text field, for a typed, an untyped and a primary declaration alike;
the test now holds all three. So the check tests only for U+001F, and says why.

Code review round 1 passed, with no blocking or major finding. Its three minors are
recorded rather than fixed:
- No test constrains two query-side moves to `analyze_query`: a share's `terms` (`shares`),
  since every M158 test runs on one server, and `Bound::new`'s prefix branch with an
  operand of two or more tokens, since criterion 3's prefix operand has one. The code is
  right; reverting either would survive.
- A `schema_conflict` message describes an analyzer without its tokenizer, so a conflict on
  the tokenizer alone shows the same "expected" and "got".
- `check_pre_tokenized` judges a write's documents, not its patches, as `check_further`
  does. A patch can store an element holding U+001F, a token that cannot be queried by
  name. This is the class the spec's Risks accept for a write that does not declare the
  field.

The whole workspace's tests pass, except the two IPv6 `peer_http::response_tests`, which fail
in this container on the parent too.
