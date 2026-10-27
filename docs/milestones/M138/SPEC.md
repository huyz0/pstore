# M138: token filters over string arrays, and `last_as_prefix`

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("token arrays,
`last_as_prefix` ... match exhaustive reference predicates ... A negative predicate must never
turn an unbound analyzer into an all-row match"); the [comparison](../../comparisons/turbopuffer-2026-10-25.md)
lists both as missing. No D- or OQ- id covers it.

## Delta
Today the token filters of [M14.2](../M14/SPEC.md) take a string, match a scalar string
attribute only (an array is false), and refuse a fourth element.

After, following the vendor's [query documentation](https://turbopuffer.com/docs/query):
- **String arrays.** An array holding at least one string is a string-holding attribute; its
  string elements are analyzed one by one (other elements contribute nothing):
  - `ContainsAllTokens`: every query term is among the terms of some element, together;
  - `ContainsAnyToken`: some query term is among some element's terms;
  - `ContainsTokenSequence`: the sequence occurs within one element, never across two.
  - A query with no terms keeps M14.2's vacuous readings (`All` and `Sequence` admit, `Any`
    does not). An array with no string element, the empty array included, admits nothing,
    as a number attribute admits nothing.
- **`last_as_prefix`.** An optional fourth element on `ContainsAllTokens` and
  `ContainsAnyToken`: an object whose only key may be `last_as_prefix`, a bool. `{}` and
  `false` are today's behaviour. With `true`, with `a` the index's analyzer and `plain` that
  analyzer with stemming and stopword removal off:
  - the **prefix** is the query's last raw token (the last span of `text::tokens`) as `plain`
    makes it: case and folding applied, never stemmed, never dropped as a stopword;
  - the **other terms** are `analyze(a, text before that token)`, matched as today;
  - the prefix matches a row token whose `plain` form starts with it. That includes a row
    token `a` drops as a stopword: prefix `the` matches the row's `the`.
  - A query with no raw tokens has no prefix and behaves as without the option.
  - `All` needs every other term and the prefix; `Any` needs one of them. Across an
    array's elements, as above.
- **Refused** (`400 malformed filter`): a fourth element that is not an object (`null`, `true`),
  any other key, a non-bool `last_as_prefix`, any fourth element on `ContainsTokenSequence`
  (the vendor documents none), and a fifth element.
- **Representation.** `TokenOp` gains `AllPrefix` and `AnyPrefix`; `Bound` gains the prefix,
  computed in `Bound::new` from the op. The condition encoding writes them as token-op codes
  3 and 4.
- **Unbound stays unknown** (M14.2): binding is unchanged.
- **Reversed on purpose**, each in the commit that implements it: `filter.rs`'s
  `tokens_are_three_valued_and_never_admit_unbound` asserted an array of `"fox"` false for
  `Any "fox"`; `tests/it/tokens.rs`'s reference mapped the `tags` array to no tokens;
  `condition.rs`'s unknown-token-op case `07..03` moves to `05`. The M14 spec gets a
  correction banner on "an array of strings admits nothing".

Unchanged: the analyzer's configuration and pipeline; BM25 ranking (`rank_by` with
`last_as_prefix` needs a dictionary prefix walk with its own request budget, a later
milestone); a token predicate over a number or bool attribute; storage formats.

## Acceptance criteria
1. **Oracle.** Over rows holding: absent, `""`, strings, `[]`, `[5]`, string arrays of 1 and 2
   elements (a query term pair split across two elements, a repeated token, a non-string
   element beside strings), for each analyzer of {default, English stemming + stopwords,
   case-sensitive, ASCII folding}, and queries of 0 to 3 raw tokens with and without the
   prefix: each operator admits exactly what an independent reference admits, and so do `Not`
   of it and `Not(Not)` of it. The reference: per string element, `analyze(a, e)` for terms
   and `analyze(plain, e)` for prefix forms; the prefix the last of `analyze(plain, q)`.
2. **Prefix specifics**, English stemming and stopwords on: prefix `run` matches `running`;
   `running` does **not** match `runs` and `theory` does **not** match `theories` (stemmed,
   both pairs would); `the` matches `theory`; a prefix never matches a shorter token;
   with folding, `cafe` matches `Café`.
3. **HTTP.** Over folded and unfolded rows, a string-array attribute and the prefix option
   return the expected ids through a query filter and through `delete_by_filter`, with a
   prefix query whose answer differs from the same query without the option.
4. **Refusals.** Each refused shape above is `400`; `{}` and `{"last_as_prefix": false}`
   answer as no option.
5. **Conditions.** Codes 3 and 4 round-trip with their prefix; code 5 decodes as `None`.

## Test plan
Red first against the parent: 1 to 3 fail (arrays false; the option a 400); 5 does not
compile. 4's refusals are guards, green on the parent (it refuses every fourth element);
VERIFIED.md records them as such.
- 1, 2: `pstore-query` unit tests. Mutations: "the prefix is stemmed" (2's negatives), "a
  sequence matches across elements", "`All` per element instead of across elements",
  "the prefix is matched as a whole token", "an empty array admits `All \"\"`".
- 3, 4: `crates/pstore-server/tests/it/tokens.rs`. Mutation: "the option is read and
  ignored" (3's differing answer).
- 5: the `condition` round-trip tests, extended. Mutation: "the prefix is not encoded".

## RA budget
Unchanged: evaluated on rows a mask already decodes; token predicates never prune (M14.2).

## Risks
- **"Literal prefix" is the vendor's phrase, not a definition.** This spec reads it as `plain`.
- **`ContainsAnyToken` on arrays and non-string elements are undocumented** by the vendor;
  this spec picks "any element" and "contribute nothing". Conformance fixtures would reveal
  a different reading.
- **An older fold** decodes codes 3 and 4 as `None` and skips the guarded write (M135's risk).

## Tasks
- M138.1: arrays and the prefix in `pstore-query` (criteria 1, 2, 5), with the reversed unit
  assertion and the condition case.
- M138.2: the parser's option, the server tests (3, 4), the `tokens.rs` reference, and the
  M14 correction banner.
