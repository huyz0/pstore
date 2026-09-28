# M14 — Full-text analyzer options and token predicates

## Serves

- D-30: BM25 with two-pass IDF, under a chosen analyzer and chosen `k1` and `b`.
- Analysis "stored in the schema, in HEAD, versioned", where a change is a reindex that says so
  ([`full-text-search.md`](../../research/06-indexing/full-text-search.md)).

## The rule this milestone keeps

**An index's analyzer is its schema's, fixed when that schema is created.** Every place text
becomes terms uses it:
1. the seal of a folded segment;
2. a compaction, which re-analyzes the stored text;
3. the fresh segment a query builds over unfolded rows;
4. a BM25 query;
5. a token predicate.

Before the first fold there is no schema. Places 3–5 then use the **first declared analyzer
among the unfolded rows**, else the default, as `fresh_metric` does for M9d.

The **default analyzer** is today's: split on anything not alphanumeric, lowercase, nothing
else, with `k1 = 1.2` and `b = 0.75`.

Every existing index, and every HEAD written before M14, reads as it did.

## M14.1 — analyzer options

### Delta

- **`Analyzer`** in `pstore-format::text` has these fields:
  - `tokenizer`: `word_v1` only, today's split;
  - `language`: one of the 18 that the `rust-stemmers` crate's Snowball stemmers cover,
    default `english`;
  - `stemming`, `remove_stopwords`, `case_sensitive` and `ascii_folding`, all default false.
    - Stopwords are the Lucene English set, so asking for them with another language is
      refused.
    - Folding is NFKD with the combining marks dropped (`unicode-normalization`): `é` becomes
      `e`, and `ß` and `æ` stay as they are.

  The steps run in order: split, case, fold, stopwords, stem. `analyze(&Analyzer, &str)`
  replaces `analyze(&str)`.
  - ⚠️ A `case_sensitive` index keeps `The` past the lowercase stopword list, and Snowball
    does not stem `Running`. That is stated, not repaired.
- **`k1` and `b`** are stored as `f32` beside the analyzer, with `k1` in `[0, 3]` and `b` in
  `[0, 1]`. A declaration is converted to `f32` before it is compared, so re-declaring `1.2`
  equals the stored value.
- **Wire.**
  - The write's `schema` map (M9h.3) accepts `{"<text field>": {"type": "string",
    "full_text_search": true | {options}}}`, where `true` means the default.
  - An unknown option or value is refused, and so is naming an attribute that is not the
    index's text field.
  - `GET /v1/indexes/{index}` reports the analyzer, `k1` and `b`.
- **`$fts`**, a reserved row attribute carrying the encoded declaration, is written for
  **every** declaration, the default included.
  - An **absent `$fts` is "no opinion"**: the door, `row_conflict` and the reject pass accept
    it against any schema.
  - Only `implied()` reads it as the default: the schema takes the first declared analyzer in
    the fold, else the default.
  - A row declaring a different analyzer than the schema is dropped by the reject pass and
    counted in `rejected_rows`.
  - `stripped()` removes `$fts` as it removes `$metric`, so it never reaches a segment or a
    response.
- **The door** checks a declaration against:
  - the cached schema;
  - this process's unfolded rows, the `known` rung of M9d.

  A different one is `schema_conflict` naming "the analyzer". The message says a change is a
  reindex into a new index, and names M16's copy as the path.
- **HEAD** gains a trailing section for every schema, live or dropped, whose analyzer, `k1`
  or `b` differs from the default. It uses M9d's mechanism, so an older HEAD decodes with the
  default.
- **Queries.** A BM25 leg analyzes with the view's analyzer and scores with its `k1` and `b`.
  `as_of` an epoch uses that epoch's schema, including a dropped index's.

**Does not change:** the segment format, the dictionary, any operation's request count, or
two-pass IDF. The parity doc's row, which says "an analyzer is part of the segment format",
is amended: the analyzer is part of the schema, and segments are built under it.

### Acceptance criteria

1. **The default is today.**
   - `analyze(&Analyzer::default(), s)` equals the pre-M14 `analyze(s)` over mixed-script
     strings.
   - An undeclared index answers BM25 queries with the same ids, and scores equal within
     `1e-6`.
2. **Each option applies.** Each case below finds a document only because of its option:
   - `stemming`: `running` finds `runs`;
   - `remove_stopwords`: `the` alone finds nothing;
   - `case_sensitive`: `Apple` does not find `apple`;
   - `ascii_folding`: `cafe` finds `café`;
   - `language: german` with stemming.

   Each is queried **before and after** the first fold, and gives the same answer.
3. **`k1` and `b` apply.** Scores equal an independent BM25 with the declared values within
   `1e-4`, where the defaults' scores differ by more than `1e-3`.
4. **Fixed at creation.**
   - A write declaring a different analyzer is `400 schema_conflict`, both against the
     schema and against unfolded rows. The message names "reindex" and "copy".
   - Declaring `true` on a non-default index is also `400`.
   - A write declaring nothing is accepted **and readable after the fold**, analyzed with the
     schema's analyzer.
   - Two writers racing to create an index with different analyzers leave one in HEAD. The
     other's rows are counted in `rejected_rows`.
5. **Recorded.** Each of these keeps the analyzer and a non-default `k1`:
   - a compaction;
   - a new process;
   - `GET /v1/indexes/{index}`.

   A pre-M14 HEAD decodes with the default. `as_of` before a drop-and-recreate with another
   analyzer uses the old one. `$fts` appears in no response and no sealed row.
6. **Refusals.** Each is `400`, and names the rule:
   - an unknown option;
   - a language outside the 18;
   - stopwords with a non-English language;
   - `k1` or `b` out of range;
   - an attribute other than the text field.
7. `./scripts/gates.sh` passes, including `ndcg.sh`, and `./scripts/mutants.sh` over the
   diff misses 0.

### Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `Analyzer` does not exist | a default step changed |
| 2 | `schema` refuses `full_text_search` | a step removed; the query, or the fresh segment, analyzed with the default |
| 3 | as 2 | the constants used instead of the schema's `k1` and `b` |
| 4 | as 2 | `$fts` omitted for `true`; absent `$fts` read as the default by the door or the reject pass; the `known` rung skipped |
| 5 | as 2 | compaction with the default; the HEAD section not written, or not read, for `k1` alone or for a dropped schema; `$fts` not stripped |
| 6 | as 2 | a refusal accepted |

## M14.2 — token predicates

### Delta

- **Filters.** `[attr, "ContainsAllTokens", "<text>"]`, `[attr, "ContainsAnyToken",
  "<text>"]` and `[attr, "ContainsTokenSequence", "<text>"]`. The text, and a string
  attribute, are analyzed with the index's analyzer, which is **the text field's, whichever
  attribute is named**. Then:
  - `All` admits a row whose tokens include every query token;
  - `Any` admits a row whose tokens include at least one;
  - `Sequence` admits a row whose tokens hold the query's consecutively, in order.
  - A missing attribute, a non-string one, or **an array of strings** admits nothing.
  - A query with no tokens admits every string for `All` and `Sequence`, and none for `Any`.
  - ⚠️ With stopwords removed, `bank of america` matches `bank in america`, because no
    positions are kept.
- **Binding.** A token predicate carries `Option<Analyzer>`, and an **unbound one admits
  nothing**. A missed bind then fails every token test, even on a default index, rather than
  answering as the default. There is one bind per path:
  - for reads, where the view is resolved, with the view's analyzer (place 3's rule before a
    fold);
  - for deferred operations, in `condition_of`, with the fold's schema, `implied()`'s when
    the fold creates it. The fold's `keep` and `resolve` both read through it.
- **No index.** Zone maps admit every block, and evaluation is per row on the attribute
  already read, in queries, aggregations, conditions and by-filter operations.
  `condition.rs` gains their tags, and an unknown tag still decodes as `None`.

### Acceptance criteria

1. Each predicate equals brute force over 2,000 rows under a **stemming** English analyzer,
   where the default's answer differs. This includes `Not`, `And` and `Or`, arrays and
   non-strings, and an attribute other than the text field.
2. The same predicates give the same answer in an aggregation, and before the first fold.
3. As `upsert_condition`, `delete_by_filter` and `patch_by_filter`, a stemmed token predicate
   decides at the fold as the query does, including in the fold that creates the schema.
4. Every token predicate survives `condition::encode`/`decode`.
5. A filtered query with a token predicate stays within 3 sequential rounds.
6. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

### Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the filter is refused | `All` as `Any`; the order ignored in `Sequence`; a query bound to the default |
| 2 | as 1 | the aggregation path or the fresh view unbound |
| 3 | as 1 | `condition_of` unbound, so `keep` never loads the row; the fold binding HEAD's schema instead of `implied()`'s |
| 4 | a tag is missing | a tag or the analyzer dropped in the encoding |
| 5 | as 1 | a token predicate adding a round |

## RA budget

Unchanged for writes, queries and folds. Analysis is CPU over bytes already read.

## Risks

- **Stemming quality is the crate's**, and no eval set here measures it. Criterion 2 shows
  only that each option does something.
- **A token predicate costs a filter scan**: nothing prunes it, and no response says so.
- **A mixed-version cluster erases the analyzer.** `Head::decode` ignores trailing sections
  it does not know, so a pre-M14 process's next commit drops the section, and its
  compaction re-seals with the default. Nothing reveals it except the ranking changing.
  M9d's metric has the same exposure. An upgrade must reach every process before an index
  declares an analyzer.
- **Declared but absent:** a write declaring `full_text_search` on a text field no row
  carries still sets the schema.

## Tasks

- **M14.1** — analyzer options; criteria 1–7.
- **M14.2** — token predicates; criteria 1–6.
