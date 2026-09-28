# M14 — Full-text analyzer options and token predicates

## Serves

- D-30: BM25 with two-pass IDF, now under a chosen analyzer and chosen `k1` and `b`.
- The corpus's rule that analysis is "stored in the schema, in HEAD, versioned", and that
  changing it is a reindex which must say so
  ([`full-text-search.md`](../../research/06-indexing/full-text-search.md),
  [`api-design.md`](../../research/11-design/api-design.md) § Schema).
- The parity rows for `full_text_search` options and the three token predicates
  ([`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md)).

## The rule this milestone keeps

**An index's analyzer is its HEAD schema's, fixed when that schema is created.** Every
place that turns text into terms reads it from there:
- the seal of a folded segment;
- a compaction, which re-analyzes the stored text;
- a BM25 query;
- a token predicate.

A write only *declares* an analyzer. That sets the schema at the index's first fold, or is
checked against it. **No declaration is the default analyzer**, which is today's:
- split on anything that is not alphanumeric;
- lowercase;
- no stemming, no stopwords, no folding;
- `k1 = 1.2`, `b = 0.75`.

So every existing index, and every HEAD written before M14, reads as it did.

## M14.1 — analyzer options

### Delta

- **`Analyzer`** in `pstore-format::text` holds these fields:
  - `tokenizer`: `word_v1` only, which is today's split;
  - `language`: one of the 18 that the Snowball stemmers in the `rust-stemmers` crate cover,
    default `english`;
  - `stemming`, default false;
  - `remove_stopwords`, default false. Stopwords are the Lucene English set, and asking for
    stopwords with another language is refused;
  - `case_sensitive`, default false;
  - `ascii_folding`, default false. It is NFKD followed by dropping combining marks
    (`unicode-normalization`), so `é` becomes `e`. `ß` and `æ` are not letters with marks,
    and stay as they are.

  `analyze(&Analyzer, &str)` replaces `analyze(&str)`, and the default analyzer gives
  byte-for-byte today's tokens. The order of steps is: split, case, fold, stopwords, stem.
- **`k1` and `b`** are stored beside the analyzer and used by scoring. `k1` must be in
  `[0, 3]` and `b` in `[0, 1]`; values outside are refused.
- **Wire.** The write's `schema` map (M9h.3) accepts `{"<text field>": {"type": "string",
  "full_text_search": true | {options}}}`.
  - `true` means the default analyzer.
  - An unknown option or value is refused.
  - The attribute must be the index's text field, and naming another is refused.
- **Carried to the fold** as a reserved row attribute, `$fts`, as `$metric` is (M9d).
  - `implied()` records the first declared analyzer it sees, or the default.
  - The reject pass drops a row declaring a different analyzer, and counts it in
    `rejected_rows`.
- **At the door**, a declaration that differs from the cached schema is refused as a
  `schema_conflict` naming "the analyzer". Its message says a change is a reindex into a new
  index, and M16's copy is the path.
- **HEAD** gains a trailing section with each non-default analyzer, for live schemas and for
  dropped ones. The mechanism is the one M9d used for metrics, so an older HEAD decodes to
  the default.
- **Query.** `GET /v1/indexes/{index}` reports the analyzer. A BM25 query analyzes its text
  with the schema's analyzer, and scores with its `k1` and `b`. `as_of` a past epoch uses
  that epoch's schema, as the metric does.

**Does not change:** the segment format, the dictionary, the request count of any operation,
or two-pass IDF.

### Acceptance criteria

1. **Default is today.**
   - `analyze(&Analyzer::default(), s)` equals the pre-M14 `analyze(s)` over a corpus of
     mixed-script strings.
   - An index written without a declaration answers a BM25 query exactly as before: the
     same ids, and scores equal within `1e-6`.
2. **Each option applies**, at seal and at query, checked through the API with a small
   judged set per option. Each case has a query that finds a document only because of that
   option:
   - `stemming` (`running` finds `runs`);
   - `remove_stopwords` (`the` alone finds nothing);
   - `case_sensitive` (`Apple` does not find `apple`);
   - `ascii_folding` (`cafe` finds `café`);
   - `language: german` with stemming.

   Each must fail with that option's step removed.
3. **`k1` and `b` apply.** Against an independent BM25 over the same corpus with the
   declared `k1` and `b`, the scores are equal within `1e-4`, where the defaults' scores
   differ by more than `1e-3`.
4. **Fixed at creation.**
   - A second write declaring a different analyzer is `400 schema_conflict`, and the message
     says "reindex".
   - A write declaring nothing is accepted.
   - Two writers racing to create an index with different analyzers leave one in HEAD. The
     other's rows are counted in `rejected_rows`.
5. **Survives compaction and restart.**
   - After compaction, the stemmed query still finds the document.
   - A new process reading HEAD uses the recorded analyzer.
   - A HEAD encoded before M14 decodes with the default.
6. **Refusals.** Each is `400`, and names the rule:
   - an unknown option;
   - a language outside the 18;
   - stopwords with a non-English language;
   - `k1` or `b` out of range;
   - `full_text_search` on an attribute other than the text field.
7. `./scripts/gates.sh` passes, `./scripts/ndcg.sh` passes (the default analyzer's ranking
   is unchanged), and `./scripts/mutants.sh` over the diff misses 0.

### Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `Analyzer` does not exist | any default step changed: lowercase dropped, a split rule changed |
| 2 | `schema` refuses `full_text_search` | each step removed; the query analyzed with the default instead of the schema's |
| 3 | as 2 | the constants used instead of the schema's `k1` and `b` |
| 4 | as 2 | a conflicting declaration accepted; `$fts` ignored by `implied` or the reject pass |
| 5 | as 2 | compaction re-analyzing with the default; the HEAD section not written or not read |
| 6 | as 2 | any refusal accepted |

## M14.2 — token predicates

### Delta

- **Filters** `[attr, "ContainsAllTokens", "<text>"]`, `[attr, "ContainsAnyToken",
  "<text>"]`, and `[attr, "ContainsTokenSequence", "<text>"]`. The text and a string attribute
  are both analyzed with the index's analyzer. Then:
  - `ContainsAllTokens` admits a row whose tokens include every query token;
  - `ContainsAnyToken` admits a row whose tokens include at least one;
  - `ContainsTokenSequence` admits a row whose tokens hold the query's tokens consecutively,
    in order.

  A missing or non-string attribute admits nothing. A query with no tokens admits every
  string for `All` and `Sequence`, and none for `Any`. Those are the vacuous readings,
  stated rather than refused, because the analyzer that empties a query is the index's.
- **Where the analyzer comes from.** The server parses with the default analyzer, and the
  engine binds the schema's before evaluating:
  - on reads, where it resolves the view;
  - on deferred operations, at the fold.

  Unbound is the default, which is correct for every index that declared nothing.
- **No index.** A zone map cannot prune on tokens, so these predicates admit every block,
  and evaluate per row on the attribute already read. This holds in queries, aggregations,
  write conditions and by-filter operations. `condition.rs` gains their tags.

### Acceptance criteria

1. Each predicate equals brute force over 2,000 rows written with a stemming English
   analyzer. That includes `Not` of each, and each inside `And`/`Or`.
2. Binding: on a `case_sensitive` index, `ContainsAnyToken "Apple"` does not match `apple`.
   On a default one, it does.
3. As a write condition and in `delete_by_filter`, a token predicate decides at the fold as
   the query does.
4. Round trip: every token predicate survives `condition::encode`/`decode`.
5. Depth: a filtered query with a token predicate stays within 3 sequential rounds.
6. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

## RA budget

Unchanged for writes, queries and folds. Analysis is CPU over bytes already read.

## Risks

- Stemming quality is the `rust-stemmers` crate's, and no eval set here measures it.
  Criterion 2 shows only that each option does something, not that it helps.
- A token predicate costs a filter scan: nothing prunes it, and no response says so. An
  index for it would be a positions or trigram section (M15), not this milestone.
- Declared-but-absent: a write naming `full_text_search` on a text field no row carries
  still sets the schema, and a later declaration conflicts.

## Tasks

- **M14.1** — analyzer options; criteria 1–7.
- **M14.2** — token predicates; criteria 1–6.
