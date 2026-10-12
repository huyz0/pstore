# M63 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. No latency is claimed;
depth, requests and bytes are exact.

Commands:
- `cargo test -p pstore-server --test highlight`: 8 tests.
- `cargo test -p pstore-format --test text tokens_spans_cover_what_analyze_returns`
- `cargo test -p pstore-blob -p pstore-format -p pstore-query -p pstore-engine -p pstore-server -p pstore-index`

1. **Terms are marked.** `highlight_marks_the_querys_terms`, with every expected string
   written by hand:
   - under the default analyzer `Running` is not marked for `run` and `RUN` is, punctuation is
     kept byte for byte, `pre` and `post` are the client's (and may be empty), and two text
     legs' terms are both marked;
   - a text leg of stopwords alone marks nothing and is not refused.

   `highlight_follows_the_indexs_analyzer`, under English with stemming, stopwords and
   folding, unfolded and folded: `run` marks `Running` and `runs`, not `ran`; `The` is never
   marked; `café` and a decomposed `cafe` + U+0301 are marked for `cafe` with the accent inside
   the marks; and the query words `running` and `Café` are the terms `run` and `cafe` only under
   that analyzer.
   - Seen red with the query's terms analyzed by the default analyzer (it fails the
     English test, and the `as_of` one).
2. **Fragments.** `highlight_fragments_hold_the_most_marks`, by hand: exactly `max_chars` is
   whole; the first window holding the mark; the densest window over an earlier, sparser one;
   the earliest among equals; from the start when nothing is marked; a token longer than
   `max_chars`, cut, followed by `…`, and marked when it matches; 日本語 counted as three
   characters; a text with no token cut to its first characters.
3. **Where it appears.** `highlight_rides_every_ranked_answer`: absent for a missing or
   non-string attribute; another string attribute named by `attr`; present with
   `include_attributes` false and `exclude_attributes` of the text; present on a boosted
   query's final rows; per sub-query in a multi-query, one without it none.
   `highlight_uses_the_epochs_analyzer`: under `as_of` an epoch before the index was dropped and
   re-created under another analyzer, marked by the old one. Seen red with `Answer::fts` of the
   `as_of` path reporting the default analyzer.
   `the_docs_say_marks_are_not_escaped`: `docs/deploy.md`'s Highlighting section says it.
4. **`analyze` is `tokens`.** `tokens_spans_cover_what_analyze_returns`: under three
   analyzers and eight texts (`İstanbul`, `½`, a decomposed `é`, a capitalised stopword under a
   case-sensitive analyzer, CJK, no tokens at all), the terms of `tokens` are `analyze`'s;
   every span is one maximal alphanumeric run, in order, without overlap; and analyzing a
   token alone gives its term. Seen red with `tokens` absent (compile).
5. **Refusals.** `highlights_are_refused_at_the_door`: every `400` of rule 6, 15 cases, each
   message naming `highlight`, and the limits and `null` accepted.
6. **Cost.** `a_highlight_costs_nothing`: four rounds, and blob reads and bytes equal to the
   same query without it, at one segment and four.
7. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `cargo mutants --in-diff` over the changed source lines, `--no-config`.
     The server's was resumed three times after time limits and edits (`--iterate`).
     - `pstore-format/src/text.rs`, against `pstore-format`'s text and analyzer tests:
       17 mutants, 5 missed, all resolved below.
     - `pstore-server/src/highlight.rs` and `lib.rs`, against `highlight`, `boost` and the
       library's tests: 64 mutants, 6 missed, resolved below, one timeout (an infinite loop
       counts as caught).
   - **The 11 misses:**
     - `extend_over_marks` as `0`, `1`, `end - …`, `end * …` (4): it was exercised only
       through the server. Killed by `extend_over_marks_keeps_an_accent_with_its_letter`.
     - `tokens`' `i < s.len()` as `<=`: ⚠️ equivalent, and the condition was redundant (the
       sentinel space is not alphanumeric). Deleted.
     - `render`'s `first.c_end - first.c_start > max_chars` as `>=`: a token of exactly
       `max_chars` got a spurious trailing `…`. Killed by the `fg abcde` case in
       `highlight_fragments_hold_the_most_marks`.
     - `window`'s long-token test `>` as `==`: a long marked token lost to an earlier unmarked
       window. Killed by the `aa abcdefghij` case in the same test.
     - `aggregation -> Ok(None)`: the one line M63 added to it, `highlight` in its refusal
       list, was unreachable (`plan` refuses first). Deleted; the function mutant is killed
       by `tests/aggregate.rs`, which the sweep did not run.
     - ⚠️ **Three equivalent, not tested:**
       - `window`'s `hi <= i` inverted: the window is then rebuilt from the token instead of
         slid, which gives the same answer more slowly (quadratic in the window's tokens, at
         most 2,000 characters wide);
       - `window`'s long-token `>` as `>=`: a token of exactly `max_chars` takes the same
         window either way;
       - `hi = i + 1` as `i * 1` for a long token: `render` reads only the start token of a
         long window, and the next iteration resets `hi` regardless.

**Code review**, one round:
- Pass with minors. **Fixed:** U+0345 is a combining mark that is also a letter, so extending
  a marked token over it overlapped the next token and marked it twice
  (`extending_over_marks_stops_at_the_next_token`, seen red: "5 overlaps 3..6"), and
  `render`'s unreachable fallback returned the whole text unbounded: it is now the same
  bounded fallback as a text with no token.
- **Noted:** the spec was edited after its review, once the code settled how a cut long token
  ends (`…`) and how the mark extension stops. The code follows the spec as it now stands.
- **Noted:** a result's attribute is tokenized and stemmed in full whatever its size, for
  each of the `top_k` rows: CPU on the request path scales with the bytes of the attributes
  returned, not with any request count. `max_chars` bounds the response, not the work.

**Not covered**, as the spec states:
- phrase or proximity awareness, several fragments a result, and a token-predicate filter's
  terms;
- the text is **not escaped**, by design, and the docs say so.
