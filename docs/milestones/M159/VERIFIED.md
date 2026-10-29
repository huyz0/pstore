# M159 verification

The spec was reviewed in two rounds before any code.
- **Round 1** had six majors: `fragment_limit: 0` refused though the vendor allows it; CJK
  terminators that split only before whitespace, which CJK prose rarely has; no candidate for
  a long unit with no token; `none` cutting a marked token unstated; what a match range spans
  (the token alone, or with its combining marks); and AC1's data unable to kill "a zero-score
  window taken". All six were fixed, with the minors on trimming, `null`, the vendor's
  unpublished defaults and the section caps.
- **Round 2 passed.** Its minors were applied: a value with no unit after trimming gives no
  `$highlights`; 0 giving `[]` is pstore's reading; the limit-0 rule precedes the fallback;
  `! ` and `? ` are tested; the caps overrun is recorded in the spec.

How to run the tests: `cargo test -p pstore-server --test it highlight::`

Red run: on the parent, with the tests alone, all seven new tests failed (the new keys are
refused there as unknown highlight fields, and `$highlights` does not exist). M63's eight
tests passed before and after.

Each criterion's power to fail rests on the mutations below. Each was applied by a script,
the `highlight::` tests run, and the mutation reverted. All 29 were observed red. No
`cargo mutants` run is claimed for them.

1. `structured_word_fragments_are_ranked_and_disjoint`: green after. Killed: rank by start;
   overlapping windows taken; a zero-score window taken; ties to the later; text order.
2. `structured_sentences_and_paragraphs_are_units`: green after. Killed: no split after `.`,
   after `!`, after `?` (three mutations); a split after `.` without whitespace; `。` needing
   whitespace; `paragraph` splitting at `. `; `\r\n` not a break; no trim; `none` seeking
   the densest window; a cut token unmarked.
3. `structured_offsets_count_in_the_asked_unit`: green after. Killed: UTF-16 counted as
   scalars; codepoints counted as bytes; ranges relative to the fragment; a match range
   without its combining mark; a match range not clipped at a cut (the same mutation as the
   cut-token one in 2).
4. `structured_highlights_read_string_arrays`: green after. Killed: `array_index` from the
   rank; offsets shifted by the element; a non-string element stopping the walk. A scalar
   string's missing `array_index` is pinned by the exact `{"text": ...}` matches of 1 to 3.
5. `structured_highlights_replace_the_string`: green after. Killed: `$highlight` set beside
   `$highlights`; the switch reading only `fragment_by`.
6. `structured_highlights_are_refused_at_the_door`: green after. Killed: each refusal removed
   (`fragment_by`, `include_offsets`, the limit's bound, `rank_fragments_by` accepted); the
   bound 16 moved to 17.
7. `a_structured_highlight_costs_nothing`: green after. No mutation applies: the structured
   path issues no request to remove. It compares a cold query's depth, blob reads and bytes
   with and without a structured highlight.
8. Gates: NOT-RUN in full here. `cargo fmt --check` and `cargo clippy -p pstore-server
   --all-targets --all-features -D warnings` pass. The workspace suite's two IPv6
   `peer_http::response_tests` cannot run in this container (no IPv6), as on every parent.
   The mutation sweep is CI's PR shards.

M63's `window` became `windows` (every window) and `best` (the earliest of the most marked),
and its span building became `spans`. M63's tests pass unchanged, and code review traced the
two as equivalent.

Code review round 1: its one major, that `include_offsets` accepted any value, was read while
the mutation script had that refusal removed. The restored tree refuses it, and the test
asserting so passes. Its minors were applied (a stray doc line; `docs/deploy.md` now says 0
gives `[]` and that a sentence ends at whitespace holding a line feed) or recorded: offset
conversion is O(fragments × length), bounded by 16 fragments.

**Amendment (PR #14's mutation shards 10 and 11).** Three mutants in `Highlight::candidates`
survived: `>` to `>=` and `-` to `+` in the test choosing whether to cut a window, and `>` to
`>=` in `total > max_chars` for a unit with no token. All three are equivalent.
A window's characters, combining marks included, are counted the way `cut` counts them, so
`cut` leaves any window that fits whole, and the condition only skipped a call that changed
nothing. The same holds for a tokenless unit that fits. Both conditions were removed, so `cut`
is now always called, and the `highlight::` tests pass unchanged.

Shard 4 timed out `+=` to `*=` in `windows`: the window's end was stepped in a `while` loop,
which never ended once its step was broken. A timeout is not a kill (`scripts/mutants.sh`), so
the step is now a count of the tokens that fit, which is finite under any mutation. With
`*=` applied locally, the `highlight::` tests fail rather than hang.

Shards 6, 7 and 9 missed five mutants that no case reached, and
`structured_sentences_and_paragraphs_are_units` now covers each one:
- `<` to `<=` in `units`, which kept an empty unit at a leading break and made it the earliest, unmarked fragment. The new case is `"\n\nq a"` by paragraph with no mark.
- `<` to `<=` on either side of the overlap test, which skipped a unit that only touches another. The new cases are `"q。q q"` and `"q q。q"`, so each order is covered.
- `to - from` to `to + from` in `cut`, reached on every window since the change above.
- (shard 6) `+` to `*` in a break's end, equivalent while the whitespace is one byte. The new case is `"q\n\u{3000}\u{3000}b q"`, which holds wide spaces.

Each mutant was applied locally and the `highlight::` tests then failed.
