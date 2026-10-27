# M156: token filters over a further text field

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"). [M152](../M152/SPEC.md) recorded the risk and
[M155](../M155/SPEC.md) made it reachable over HTTP: a token filter over a further text field
(`ContainsAllTokens`, `ContainsAnyToken`, `ContainsTokenSequence`, and M138's
`last_as_prefix`) analyzes with the primary's analyzer. So `["title", "ContainsAllTokens",
"Running"]` on a `title` declared with English stemming does not match a `title` of "run",
though a `title` leg ranks it. No D- or OQ- id covers it.

## Delta
Today `Predicate::bound(analyzer)` binds every token predicate in a filter to one analyzer,
and every bind site passes the index's primary `fts.analyzer`:
- the fold's `Conditions::new` (write conditions, `delete_by_filter`, `patch_by_filter`);
- the ranked query live (`answer`) and `as_of`;
- the ordered query (`rank_by` an attribute) and the aggregation, through `Scope`;
- a peer's `part` and `scan_part`, from `Part.fts` (`open_part` binds no filter).

After:
- **`pstore_query::Predicate::bound_texts(&self, primary: &Analyzer, texts:
  &BTreeMap<String, FullText>)`.** A token predicate over an attribute named in `texts` is
  bound to that field's analyzer; any other token predicate to `primary`. `bound(a)` is
  `bound_texts(a, &BTreeMap::new())`, unchanged.
- **Every engine bind site but a peer's** calls `bound_texts` with the index's `texts`, read
  where its `fts` is read: the fold's (from the HEAD's schema or the creating fold's), the
  live query's (the fresh view's own before the first fold), `as_of`'s (that epoch's schema),
  and `Scope`'s, which gains `texts`.
- **A peer's bind is unchanged**, from `Part.fts`, because a `Part` carries one `FullText`.
  So **a query whose filter holds a token predicate over a further field is not split**:
  `one_part` (M153) also refuses shares when the filter names, in a token predicate, a field
  in the index's `texts`. A new `Predicate::token_fields()` yields the attribute of each token
  predicate in it, through `And`, `Or` and `Not`.
- A token predicate over an attribute that is neither the primary nor a further field binds
  to the primary's analyzer, as today.

**Unchanged:** every format, wire and request; the server; a filter with no token predicate
over a further field, whose bind and split are byte for byte today's.

## Acceptance criteria
1. **Filters as its own index.**
   - Index `multi` declares `title` with `{"language": "english", "stemming": true}` and
     holds rows with and without `title`. Its primary `text` is declared with the default
     analyzer and holds other words. Twin `twin_title` holds the same ids with `text` set to
     `title`, declared with `title`'s options.
   - For each of `ContainsAllTokens "Running reports"`, `ContainsAnyToken "Running"`,
     `ContainsTokenSequence "running late"`, and `ContainsAllTokens` with `last_as_prefix`
     over "Running repo": the filter over `title` on `multi` admits the same ids as the same
     filter over `text` on `twin_title`. Each filter admits some row and refuses some row
     holding `title`. **Each admits a row the primary's analyzer would refuse**: a `title` of
     "run report", "run", "run late again" and "run reporting" respectively, holding "run" and
     not "running". The test asserts that row is admitted.
   - Each is checked through a `rank_by: ["id", "asc"]` query, a `vector` query, and a `Count`
     aggregation, before the first fold, after it, and `as_of` the first fold after a second
     write, **folded**, changes some `title` so that the live and `as_of` answers differ.
   - The same filter wrapped in `Not`, and inside `Or` with a filter over `text`, admits the
     ids the twin's equivalent does. (On the twin, the `text` half becomes a filter over an
     attribute `body` holding `multi`'s `text`. That half binds under the twin's stemming
     analyzer there and the default one on `multi`, so its words are chosen to analyze alike
     under both: no stopword, and unchanged by English stemming, such as "zebra".)
2. **Through writes.** On `multi`, before and after the first fold:
   - `delete_by_filter` with `["title", "ContainsAnyToken", "Running"]` deletes exactly the
     rows whose `title` holds "run" or "running";
   - `patch_by_filter` with the same filter patches exactly those rows;
   - an upsert whose `upsert_condition` is `["title", "ContainsAllTokens", "running"]` applies
     to a row whose `title` is "run" and skips one whose `title` is "walk".
3. **Not split.** Over `split.rs`'s peers, on an index whose creating write declares a
   further field and which holds enough folded segments to be split, each of two
   queries makes no peer call and answers as unsplit when its filter holds a token predicate
   over the further field, at the top level and under `Not`:
   - a vector-only query, which today splits through one-exchange shares (`Share::Hits`):
     no `part` call;
   - a query with a text leg over the primary, which today splits through phased shares: no
     `open` and no `scan` call.

   With the same filters over the primary instead, each is split, as today.
4. **The primary is unchanged.** A token filter over `text` on `multi` admits the same ids as
   over `text` on an index declaring the same primary and no further field.

## Test plan
Red first against the parent:
- 1's and 2's stemmed cases fail: the parent binds `title`'s filter to the primary's
  analyzer, which does not stem.
- 3's further-field cases fail: the parent splits them.
- 4, and 3's primary case, are guards.

VERIFIED.md rests each test's power on these mutations, each observed red:
- 1: "`bound_texts` binds every predicate to `primary`"; "the live query passes no `texts`";
  "the fresh view's `texts` are not read before the first fold"; "`as_of` passes no `texts`";
  "`Scope` carries no `texts`" (its `as_of` branch included); "`Scope`'s fresh view passes empty `texts`" (the ordered and
  aggregation queries before the first fold); "`Not` and `Or` bind their children to
  `primary`".
- 2: "the fold's `Conditions` take no `texts`".
- 3: "`one_part` ignores the filter"; "`token_fields` does not look under `Not`"; "a filter
  over the primary is not split".

The tests live in `crates/pstore-server/tests/it/text_filters.rs` (1, 2, 4) and
`crates/pstore-engine/tests/it/split.rs` (3), with a unit test of `bound_texts` and
`token_fields` in `crates/pstore-query/src/filter.rs`.

## RA budget
Unchanged in requests, bytes and rounds. The bind analyzes each token predicate's text once,
as today, under its field's analyzer.

## Risks
- **Not split.** A query filtering on a further field's tokens runs on one server, as a
  further-field leg does, until [M154](../M154/SPEC.md) carries per-field analyzers in a
  `Part`.
- **An undeclared attribute** binds to the primary's analyzer, as today. Turbopuffer refuses a
  token filter on an attribute without `full_text_search`; that refusal is a separate change.

## Tasks
- M156.1: `bound_texts` and `token_fields` (unit test).
- M156.2: every engine bind site, `Scope.texts`, and `one_part`'s filter check (1 to 4).
