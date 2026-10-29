# M159: structured highlights

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "highlights
return structured fragments and all specified offsets, including emoji, combining
characters and string-array indices". The [comparison](../../comparisons/turbopuffer-2026-10-25.md)
records M63's `$highlight` as one marked string. No D- or OQ- id covers it.

The vendor's contract, from [the inventory](../../comparisons/turbopuffer-contract-inventory.json):
`HighlightConfigParams` is `rank_fragments_by`, `fragment_by` (`none`, `sentence`,
`paragraph`, `word`), `fragment_limit` (integer, minimum 0) and `include_offsets` (`utf-8`,
`utf-16`, `codepoints`). A `HighlightMatch` is `{text, fragment_range?, match_ranges?,
array_index?}`. No defaults are published. The vendor reaches it through a computed attribute;
pstore has no compute map (workstream F), so the configuration goes on M63's `highlight`.

## Delta
1. **Request.** `highlight` gains `fragment_by` (default `"word"`, pstore's choice),
   `fragment_limit` (a JSON integer 0 to 16, default 1) and `include_offsets` (absent: no
   offsets). **Naming any of the three makes the highlight structured:** its result carries
   `$highlights` and no `$highlight`. Naming none is M63's, byte for byte.
2. **Answer.** `$highlights` is a list of `{text, fragment_range, match_ranges, array_index}`.
   - `text` is the fragment, marked as M63 marks (same tokens, terms, combining marks inside
     the marks), with **no ellipsis**.
   - Under `include_offsets` only: `fragment_range`, and `match_ranges`, the spans between
     `pre` and `post` in `text` (the token plus the combining marks after it), in order, each
     clipped at the fragment's end. Each is `[start, end)` into the **original value** (the
     element, for an array), never into `text`, in UTF-8 bytes, UTF-16 code units or Unicode
     scalar values.
   - `array_index` only when the attribute is an array.
3. **Units.** A fragment never spans two. `word` and `none`: the whole value. `paragraph`:
   split at any run of whitespace holding a `\n`. `sentence`: paragraphs, further split after
   `.`, `!` or `?` followed by whitespace, and after `。`, `！` or `？` always. A terminator
   belongs to the sentence before it. Every unit is trimmed of leading and trailing
   whitespace (`char::is_whitespace`), so unlike M63 a short `word` value loses its outer
   whitespace; a unit empty after trimming is dropped.
4. **Candidates of a unit** (characters counted as M63 counts them).
   - `none`: the unit's first `max_chars` characters. A marked token the cut splits is
     marked over its kept part, and its match range ends at the cut.
   - A unit of at most `max_chars` characters: one candidate, whole.
   - A longer unit with no token: its first `max_chars` characters, score 0.
   - A longer unit with tokens: M63's windows. `sentence` and `paragraph` take the unit's best
     window only; `word` takes every window. A token longer than `max_chars` is cut as M63
     cuts it.
5. **Choosing.** Score is the count of marked tokens. Rank by score, highest first; ties to
   lower `array_index`, then lower start. Take up to `fragment_limit` candidates scoring at
   least 1, in rank order; under `word` skip one sharing a token with one taken. If none
   scores, the answer is the earliest candidate alone, unless the limit is 0, which gives
   `[]` (pstore's reading: the vendor publishes no meaning for 0). The list is in rank order.
6. **Arrays.** An `Array` attribute is highlighted per string element; other elements are
   skipped. Each element is its own value for units and offsets; candidates of every element
   are ranked together. A value with no unit after trimming (empty or whitespace only,
   scalar or element) has no candidate. No candidate at all, a missing attribute or a
   non-string scalar gives no `$highlights`. M63's unstructured form still marks only a
   string.
7. **Refused `400`**, naming `highlight`: a `fragment_by` or `include_offsets` outside its
   list, `null` included; a `fragment_limit` not a JSON integer 0 to 16 (`17`, `-1`, `1.0`,
   `null`); `rank_fragments_by`, which has no published shape.

**Not changed:** M63's answer and every M63, M155 and M158 refusal; ranking, cost, format and
wire. **Not covered:** Unicode sentence segmentation (`e.g. this` splits);
`rank_fragments_by`; a further text field's terms; a compute map.

## Acceptance criteria
In `crates/pstore-server/tests/it/highlight.rs`, expected values written by hand.

1. `structured_word_fragments_are_ranked_and_disjoint`: limit 3 over a value whose best
   window comes after a weaker scored one gives three disjoint windows in rank order, no
   ellipsis; a tie goes to the earlier; limit 3 over a value with one scored window gives one
   match; no mark gives one match, the first window; the default limit is 1; limit 0 is `[]`.
2. `structured_sentences_and_paragraphs_are_units`: a sentence never crosses `. `, `! `,
   `? `, `\n` or `。`; `3.5` does not split; `paragraph` keeps `. ` and splits at `\n` and `\r\n`; units
   are trimmed; a long unit gives its best window; a long unit of punctuation gives its
   first `max_chars`; `none` gives the first `max_chars` though the marks lie beyond them,
   and marks a token its cut splits.
3. `structured_offsets_count_in_the_asked_unit`: one value with an emoji, a decomposed `é`
   inside a marked token and a CJK character; hand-counted ranges in all three units, into
   the value; none without `include_offsets`; a cut token's range ends at the cut.
4. `structured_highlights_read_string_arrays`: `array_index` present; element 2's fragment
   ranks first; offsets restart per element; a number element skipped; a scalar string has
   no `array_index`.
5. `structured_highlights_replace_the_string`: `$highlights` and no `$highlight` for each of
   `fragment_by`, `fragment_limit` and `include_offsets` named alone; unstructured unchanged;
   a missing or numeric attribute gives neither; per sub-query in a multi-query.
6. `structured_highlights_are_refused_at_the_door`: each `400` of rule 7; 0, 16 and every
   enum value accepted.
7. `a_structured_highlight_costs_nothing`: a cold query's depth, requests and bytes equal
   the same query's without a highlight.
8. Gates: `scripts/gates.sh`, and the mutation sweep of changed lines misses 0 or each miss
   is killed by a named test.

## Test plan
Each test is seen red on the parent (the new keys are refused as unknown, or `$highlights` is
absent). Named mutations, each applied and observed red:

| AC | Mutation |
|---|---|
| 1 | rank by start; overlapping windows taken; a zero-score window taken beside a scored one; ties to the later; text order |
| 2 | no split after `. `, `! ` or `? `; split after `.` without whitespace; `。` needs whitespace; `paragraph` splits at `. `; `\r\n` not a break; no trim; `none` seeks the densest window; a cut token unmarked |
| 3 | UTF-16 counted as scalars; codepoints as bytes; ranges into `text`; a match range without its combining mark; not clipped at a cut |
| 4 | `array_index` from rank; offsets carried across elements; a non-string element stops the walk |
| 5 | `$highlight` set beside `$highlights`; the switch reads only `fragment_by` |
| 6 | each refusal of rule 7 removed in turn; 16 moved to 17 |

Delta (about 45 lines) and the criteria (about 27) exceed the skill's caps of 30 and 20;
spec review judged the rules dense rather than padded, and the overrun is recorded here.

## RA budget
Unchanged: W 0, Rseq 0, Rpar 0, List 0, as M63. Output is bounded: 16 fragments of at most
2,000 characters each, plus marks.

## Risks
- A client reading `$highlight` gets nothing once it names `fragment_limit`. That is the
  switch, stated in `docs/deploy.md`'s Highlighting section.
- The sentence rule differs from the vendor's on abbreviations; only its output could show
  that, so the rule is stated.
- `word` windows with a limit above 1 cost O(tokens × limit), over an attribute in memory.

## Tasks
- **M159.1** The spec, reviewed.
- **M159.2** `parse`, units, candidates, ranking, offsets and `$highlights`, with the tests
  and `docs/deploy.md`.
- **M159.3** The comparison's highlight rows and DELIVERY's E row.
