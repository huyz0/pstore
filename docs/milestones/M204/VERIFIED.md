# M204 verification

The spec was reviewed once before code: one blocking finding (a build before M204 would
read a French-with-stopwords schema line and drop English's list from French text during a
rolling deploy), applied as the `v4:` line of Delta 5; three majors (German `daß` uppercases
off the list; nothing caught a list gaining a word; `cargo deny` cannot check a copied
notice), all applied; minors on the mutation for English, the query side, and NFC, applied
in Delta 4's wording.

How to run the tests: `cargo test -p pstore-format --test it stopwords::` and
`cargo test -p pstore-server --test it analyzer::`.

Red runs. `french_stopwords_over_http` was run against the parent's `pstore-format` and
`pstore-server` sources (this milestone's stashed) and failed: the parent refuses French
with stopwords. The format crate's tests cannot compile against the parent
(`Language::stopwords` does not exist), so they were seen red by the mutations below.

Each named mutation was applied by a script, the tests above run, and the mutation
reverted. No `cargo mutants` run is claimed for them. All were killed.

1. `stopwords_drop_each_languages_list`: green, twelve golden sentences, each written by
   hand against the language's list, unstemmed and stemmed, documents and queries. Killed:
   French mapped to Spanish's list (N1); stopwords compared after stemming (N2, by Spanish
   `estaba`, whose stem `estab` is not on the list).
2. `every_list_word_is_a_stopword`: green, every list word dropped alone, folded and not,
   uppercase too (but `daß`), kept under `case_sensitive`; the twelve distinct counts and
   the BSD notice. Killed: Danish losing `og` (N3); Danish gaining `hund` (N4); the raw,
   unlowered token compared (N5).
3. `english_stopwords_are_lucenes`: green. Killed: English mapped to Danish's list (N6).
4. `folded_stopwords_match_folded_tokens`: green. Killed: the unfolded list compared under
   `ascii_folding` (N7).
5. `stopword_lines_are_refused_by_older_grammars`: green over 18 languages, both flags and
   three tokenizer/limit lines. Killed: the `v4:` prefix not written (N8); a bare inner line
   accepted (N9).
6. `french_stopwords_over_http`, and `what_an_analyzer_cannot_mean_is_refused` with its
   refused case moved from French to Arabic (French is now valid; the refusal is kept, not
   weakened): green. Killed: the English-only refusal kept (N10).
7. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. `cargo test --no-fail-fast` over pstore-format, -index, -query,
   -engine and -server, skipping the two IPv6 tests (NOT-RUN in this container, no IPv6):
   2,144 passed, none failed. `cargo deny check`, coverage and the mutation sweep are
   NOT-RUN here; they are CI's on the PR.

Code review, round 1: pass, with three minors. Fixed: `docs/deploy.md` said an older build
refuses a `v4:` line; it does in a HEAD, but may read a row's stamp as no declaration, as
M181's lines. Kept: an engine-level Arabic analyzer with stopwords encodes a line `decode`
refuses (HTTP refuses it first); `is_stopword` finds its set once per token, not per call.
