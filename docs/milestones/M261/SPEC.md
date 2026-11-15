# M261: the `whitespace` tokenizer

## Serves
The [turbopuffer gap list](../../comparisons/turbopuffer-gaps-2026-11-15.md), "Shipped by the
vendor after the pinned spec": CJK tokenizers and `whitespace`; this milestone is
`whitespace` only. Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md).
No D- or OQ- id covers it.

## Delta
1. **`full_text_search.tokenizer: "whitespace"`**: a token is each maximal run of
   characters that are not Unicode whitespace (`char::is_whitespace`). Punctuation stays in
   the token: `"don't stop."` is `don't`, `stop.`.
2. **Then the usual pipeline** (`tokens`): `max_token_length` drops an over-long token,
   `case_sensitive: false` (the default) lowercases, `ascii_folding` folds. **`stemming` and
   `remove_stopwords` are refused** with the tokenizer named, as the vendor documents.
   `language` is accepted and changes nothing, as no step that reads it can run.
3. **Everywhere a tokenizer applies**: BM25 legs, `ContainsAllTokens`/`ContainsAnyToken`/
   `ContainsTokenSequence`, `last_as_prefix`, highlights (M159), further text fields (M151),
   reindex (M168). A query is tokenized the same way as a document (M63).
4. **Encoding**: the tokenizer's name in the existing `v2`/`v3` line (M181). A build before
   M261 does not parse the name and so refuses the schema, which is the rollback answer
   M181 already gives for its tokenizers. Every other line is byte for byte unchanged.

**No setting.** A tokenizer is already a per-field declaration; nothing changes for a field
that does not declare it.

**Not changed:** every other tokenizer's tokens; the default tokenizer (`word_v1`).

**Not covered:** `smartcn`, `kuromoji`, `nori` (the gap list defers them until a dictionary's
size is measured).

## Acceptance criteria
1. **Golden tokens.** `whitespace_tokens`, in `crates/pstore-format/tests/it/tokenizers.rs`:
   fixed strings (ASCII punctuation, tabs, newlines, U+3000 ideographic space, NBSP U+00A0,
   emoji, a leading and trailing space, an empty string) give golden spans and terms, with
   `case_sensitive` on and off, `ascii_folding` on, and `max_token_length: 3`.
2. **Encoding.** `whitespace_lines_round_trip`: `whitespace` with and without a limit
   round-trips as a `v2`/`v3` line naming it; every pre-M261 tokenizer's line is unchanged.
3. **HTTP.** `whitespace_tokenizer_over_http`, in `crates/pstore-server/tests/it/tokenizers.rs`:
   an index declared `{"tokenizer": "whitespace"}` finds `stop.` for the query `stop.`, not
   for `stop`, and a `word_v1` index finds the same row for both; the schema reads back
   `tokenizer: "whitespace"`; `stemming: true` and `remove_stopwords: true` with it are
   refused 400 naming the tokenizer.
4. **Gates:** fmt, clippy, the workspace tests (the two IPv6 tests NOT-RUN in this
   container), and the named mutations below observed red.

## Test plan
Each test is seen red first against the parent, where `whitespace` is an unknown tokenizer.

| AC | Mutation |
|---|---|
| 1 | split on ASCII space only (NBSP and U+3000 kept inside a token); punctuation stripped |
| 2 | `whitespace` written as a `v1` line |
| 3 | the stemming refusal dropped |

## RA budget
Unchanged: a schema option. W, R and List are what a write and a query already cost.

## Risks
- **Vendor's whitespace set.** "Unicode whitespace" may differ by a code point or two (for
  example U+200B is not whitespace in Rust). The facade's fixtures would show it.

## Tasks
- **M261.1** The spec.
- **M261.2** The tokenizer, the server's refusal, tests, docs.
