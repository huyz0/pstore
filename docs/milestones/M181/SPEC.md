# M181: the `word_v0`, `word_v2`, `word_v3` and `word_v4` tokenizers, and `max_token_length`

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("explicit
tokenizer versions"; "token byte limits"). The [comparison](../../comparisons/turbopuffer-2026-10-25.md)
says pstore accepts `word_v1` and `pre_tokenized_array` (M158) but not `word_v0` to
`word_v4`, and has no `max_token_length`. No D- or OQ- id covers it.

The vendor's [FTS guide](https://turbopuffer.com/docs/fts), read 2026-10-29:
- `word_v4` is the default for new namespaces, and the default "is periodically upgraded".
- `word_v4` "behaves like `word_v3`, but is roughly 3x faster and fixes a few tokenization
  edge cases". `word_v3` and `word_v4` use Unicode v17.0 text segmentation (UAX #29), with
  "accurate segmentation across most languages, scripts, and emojis".
- `word_v2` "forms tokens from ideographic codepoints" and from "sequences of emoji
  codepoints that form a single glyph". "Codepoints that are not alphanumeric, ideographic,
  or an emoji are discarded." It uses Unicode v16.0.
- `word_v1` is `word_v2` except that "ideographic codepoints are treated as alphanumeric".
  It uses Unicode v10.0.
- `word_v0` is `word_v1` except that emoji codepoints are discarded.
- "Every word-like token consumes a position even when a filter (length, stopword) drops
  it". `max_token_length` is "in characters (1–255)". Its default, and what a longer token
  becomes, are not published. The positions sentence names a length filter that drops.

## Delta
1. **`Tokenizer` gains `WordV0`, `WordV2`, `WordV3` and `WordV4`.** Each splits a string into
   raw tokens, each with its byte span. Case, ASCII folding, stopwords and stemming then apply
   to each raw token as they do under `WordV1` today: one pipeline, `tokens()`, which every
   document, query, filter and highlight path already calls.
   - **`WordV0`:** maximal runs of alphanumeric codepoints (`char::is_alphanumeric`, which
     includes ideographs). This is exactly what pstore's `WordV1` does today.
   - **`WordV1`: unchanged, byte for byte.** It equals `WordV0`. The vendor's `word_v1` also
     keeps emoji. pstore's does not, because changing it would silently retokenize every
     existing index whose stamp says `word_v1`. That difference is stated in `docs/deploy.md`
     and the comparison.
   - **`WordV2`:** maximal runs of alphanumeric codepoints that are not `Ideographic`. Each
     `Ideographic` codepoint is a token of its own. Each extended grapheme cluster (UAX #29)
     that starts with an `Extended_Pictographic` or `Regional_Indicator` codepoint is one
     token. Everything else is discarded. "Forms tokens from ideographic codepoints" is read
     as one token per codepoint, since `word_v1`'s difference is that it treats them "as
     alphanumeric", which is runs.
   - **`WordV3` and `WordV4`:** UAX #29 word boundaries (the `unicode-segmentation` crate). A
     segment is a raw token when it holds an alphanumeric codepoint, or starts with an
     `Extended_Pictographic` or `Regional_Indicator` codepoint. Whitespace and punctuation
     segments are discarded. `WordV4` is the same token stream as `WordV3`, because the
     vendor's edge-case fixes are not published. The crate implements Unicode 16.0, not
     17.0. Both differences are stated.
2. **`max_token_length`, `1..=255` characters.** It is an `Analyzer` field, `None` by default
   (no limit, as today). A character is a Unicode scalar value (`char`), not a grapheme:
   `👩‍💻` is 3. A raw token holding more characters than the limit has no term: it keeps its
   span and is dropped from the document, as a stopword is, and the row's fieldnorm does not
   count it. Characters are counted in the raw token before case or folding. It applies
   under every `word_v*` tokenizer. Under `pre_tokenized_array`, where a token is kept as
   written, it is refused.
3. **An over-limit query token matches nothing.** Every stored term comes from a raw token
   within the limit, so a longer query token can match no row, and is never read as "no
   constraint":
   - BM25: it is not a term, so it scores nothing; the other terms score as usual.
   - `ContainsAllTokens` and `ContainsTokenSequence` with an over-limit token match no row.
     `ContainsAnyToken` ignores it.
   - A literal prefix (M138's filter, M180's leg) over the limit matches no row: the
     `AllPrefix` filter is false, `AnyPrefix` ignores it, and M180's leg adds no 1.0.
     `plain` keeps the index's limit, so a row's tokens are what the index stored.
   - This differs from a stopword, which those filters ignore, as today.
4. **Encoding.**
   - `FullText::encode` keeps the `v1` line for `WordV1` with no limit, byte for byte.
   - It keeps the `v2;...;pre_tokenized_array` line for M158's tokenizer.
   - It writes `v2;...;<tokenizer>` for any other tokenizer with no limit.
   - It writes `v3;...;<tokenizer>;<max_token_length>` whenever a limit is set.
   - A build before M181 refuses a `v2` line with an unknown name and any `v3` line, so it
     refuses such an index rather than reading it wrongly (as M158).
   - `decode` reads exactly these, and refuses anything else, so each `FullText` has one
     encoding:
     - `v2;...;word_v1`, which is the `v1` line's;
     - a `v3` line naming `pre_tokenized_array`, which the door refuses;
     - a `v3` limit outside `1..=255`, or not written as a canonical decimal (`05`, `+5`,
       `5.0`).
5. **Server.**
   - `full_text_search.tokenizer` accepts `word_v0` to `word_v4` and `pre_tokenized_array`.
     The default stays `word_v1`, so every write that names no tokenizer is unchanged. The
     vendor's newer default is stated as a difference.
   - `max_token_length` is a JSON integer in `1..=255`, read with `as_u64`. Anything else is
     refused `400`, including `5.0`, `null` and negatives, and so is any value under
     `pre_tokenized_array`.
   - The index GET reports each field's tokenizer, and `max_token_length` when it is set.
   - A declaration that differs from the stored one is refused like any other change of a
     full-text schema. Tokens are fixed per index, and an upgrade never retokenizes.
6. **Dependencies and the Unicode version.**
   - `unicode-segmentation` is added to `pstore-format`.
   - The `unicode-bool` feature is added to `regex-syntax`, so `Extended_Pictographic`,
     `Regional_Indicator` and `Ideographic` are read from its tables, through ranges parsed
     once.
   - `cargo deny` must pass.
   - Three Unicode tables make the tokens: `char` (the toolchain, pinned at 1.90 by
     `rust-toolchain.toml`), `unicode-segmentation`, and `regex-syntax`'s property tables. All
     three are Unicode 16.0 today. `unicode-segmentation` and `regex-syntax` are pinned with
     `=` in `pstore-format`. A test asserts `char::UNICODE_VERSION` and
     `unicode_segmentation::UNICODE_VERSION` are `(16, 0, 0)`; `regex-syntax`, which has no
     version constant, is held by its `=` pin. A bump fails the test or needs the pin moved,
     because a Unicode change is a new tokenizer version, not an upgrade.

**Not changed:** the `word_v1` token stream; every existing stamp, index and answer;
`pre_tokenized_array`; stopword languages (English only).

**Not covered:** token positions. pstore stores none, so `ContainsTokenSequence ["a", "b"]`
matches `"a <over-limit> b"` across the dropped token, as it matches across a stopword
today; the vendor's dropped token "consumes a position". Also not covered: the vendor's
unpublished `word_v4` fixes; Unicode 17.0; stopwords beyond English; dictionary segmentation
of Thai, Lao, Khmer and Myanmar.

## Acceptance criteria
1. **Golden token streams.** `word_tokenizers_split_as_specified`, in
   `crates/pstore-format/tests/it/`, asserts each tokenizer's raw tokens, as the strings
   their spans cut, for this table. The table is the definitions applied by hand, so the test
   cannot be written by copying the code's output. Under the default analyzer (lowercase, no
   stemming) each raw token's term is the token lowercased. `WordV0` equals `WordV1` on
   every row, and `WordV4` equals `WordV3`.

   | Input | `word_v0`, `word_v1` | `word_v2` | `word_v3`, `word_v4` |
   |---|---|---|---|
   | `can't stop 3.14 e-mail` | `can` `t` `stop` `3` `14` `e` `mail` | as `word_v0` | `can't` `stop` `3.14` `e` `mail` |
   | `cafe` U+0301 ` x` (decomposed) | `cafe` `x` | `cafe` `x` | `cafe`U+0301 `x` |
   | `東京タワー` | `東京タワー` | `東` `京` `タワー` | `東` `京` `タワー` |
   | `rust是好的` | `rust是好的` | `rust` `是` `好` `的` | `rust` `是` `好` `的` |
   | `👩‍💻 dev` (U+1F469 U+200D U+1F4BB) | `dev` | `👩‍💻` `dev` | `👩‍💻` `dev` |
   | `🇯🇵` | none | `🇯🇵` | `🇯🇵` |
   | `1` U+FE0F U+20E3 (keycap) | `1` | `1` | the whole cluster |
   | empty, and `" \t\n "` | none | none | none |
2. **One pipeline.** For each tokenizer, `analyze` equals `tokens` filtered to terms, with
   case, folding, English stopwords and stemming applied per raw token as `WordV1` applies
   them. `analyze_query` equals `analyze`.
3. **Length.**
   - Under `max_token_length: 5`, `"abcde abcdef"` gives one term, `abcde`, and two spans.
   - The limit counts characters, not bytes: `"ééééé"` (precomposed U+00E9, 10 bytes) is
     kept. Under `word_v3`, `"👩‍💻"` (U+1F469 U+200D U+1F4BB: 3 characters, 11 bytes, one
     grapheme) is dropped at 2 and kept at 3.
   - The row's fieldnorm counts only kept terms, so BM25 over `"abcde abcdef"` scores
     `abcde` with a length of 1.
   - `over_limit_query_tokens_match_nothing`, in `crates/pstore-query/tests/it/`, over rows
     `"abcde x"` and `"x"`:
     - `ContainsAllTokens "abcde abcdefg"`, `ContainsTokenSequence "abcde abcdefg"` and
       `ContainsAllTokens "abcdefg"` match no row;
     - `ContainsAnyToken "abcde abcdefg"` matches the first row;
     - `ContainsAllTokens "x abcdefg"` with `last_as_prefix` matches no row, and
       `ContainsAnyToken` with it matches both;
     - a BM25 leg `"abcdefg"` has no hits; with `last_as_prefix` it has none either; and
       `"x abcdefg"` with `last_as_prefix` scores both rows by BM25(`x`) alone.
4. **Encoding.**
   - `encode` then `decode` round-trips every tokenizer, with and without a limit.
   - A `WordV1` line with no limit equals today's bytes.
   - `decode` refuses `v2;...;word_v1`, `v3;...;pre_tokenized_array;5`, and `v3` limits
     `0`, `256`, `05`, `+5`, `5.0` and `x`.
   - The parent's refusal is an observation, recorded in VERIFIED.md: the parent commit's
     `decode`, run on a `v2;...;word_v3` line and a `v3` line that the new `encode` wrote,
     returns `None`.
5. **HTTP.** `tokenizers_over_http`, in `crates/pstore-server/tests/it/`:
   - a query `"東京"` over a `word_v3` index returns the row `"東京タワー"` with a score
     above 0; the same query over a `word_v1` index returns nothing; `"東京タワー"` over the
     `word_v1` index returns it;
   - `word_v2` finds that row by `"京"` alone;
   - an emoji query finds the emoji row under `word_v2`, and finds nothing under `word_v1`;
   - `max_token_length` drops a long token from both the document and the query;
   - the index GET reports both settings;
   - redeclaring a different tokenizer is refused;
   - refused `400`: tokenizer `word_v5`, `max_token_length` of `0`, `256`, `1.5` or `"5"`,
     `max_token_length` of `5.0`, `null` or `-1`, and `max_token_length` under
     `pre_tokenized_array`.
6. **Unicode version.** `tokenizer_tables_are_unicode_16` asserts the two version constants
   are `(16, 0, 0)`, that `pstore-format/Cargo.toml` pins both crates with `=`, and that the
   property ranges are read: U+1F600 is `Extended_Pictographic` and U+0041 is not; U+3007 is
   `Ideographic` and U+30BF (`タ`) is not; U+1F1E6 is `Regional_Indicator`.
7. **Gates:** fmt, clippy, the workspace tests (the two IPv6 tests NOT-RUN in this
   container), `cargo deny check`, and the named mutations below each observed red.

## Test plan
Each test is seen red first: the new tokenizers and the option are refused, or undefined,
on the parent.

| AC | Mutation |
|---|---|
| 1 | `WordV2` treating ideographs as alphanumeric; emoji dropped under `WordV2`; a grapheme split inside an emoji; `WordV3` keeping punctuation segments; `WordV3` dropping emoji |
| 2 | case or stemming skipped under a new tokenizer |
| 3 | the limit counted in bytes; the limit counted in graphemes; `>` as `>=`; the dropped token still counted in the fieldnorm; `plain` dropping the limit; an over-limit prefix read as no constraint; an over-limit `All` token ignored |
| 4 | `v3` written without the limit; a `v2` line for `WordV1`; `decode` accepting a `v3` line for `pre_tokenized_array`; `decode` accepting a non-canonical limit |
| 6 | (a table version moved: checked by the assertion itself) |
| 5 | the option ignored at the door; the tokenizer not stamped |

## RA budget
W 0. Rseq 0. Rpar 0. List 0. Tokenization is CPU, at write and query time, and no request
changes.

## Risks
- `unicode-segmentation` is not the vendor's library, so token streams may differ at the
  edges the vendor fixed in `word_v4`. Golden streams state pstore's tokens, not the
  vendor's, and the comparison says so.
- Thai, Lao, Khmer and Myanmar have no dictionary segmentation. UAX #29 leaves their letters
  out of `ALetter`, so `word_v3` splits them about one segment per consonant, with its marks
  attached. `word_v0` to `word_v2` keep runs of alphanumerics, where combining vowels may
  break a run. That is stated in `docs/deploy.md`, and is not repaired here.
- A Unicode bump in the toolchain or either crate changes token streams that existing
  segments were written with. AC6's test fails on it, and the remedy is a new tokenizer
  version, never a silent upgrade.
- A mixed-version fleet: declare a new tokenizer or a limit only once every server runs M181,
  as M158 states for its own.

## Tasks
- **M181.1** The spec, reviewed.
- **M181.2** The tokenizers, the limit, the encoding, HTTP, tests, docs.
