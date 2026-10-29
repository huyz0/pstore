# M156 verification

The spec was reviewed in two rounds before any code.
- **Round 1** had one major and four minors. The major: criterion 3 named the phased path for
  a vector-only query, which splits through one-exchange shares, and missed a text leg over
  the primary beside a further-field token filter. The minors: two filters could not tell
  the analyzers apart, the `Or` twin's words, `as_of` after a folded write, and `Scope`'s
  fresh view.
- **Round 2 passed.** Its three minors were applied.

How to run the tests:
- `cargo test -p pstore-server --test it text_filters::`
- `cargo test -p pstore-engine --test it split::a_token_filter`
- `cargo test -p pstore-query --lib filter::tests::a_further`

Red runs, against the parent with the tests alone:
- `a_further_fields_token_filter_is_analyzed_as_its_own_index` failed before the first fold:
  `["title","ContainsAllTokens","Running reports"]` admitted only `r04` on `multi`, the row
  holding "running reports", and its twin admitted a different set.
- `writes_judge_a_further_field_by_its_own_analyzer` failed in the creating fold:
  `patch_by_filter` over `title` marked `w2` alone, the row holding "running".
- `a_token_filter_over_a_further_field_is_not_split` failed: "vector-only
  Tokens("title", Any, "Running", None) was split", with 2 peer calls.
- `a_token_filter_over_the_primary_is_unchanged` was green, a guard, as the spec says. The
  split test's primary half did not run on the parent, since the further-field half panics
  first; it is green after.
- The unit test failed to compile: `bound_texts` and `token_fields` did not exist.

Each test's power to fail rests on the named mutations below. They were applied one at a
time by a script, the test run, and then reverted. All 12 were observed red. One,
"the fold's `Conditions` take no `texts`", first failed to compile as written (a temporary
map borrowed past its statement). It was rewritten to make the fold read an empty `texts`,
which is the same mutation, and observed red. No `cargo mutants` run is claimed for them.

1. `a_further_fields_token_filter_is_analyzed_as_its_own_index`: green after. Killed:
   - "`bound_texts` binds every predicate to `primary`";
   - "the live query passes no `texts`";
   - "the fresh view's `texts` are not read before the first fold";
   - "`as_of` passes no `texts`";
   - "`Scope` carries no `texts`";
   - "`Scope`'s fresh view passes empty `texts`";
   - "`Not` and `Or` bind their children to `primary`".

   The unit test `a_further_fields_token_predicate_binds_to_its_own_analyzer` checks the
   first and last of these directly, and `token_fields` through `And`, `Or` and `Not`.
2. `writes_judge_a_further_field_by_its_own_analyzer`: green after. Killed: "the fold's
   `Conditions` take no `texts`".
3. `a_token_filter_over_a_further_field_is_not_split`: green after. Killed:
   - "`one_part` ignores the filter";
   - "`token_fields` does not look under `Not`";
   - "a filter over the primary is not split".
4. `a_token_filter_over_the_primary_is_unchanged`: green, before and after.

The whole workspace's tests pass, except the two IPv6 `peer_http::response_tests`, which fail
in this container on the parent too.

One finding outside this milestone: clippy refused M155's last commit, a cloned
`Prefetch` in the `query_error` unit case added after its review. M155's clippy run came
before that case was added. It is fixed in its own commit before this one.

Code review round 1 passed with no blocking or major findings. Its minors:
- Criterion 2 runs `patch_by_filter` in the creating fold, but the upsert condition and
  `delete_by_filter` after a fold in both iterations. All three read the same `Conditions`,
  so the patch pins the creating fold's bind; the deviation from the spec's wording is
  recorded here rather than the test restructured.
- "Refuses some row" was met by the row without `title`. It now requires a titled row
  refused.
- One criterion 4 guard compared two empty sets (`"zebras"` under a non-stemming analyzer).
  It now queries `"zebra"`, which admits rows.
