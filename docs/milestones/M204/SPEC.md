# M204: stopwords beyond English

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Golden token
streams cover every supported tokenizer/language/config combination ... neither side's
disputed stopword default is inferred." The last open item of
[DELIVERY.md](../../comparisons/DELIVERY.md)'s E row "stopwords beyond English pending". No
D- or OQ- id covers it.

Today `full_text_search.remove_stopwords` is refused for any language but English (M14),
whose list is Lucene's 33 words.

## Delta
1. **`remove_stopwords` is accepted for twelve more languages**: Danish, Dutch, Finnish,
   French, German, Hungarian, Italian, Norwegian, Portuguese, Russian, Spanish and Swedish.
   Each drops the words of **the Snowball project's `stop.txt` for that language**, as
   tantivy 0.22 generated them (`tokenizer/stop_word_filter/stopwords.rs`), copied verbatim
   into `pstore-format` with Snowball's BSD copyright notice.
2. **English is unchanged**: Lucene's 33 words, byte for byte. An index stamped English with
   stopwords tokenizes as it did; nothing is retokenized.
3. **Still refused** for Arabic, Greek, Romanian, Tamil and Turkish, which Snowball ships no
   list for. The refusal names the languages that have one.
4. **Where in the pipeline**, as English's: after case and folding, before stemming. A list
   word is compared as the token is, so it is **folded the same way** when the analyzer folds
   (`ascii_folding: true` drops French `à` as `a`, and `était` as `etait`), and matched as
   written when it does not. `case_sensitive: true` keeps `Le`, as it keeps English's `The`.
   One list entry, Hungarian `ill.`, holds a `.` and so can never match a token, and German
   `daß` uppercases to `DASS`, which is not on the list. The comparison is on the text as
   written: no NFC step, so without folding a decomposed `était` is kept. Document and query
   go through the one `tokens` pipeline (M63), so both sides drop the same words.
5. **A new schema line, so an older build refuses it.** A build before M204 decodes
   `french;x1xx` today and would drop English's list from French text, silently, during a
   rolling deploy. So a non-English analyzer with `remove_stopwords` is encoded as
   `v4:` followed by the line it would otherwise have (`v4:v1;french;0100;...`), which no
   earlier grammar reads, as M158 and M181 bumped theirs. Every other analyzer's line is
   byte for byte what it was, English with stopwords included. `decode` reads a `v4:` line
   only when its inner line is non-English with stopwords, and refuses such an inner line
   bare, so each analyzer has one spelling.
6. **Not inferred**: the default stays `remove_stopwords: false` for every language, ours
   since M14, whatever another product's default is.

**Not changed:** tokenizers, stemmers, BM25, English's list, the wire's option names.

**Not covered:** custom stopword lists; lists for the five languages above.

## Acceptance criteria
1. **Golden streams.** `stopwords_drop_each_languages_list`, in
   `crates/pstore-format/tests/it/`: for each of the twelve languages, a fixed sentence's
   terms under `remove_stopwords` (with and without stemming) equal a golden list written in
   the test, and the same sentence without it keeps those words.
2. **Every list word is dropped.** `every_list_word_is_a_stopword`: each word of each list
   (but `ill.`) analyzes to no term, alone, with and without `ascii_folding`, and its
   uppercase form is dropped unless `case_sensitive` (but `daß`). Each list's distinct-word
   count is pinned: da 94, nl 101, fi 229, fr 154, de 231, hu 198, it 279, no 172, pt 203,
   ru 159, es 308, sv 114. And the copied file holds Snowball's BSD notice.
3. **English unchanged.** `english_stopwords_are_lucenes`: the English list is exactly the 33
   words, and a fixed English sentence's terms equal the pre-M204 golden.
4. **Folding.** `folded_stopwords_match_folded_tokens`: French `À était` drops both words with
   `ascii_folding` and without it.
5. **Encoding.** `stopword_lines_are_refused_by_older_grammars`: every language with and
   without stopwords round-trips; a non-English one with stopwords starts `v4:`, the inner
   line bare is refused, and an English one's line equals its pre-M204 spelling.
6. **HTTP.** In `crates/pstore-server/tests/it/analyzer.rs`,
   `french_stopwords_over_http`: an index declared `{"language": "french",
   "remove_stopwords": true}` is accepted; a text query for `le` finds no row while one for
   `chat` does, and its BM25 equals that of `chat` alone; the schema reads back with
   `remove_stopwords: true`. And
   `what_an_analyzer_cannot_mean_is_refused` refuses Arabic with stopwords (the French case
   it held is now valid, so it moves to Arabic).
7. **Gates:** fmt, clippy, the workspace tests (the two IPv6 tests NOT-RUN in this
   container), and the named mutations below each observed red.

## Test plan
Each test is seen red first, against the parent: French stopwords are refused there.

| AC | Mutation |
|---|---|
| 1 | a language mapped to another's list; stopwords dropped after stemming |
| 2 | a list losing a word that occurs once; a list gaining a word; uppercase compared unlowered |
| 3 | English mapped to any Snowball list |
| 4 | the list compared unfolded under `ascii_folding` |
| 5 | the `v4:` prefix not written; a bare inner line accepted |
| 6 | the English-only refusal kept |

## RA budget
Unchanged: an analyzer option, one schema line longer by 3 bytes where it is new. W, R and List are what a write and a query already cost.

## Risks
- **Another product's lists.** A vendor may use other lists; nothing here claims parity
  beyond "Snowball's". A pinned-SDK comparison (B) would show a difference.
- **Folded lists.** Folding two list words to one form is harmless (a set); folding a
  non-stopword onto a list word drops it (French `où`, "where", folds to the list's `ou`, "or"). That is what folding
  before the stopword filter means, and AC4 pins it.

## Tasks
- **M204.1** The spec, reviewed.
- **M204.2** The lists, the analyzer, the server's check, tests, docs.
