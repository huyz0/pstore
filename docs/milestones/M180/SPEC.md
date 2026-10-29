# M180: `last_as_prefix` on a BM25 text leg

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("prefix/
token-array queries"). The [comparison](../../comparisons/turbopuffer-2026-10-25.md)'s
`BM25 ... last_as_prefix` row: pstore has no prefix clause on ranking. [M138](../M138/SPEC.md)
gave the filters one and left ranking out, because it "needs a dictionary prefix walk with its
own request budget". No D- or OQ- id covers it.

The vendor's [query reference](https://turbopuffer.com/docs/query), read 2026-10-29: "the
last token in the input string is treated as a literal prefix", so `wal` matches `wal`,
`walrus` and `walnut`; "`BM25` prefix matches are assigned a score of `1.0`"; the option is
`{"last_as_prefix": true}` after the query. No limit is published.

## Delta
1. **Request.** A text leg object may carry `"last_as_prefix": true`:
   `{"field": "title", "query": "lazy wal", "last_as_prefix": true}`. `false` is the leg
   without it. A string leg has no option, so the primary takes it by naming
   `"field": "text"`. `pstore_query::Prefetch::Text` gains `last_as_prefix: bool`.
2. **Splitting the query.** As M138 splits a filter's operand: the last raw token
   (`query_tokens`) is the prefix, as `plain` (M138: the analyzer without stemming or stopword
   removal) makes it. The text before that token is analyzed as today and scored by BM25; the
   coordinator's statistics terms stop before it too. A query with no raw token has no prefix
   and is the leg without the option. Under `pre_tokenized_array` the prefix is the last
   element, whole.
3. **Scoring.** A row's leg score is the BM25 of the other terms, plus **1.0** when the
   segment's term dictionary holds at least one term that starts with the prefix (bytes) and
   that term's postings hold the row. The 1.0 is added once, however many terms match. The
   prefix is not BM25-scored, even when it is itself a term. A row matching only the prefix
   scores 1.0 and is a hit, so a segment holding none of the BM25 terms still walks the
   prefix (`search_with` no longer returns early then). Global statistics (D-30) are
   unchanged and cover only the BM25 terms. Under `Sum` fusion the 1.0 enters the weighted
   sum like any leg score; under RRF only its rank counts.
4. **What a prefix matches.** Dictionary terms, which are analyzed. So a term matches when it
   starts with the prefix's `plain` form **or** with its fully analyzed form (its stem), when
   the analyzer keeps one: under English stemming the complete word `running` meets the
   stored `run`, and `theory` meets `theori`, so adding the option never loses the last word
   itself. A partial word is matched as typed and as stemmed (`runn` stems to itself, so it
   misses `run`), and stopwords
   dropped from the index are never matched. M138's filter matches the row's `plain` tokens
   instead; the two agree for every analyzer without stemming or stopword removal, and
   differ otherwise. That difference is stated in `docs/deploy.md` and the comparison.
5. **Reads.** The dictionary is sorted and postings are written in term order, so each of the
   (at most two) prefix forms' terms is one contiguous run of entries and one contiguous byte
   range of postings, fetched as one range, not a range a term. When one form starts with
   the other, only the shorter is walked: its run holds the other's. Its
   range rides the round the BM25 terms' postings already take (`TextIndex::search_with`):
   **no further round trip**, and at most two ranges per segment however many terms match.
6. **Not split.** A query with a prefix leg is answered by the coordinator, as M153 kept
   several-field queries before M157: `shareable_fields`'s caller refuses to split it. Peer
   protocols are unchanged.
7. **Refused `400`:** `last_as_prefix` that is not a bool; any other unknown key in a leg
   object (unchanged).

**Not changed:** a leg without the option, byte for byte; filters; highlighting, which marks
every analyzed term of the whole query as today (the last token's own term included) and not
the prefix's other expansions; the format.

**Not covered:** scoring by how many expansions match; a cap on expansions (none published);
splitting a prefix leg across peers.

## Acceptance criteria
1. **Oracle.** `a_prefix_leg_matches_the_reference`, in `crates/pstore-query/tests/it/`:
   over three segments of rows from a small vocabulary, for analyzers {default, English with
   stemming and stopwords, case-sensitive}, and queries of 1 to 3 raw tokens, the leg's hits
   and scores equal a reference that computes BM25 over the other terms from global
   statistics and adds 1.0 for any dictionary term of the row with either prefix form.
   Analyzers also include ASCII folding and `pre_tokenized_array`. The leg's limit is at
   least the row count, so ties at 1.0 do not decide the comparison. Scores agree to 1e-5.
2. **Specifics**, by hand: `wal` matches `walrus` and `walnut` with score 1.0 each; a row
   with `wal` twice and `walnut` scores 1.0, not more; `lazy wal` adds BM25(`lazy`); under
   English stemming `runn` matches nothing in `running`, `running` matches it, and so does
   `theory` a row's `theory`; a query of punctuation alone is the leg without the option;
   under the default analyzer `Wal` matches `walrus`, and under `case_sensitive` it does
   not; a prefix-only row in a segment with no BM25 term is still a hit; under `Sum` with
   weights 2 and 1 the prefix's 1.0 is doubled; under English stopwords the prefix `the`
   (its stem form dropped) matches `theory` through the plain form; a row matching through
   both forms scores 1.0 once.
3. **Cost.** `a_prefix_leg_costs_no_round`: on a store with `coalesce_gap` 0, a cold prefix
   query's sequential depth equals the same query's without the option, and its GETs per
   segment are at most two more, with a prefix matching ten terms.
4. **HTTP.** `text_legs_take_last_as_prefix`, in `crates/pstore-server/tests/it/`: the option
   ranks a `walrus` row for `wal`; a non-bool is a `400` naming `last_as_prefix`; `false` is
   byte for byte the leg without it; a split-capable cluster answers a prefix query as one
   server does, and sends no part.
5. **Gates:** fmt, clippy, the workspace tests (the two IPv6 tests NOT-RUN in this
   container), and the named mutations below each observed red.

## Test plan
Each test is seen red first (the option is refused or ignored on the parent).

| AC | Mutation |
|---|---|
| 1, 2 | the prefix scored by BM25 too; 1.0 added per matching term; 1.0 added per form; the prefix kept among the BM25 terms; the walk stops after the first matching term; the stem form not walked; the plain form not walked; the early return kept for a segment with no BM25 term |
| 2 | case folding skipped on the prefix; the prefix's 1.0 left out of the weighted sum |
| 3 | the prefix's postings fetched in a second round; a range a term |
| 4 | the option dropped at the door; a prefix query split |

## RA budget
W 0. Rseq unchanged. Rpar: at most two more ranged GETs per text segment (one per prefix form),
in the round that already fetches postings; bytes grow with the postings of the prefix's terms.
List 0.

## Risks
- A one-letter prefix over a large segment reads most of its postings. Bytes, not requests,
  so it stays within the budget's shape; a cap would change answers silently, so none is set.
- A complete word expands through its stem: `running` reaches `run`, and so `rune` and
  `runway`. That is broader than the vendor's literal prefix, and `docs/deploy.md` says so.
- The vendor's "literal prefix" may be compared before analysis; pstore folds case as the
  analyzer does. A case-sensitive analyzer is the vendor's documented reading.

## Tasks
- **M180.1** The spec, reviewed.
- **M180.2** `Prefetch::Text.last_as_prefix`, the dictionary walk, scoring, HTTP, tests,
  docs.
