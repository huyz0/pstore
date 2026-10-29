# M158: the `pre_tokenized_array` tokenizer

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("explicit
tokenizer versions"). The [comparison](../../comparisons/turbopuffer-2026-10-25.md) records
that pstore accepts only `word_v1`, and that the vendor also lists `pre_tokenized_array`
and `word_v0` to `word_v4`. This milestone adds `pre_tokenized_array`: the client does its
own tokenization, and each string of a `[]string` value is one token. The `word_v*` versions
are not in it. Matching their token streams needs the vendor's Unicode segmentation
library, and the comparison says a shared name does not establish the same tokens. No D- or
OQ- id covers it.

The vendor's [FTS guide](https://turbopuffer.com/docs/fts), read 2026-10-27, says:
- the tokenizer applies only to `[]string` attributes, and each string is a token;
- `BM25` and `ContainsAllTokens` take a `[]string` operand;
- tokens are matched case-sensitively, with no stemming or stopword removal;
- `language`, `stemming: true`, `remove_stopwords: true` and `case_sensitive: false` cannot
  be declared with it;
- it does not support highlighting.

## Delta
- **`pstore_format::text::Tokenizer { WordV1, PreTokenizedArray }`.** `Analyzer` gains a
  `tokenizer` field, and its default is `WordV1`, so every existing analyzer is unchanged.
- **Under `PreTokenizedArray`, a document's string is one token**, kept byte for byte: no
  case folding, no ASCII folding, no stopwords, no stemming. `tokens(a, s)` and `analyze(a,
  s)`, which every document path calls (`build_with`, the filter's row match, highlight), give
  the whole of `s`, or nothing for `""`. A string holding U+001F is one token like any other.
- **A query operand is read by new `query_tokens(a, q)` and `analyze_query(a, q)`.** Under
  `PreTokenizedArray` they read `q` as tokens joined by U+001F, the ASCII unit separator: each
  non-empty piece is one token, with its span. Under `WordV1` they are `tokens` and `analyze`.
  A `[]string` operand reaches them joined by U+001F. That is how one `String` carries several
  tokens unchanged in `Prefetch::Text`, in `Predicate::Tokens` and over the peer wire. Every
  query-side call moves to them: `Bound::new`, the query layer's text terms, a share's `terms`
  (`shares`), and highlight's terms.
- **A token sequence over a pre-tokenized array spans its elements.** `ContainsTokenSequence`
  matches consecutive elements, since each element is one token. Under `WordV1` a sequence
  stays within one element, as today.
- **`FullText::encode`** writes today's `v1;...` line for `WordV1`, byte for byte. For
  `PreTokenizedArray` it writes `v2;...;pre_tokenized_array`. `decode` reads both. A build
  before this one reads a `v2` line as nothing, so a HEAD that declares a pre-tokenized
  field is refused by such a build rather than read wrongly. See Risks.
- **The server.**
  - `full_text_search: {"tokenizer": "pre_tokenized_array"}` is accepted on the primary or a
    further field. With it, the analyzer is case-sensitive.
  - Refused `400`:
    - `language` (any value);
    - `stemming: true`, `remove_stopwords: true`, `case_sensitive: false` or
      `ascii_folding: true`;
    - a declared `"type": "string"`.

    `ascii_folding: true` is not on the vendor's list. It is refused here because a token is
    kept byte for byte, and folding would change it.
  - Any other tokenizer but `word_v1` is refused, as today.
  - A document in a write that declares the field must hold `[]string` there, or nothing.
    A string or any other type is refused `400`, and so is an element holding U+001F.
  - A query's text leg `{"field": f, "query": [...]}` and a token filter's operand can be an
    array of strings, joined by U+001F. `TextIn::legs` returns owned strings to carry the join.
    `"text": [...]` still means one leg per string, as today, so a pre-tokenized primary
    takes an array only as `[{"field": "text", "query": [...]}]`.
  - An operand, a string or an array's element, holding U+001F is refused `400`. A string
    operand over a pre-tokenized field is one token.
  - An array operand over a `word_v1` field is its elements joined by U+001F, which that
    analyzer splits as it splits a space. So `ContainsTokenSequence ["a", "b"]` there
    matches "a b" within one element, as the string `"a b"` does.
  - `highlight` on a query over an index whose primary is pre-tokenized is refused `400`.
  - The index GET reports `"tokenizer": "pre_tokenized_array"` for such a field.

**Unchanged:** every `WordV1` analyzer, encoding and answer.

## Acceptance criteria
1. **Tokens are the elements.** A test of `tokens`, `analyze`, `query_tokens` and
   `analyze_query` under `PreTokenizedArray`:
   - `"Running Late"` is one term, `"Running Late"`, whose span is the whole string;
   - `""` is no term;
   - `"a\u{1F}b"` is one term under `tokens` and `analyze`, the document side;
   - under `query_tokens` and `analyze_query`, `"a\u{1F}B c\u{1F}\u{1F}d"` is the terms
     `["a", "B c", "d"]`, and each term's span is its piece's bytes;
   - under `WordV1`, `analyze_query` equals `analyze` on each of these inputs.

   `FullText::encode` then `decode` round-trips both tokenizers. A `WordV1` line equals
   today's literal.
2. **It ranks as its tokens.** A twin test over HTTP:
   - Index `pre` declares `tags` as `{"tokenizer": "pre_tokenized_array"}` and holds rows
     whose `tags` are arrays of multi-word, mixed-case strings, such as `["New York", "nyc"]`.
   - Index `twin` holds the same ids, with `tags` replaced by distinct single words that map
     one to one from those tags (`"New York"` becomes `tok0`), declared with the default
     analyzer.
   - Then `{"text": [{"field": "tags", "query": ["New York", "nyc"]}]}` on `pre` (`Sum`
     weight 1) returns the same ids and scores as the mapped query on `twin`.
   - A query for `["new york"]` matches no row of `pre`: case is kept.
   - A query for `["New"]` matches no row: an element is not split.
   - On index `prim`, whose primary `text` is pre-tokenized, `{"text": [{"field": "text",
     "query": ["New York"]}]}` ranks as the same query over `tags` on `pre`.
3. **It filters as its tokens.** On `pre`:
   - `ContainsAllTokens` and `ContainsAnyToken` with an array operand admit the same ids as
     the mapped operand on `twin`;
   - `ContainsTokenSequence ["New York", "nyc"]` admits exactly the rows where those two are
     consecutive elements, in that order;
   - `ContainsAllTokens` with `last_as_prefix` and `["New Y"]` admits the rows with an element
     starting "New Y".

   Each is checked before and after a fold.
4. **The door.** Each of these is refused `400` and stores nothing:
   - the tokenizer beside `language`, `stemming: true`, `remove_stopwords: true`,
     `case_sensitive: false` or `ascii_folding: true`;
   - the tokenizer with `"type": "string"`;
   - a document whose `tags` is a string, a number, or an array holding a number;
   - an element holding U+001F in a document of a write that declares the field;
   - U+001F in a query operand, a string or an array's element;
   - `highlight` over an index whose primary text field is pre-tokenized.

   These are accepted: `{"tokenizer": "pre_tokenized_array", "case_sensitive": true, "k1":
   1.5}`, and the same with `"type": "[]string"`.
5. **The schema reports it.** The index GET's `text_fields.tags.tokenizer` is
   `"pre_tokenized_array"`, and its `case_sensitive` is `true`. A later write that declares
   `tags` with `word_v1` is refused, as a changed analyzer is (M152).

## Test plan
Red first against the parent:
- 1 fails to compile: no `Tokenizer`.
- 2 to 5 fail: the parent refuses the tokenizer.

VERIFIED.md rests each test's power on these mutations, each observed red:
- 1: "query pieces are not split at U+001F"; "`tokens` splits at U+001F"; "a piece is
  lowercased"; "`encode` writes `v1` for both".
- 2: "the query operand's elements are joined by a space".
- 3: "a sequence stays within one element".
- 4: each refusal removed, one at a time.
- 5: "`fts_json` always reports `word_v1`".

The tests live in `crates/pstore-format/tests/it/pre_tokenized.rs` (1) and
`crates/pstore-server/tests/it/pre_tokenized.rs` (2 to 5).

## RA budget
Unchanged. Tokenizing is CPU at write and query time, as today.

## Risks
- **A rolling deploy. Declare the tokenizer only once every server runs this build.**
  - A HEAD holding a `v2` line is refused by an older build, which then cannot read that
    tenant's HEAD until it is upgraded.
  - Worse, an older build reads a row's `v2` stamp (`$fts`, `$texts`) as no stamp at all
    (`fts_of`, `texts_of`). An older build that makes an index's first fold from rows a newer
    build stamped would record the index as `word_v1`, permanently and silently.
  - No test can run an older build, so this is an operating rule, not a criterion. It is
    recorded in the comparison's `tokenizer` row, which this milestone updates. Nothing is
    deployed yet.
- **U+001F** cannot appear in a query token, so a token holding it can be stored but never
  queried by name. Writes that declare the field refuse it. A later write that does not
  declare the field is not checked, since the server sees the schema only when it is declared.
  Such a token is stored whole, never split.
- **A string operand** over a pre-tokenized field is one token. The vendor requires
  `[]string`. Accepting the string is a superset with one meaning.

## Tasks
- M158.1: `Tokenizer`, `tokens`, encoding (1).
- M158.2: sequences across elements, the server's declaration, door, operands, highlight and
  GET (2 to 5).
