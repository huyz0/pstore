# M63 — Highlighting: the query's terms marked in each result's text

**Serves:** backlog row 67, from the project owner's comparison of 2026-10-08. turbopuffer's
`Highlight` and Elasticsearch's highlighter return a result's text with the matched terms
marked, so a client can show why it matched. pstore returns the text only as an attribute.

## What is true today

Read from the tree at M62:

- **A result's attributes are in the answer.** `Engine::resolve_rows` returns each hit's,
  the text attribute among them, from the row round.
- **An index's analyzer is in its schema** (M14), and the engine resolves it for each query
  (`fts` in `query_split_as` and `query_as_of_filtered`). It is not in `Answer`.
- `pstore_format::text::analyze` turns a string into terms. It splits on non-alphanumeric
  characters, then per token lowercases, folds, drops stopwords and stems, as the analyzer
  says. It keeps no offsets.

## Delta

**A ranked query with a text leg may carry `highlight`. Each of the final `top_k` results
whose attribute is a string gets `$highlight`: a fragment of that text of at most
`max_chars` characters, with every token whose term is one of the query's text terms wrapped
in `pre` and `post`. It costs no request.**

1. **The request.** `QueryRequest.highlight`:

   ```json
   "highlight": {"attr": "text", "pre": "<em>", "post": "</em>", "max_chars": 200}
   ```

   - `attr` defaults to `text`, the default text field. Another string attribute may be
     named; it is analyzed by the index's text analyzer though BM25 never indexed it.
   - `pre` and `post` default to `<em>` and `</em>`, at most 16 bytes each; empty is allowed.
   - `max_chars`, a JSON integer from 1 to 2,000, defaults to 200 (spec review: a bound on
     the response, since marks multiply it).
   - `highlight: null` is no highlight, as serde reads it.
2. **Tokens and terms.**
   - A token is a maximal run of alphanumeric characters, as `analyze` splits.
   - `pstore_format::text::tokens(analyzer, s)` returns each token's byte span and its term,
     or none for a stopword. `analyze` becomes the terms of `tokens`, so the two cannot
     disagree.
   - The query's terms are the union of every text leg's query, analyzed by the index's
     analyzer, which `Answer` now carries (`Answer::fts`): the schema's on a live or split
     query, else the unfolded rows', and the epoch's schema under `as_of`. A query whose
     text analyzes to no term (all stopwords) marks nothing, and is not refused.
3. **Marking.** Every token whose term is a query term is wrapped as `pre`, the token's
   original bytes, `post`. A marked span extends over the combining marks that follow the
   token (spec review: in decomposed text an accent is not alphanumeric, and would land
   after `post`). Everything else is the original text, byte for byte. The text is not
   escaped: `pre` and `post` are the client's, and so is escaping (`docs/deploy.md` says
   so, and a test reads that sentence).
4. **A fragment.** Characters are Unicode scalar values (Rust `char`), counted in the
   original text, marks and ellipses excluded.
   - A text of at most `max_chars` characters is whole, with no ellipsis.
   - Otherwise a window starts at a token's first character and runs to the last character
     of a later or the same token, separators included, combining marks after it included.
     For each starting token *i*, the window ends at the last token *j* that keeps it within
     `max_chars`. Its score is the number of marked tokens in it: occurrences, so `run run
     run` beats `run fast`, deliberately. The window with the highest score is chosen, and
     the smallest *i* among equals; with nothing marked, that is the first token's.
   - **A token longer than `max_chars`** (a CJK sentence is one token; so is a URL) is cut
     at `max_chars` characters on a character boundary, and marked if its term matches.
   - `…` (U+2026, one character, no space) is prefixed when a token precedes the window, and
     appended when one follows it. Separators outside the window are dropped.
   - A text with no token is whole if it fits, and otherwise its first `max_chars`
     characters followed by `…`.
5. **Where.** `$highlight` sits beside `id`, `score` and `$dist` in each result. It is absent
   when the result's attribute is missing or not a string. It is computed on the final
   `top_k` rows only, after a boost (M62) and never on its window, from `resolve_rows`'
   attributes, whatever `include_attributes` or `exclude_attributes` asks. Multi-query
   sub-queries may each carry one.
6. **Refused `400`,** checked at the top of `plan`, before an aggregation or an order
   returns, each with a message naming `highlight`:
   - `highlight` not an object, or a field other than the four;
   - an empty `attr`, `id`, or one beginning with `$`;
   - a `pre` or `post` that is not a string or is over 16 bytes;
   - a `max_chars` that is not a JSON integer from 1 to 2,000 (`200.0` is refused);
   - `highlight` on a query with no text leg, or beside `rank_by` or `aggregate_by`.

**Not changed:** the ranking, the format, HEAD, and every query without `highlight`.

**Not covered, and the ledger says so:**
- phrase or proximity awareness: each term is marked wherever it is;
- several fragments a result;
- highlighting a token-predicate filter's terms (`ContainsAllTokens` and its kin).

## Acceptance criteria

1. **Terms are marked.** `highlight_marks_the_querys_terms`, in
   `crates/pstore-server/tests/highlight.rs`, with the expected strings written by hand:
   - under the default analyzer, `Running` is not marked for `run`, and `RUN` is;
   - under an English analyzer with stemming, stopwords and folding: the query `run` marks
     `running` and `Runs`, and not `ran`, which Snowball English leaves as `ran`; `the` is
     never marked; `café` and a decomposed `café` are marked for `cafe`, the accent inside
     the marks;
   - punctuation and spacing are kept byte for byte, and two text legs' terms are both
     marked;
   - custom `pre` and `post`.
2. **Fragments.** `highlight_fragments_hold_the_most_marks`, by hand:
   - a text of exactly `max_chars` characters is whole;
   - the densest window is chosen over an earlier, sparser one, and the earliest among equals;
   - ellipses at either end, and a fragment from the start when nothing is marked;
   - a token longer than `max_chars`, cut and marked;
   - multi-byte characters counted as one each.
3. **Where it appears.** `highlight_rides_every_ranked_answer`:
   - absent for a missing or non-string attribute;
   - present on a boosted query's final rows;
   - present per sub-query in a multi-query;
   - present with `include_attributes` false and with `exclude_attributes: ["text"]`;
   - unfolded and folded alike;
   - under `as_of` an epoch before the index was dropped and re-created with another
     analyzer, marked by the old analyzer.
   - `docs/deploy.md` says the text is not escaped.
4. **`analyze` is `tokens`.** `tokens_spans_cover_what_analyze_returns`, in
   `crates/pstore-format/tests/text.rs`: for several analyzers and texts, the terms of
   `tokens` are `analyze`; every `s[span]` is exactly one maximal alphanumeric run of the
   input, in order and without overlap; and analyzing `s[span]` alone gives that token's
   term, or none. The inputs include `İstanbul`, `½`, a decomposed `é`, and a capitalised
   stopword under a case-sensitive analyzer.
5. **Refusals.** `highlights_are_refused_at_the_door`: every `400` of rule 6, and the limits
   accepted.
6. **Cost.** `a_highlight_costs_nothing`: a cold highlighted query's depth, blob requests and
   blob bytes read equal the same query's without it.
7. **Gates:**
   - `./scripts/gates.sh` passes;
   - the mutation sweep of the changed lines misses 0, or each miss is killed by a test named
     in the ledger.

## Test plan

| AC | Seen red first under |
|---|---|
| 1–3, 5 | `highlight` absent (an unknown field ignored) |
| 1 | the query analyzed by the default analyzer instead of the index's |
| 3 | the current HEAD's analyzer on the `as_of` path |
| 4 | `tokens` absent (compile) |
| 6 | the text attribute fetched again |

## RA budget

No request: the text is in the row round's attributes. CPU: the result's text length, per
result.

## Risks

- **Unescaped text into HTML** is the client's XSS to prevent. The docs say so.
- **Response bytes.** Marks multiply a fragment: at worst 2,000 one-character tokens, each
  wrapped in 32 bytes of marks, about 70 KB a result, so `top_k` of 1,000 answers up to about
  70 MB. `max_chars` and the 16-byte marks bound it; `top_k` bounds the rest.

## Tasks

- **M63.1** `tokens` and `analyze` over it; `Answer::fts`; `highlight` parsed and applied in
  `run`; the docs; the tests: AC1–AC7.
