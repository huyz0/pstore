# M153: one query over several text fields

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"). [M151](../M151/SPEC.md) put several text fields in one
segment and [M152](../M152/SPEC.md) let an index declare them, but a query still reads one: its
text legs must name one field (M151's "one field per query"). The vendor's `Sum` and `Max` of
BM25 clauses over different attributes need one query to score several. This milestone
lifts that refusal in the query layer and the engine. M155 adds the HTTP declaration and the
request syntax. No D- or OQ- id covers it. D-30 (corpus statistics ride in the dictionary
sidecar) and D-34 (the round-trip budget) constrain it.

## Delta
Today:
- `pstore_query`'s merged path takes one `FullText`, which analyzes and scores every text leg;
- `text_field_of` refuses text legs over different fields (`QueryError::Unimplemented("text
  legs over different fields")`) in `run`, `open_part` and `part`;
- `Opened` decodes the text index of the query's one field, and `run` sums one `Stats` for it;
- the engine passes one scoring `FullText`, the queried field's (M152).

After:

**The query layer.**
- `pub struct Texts { pub default: FullText, pub fields: BTreeMap<String, FullText> }`, with
  `Texts::of(field)`: `fields[field]` when present, else `default`. `From<FullText>` gives one
  with no `fields`.
- New `query_with_texts`, `query_rows_split_texts` and `query_rows_filtered_texts` take
  `&Texts` where their namesakes take `&FullText`. The namesakes and `query` delegate through
  `Texts::from` and are otherwise unchanged. **They still refuse text legs over different
  fields**, as M151 has them: one `FullText` describes one field, and a caller that passed one
  has said nothing about how another is analyzed or scored. M151's tests of that refusal stand.
- **Each text leg is analyzed and scored under `texts.of(its field)`**, against **its field's
  statistics**:
  - the statistics are a `Stats` per queried field, each summed over the segments carrying
    that field;
  - each sum is over that field's own analyzed query terms only (M119);
  - a leg's `df`, `doc_count` and `total_tokens` are its field's, never another field's or a
    blend.
- `Opened` keeps the decoded index of **each** queried field the segment carries, decoded
  once at open.
- **The wrong-field refusal applies per field.** `run` refuses (`UnknownField`) when some
  segment carries text and some queried field is carried by none. A segment without one of
  the fields answers that field's legs with nothing, and adds nothing to its statistics, as
  M151 has it for one field.
- **Phased shares stay single-field.** `open_part`, `part` and `scan_part` keep one `FullText`
  and one `Stats`, and still refuse legs over different fields, since a peer's `Part` carries
  one of each. `run` refuses (`QueryError::Unimplemented("a phased share of several text
  fields")`) a query that both names several text
  fields and was handed a phased share: no caller does (see the engine below), and a split
  answer would be scored by the wrong statistics. A one-exchange share (M54) carries vector legs
  only, and the coordinator runs the text legs under per-field statistics, so it is accepted.
- With one queried field, every request, byte and score is what M152 produces.

**The engine.**
- It builds the `Texts` from its schema: `default` is `fts`, and `fields` is `texts` (M152).
  This happens on the live path, from the fresh view's own `texts` before the first fold, and
  `as_of` from that epoch's schema.
- The filter is still bound by `fts`, and `Answer.fts` is still `fts`.
- **Split only a query that one `Part` describes.** A query is given shares only when its
  text legs name at most one field, and that field scores under `fts`. Otherwise it runs every
  segment itself, as M152 does for a further field.

**Unchanged:** the segment format, HEAD, the write path, every request and round, the peer
wire, and the server. The server sends one field's legs until M155.

## Acceptance criteria
1. **Fused as the fields' own rankings.**
   - Over M152's `multi`, with fields `text`, `title` and `tags`, a query's text legs name two
     or three of the fields.
   - Under `Sum { weights }`, every hit's score equals the weighted sum of its BM25 score in
     each leg, to 1e-5. Each leg score is taken from that leg alone over the field's twin index,
     unlimited. The ranking is by that sum, descending, ties by `(segment, row)` as `fuse`
     breaks them.
   - Under `Max { weights }`, each score is the largest weighted leg score.
   - Under RRF, each score is `Σ w / (k + rank)`, with `rank` the row's position in that leg's
     own ranking at the leg's limit.
   - Each holds before the first fold (the fresh view), after a fold, beside fresh rows, after
     a compaction and `as_of` the first fold.
   - Every leg's limit is at least `top_k`, so a row's best leg has ranked it, and the queries
     are chosen so that no two expected scores in the top `top_k` are within 1e-5 of each
     other.
   - A query over one field gives the same ids, order and score bits as over the field's twin
     (M152 compared them to 1e-6).
2. **Statistics per field.**
   - Two segments differ in which fields they carry: the first carries `a` and `b`, and the
     second `a` alone. Some row of the first segment has no `b` text.
   - `b` has a non-default `FullText`: English stemming, and `k1` and `b` other than the
     default. The query over `b` is "Running", against rows holding "run".
   - A `Sum` query over `a` and `b` scores each leg with exactly the statistics a query over
     that field alone computes. `b`'s `doc_count` is the first segment's row count, since a
     dictionary counts every row of its segment, and `a`'s is both segments' row counts.
   - This is checked through `pstore_query` directly, against single-field queries, score bits
     equal.
3. **Refused.**
   - A query naming a field no segment carries, beside a field some segment carries, is
     `UnknownField`, through `query_rows_filtered_texts` and through the engine. Through
     `query`, the same legs are `Unimplemented("text legs over different fields")`, as M151
     has it.
   - `open_part` and `part` still refuse legs over two fields, before any request.
   - `query_rows_split_texts` with a phased share and legs over two fields is refused before
     any request, with its own message: `Unimplemented("a phased share of several text
     fields")`. The test checks that message.
4. **Not split.** Through `split.rs`'s phased shares, a `Sum` query over the primary and a
   further field makes no peer call, and answers exactly as the same query unsplit. This holds
   for a further field declared with a non-default `FullText`, and for one declared with
   `FullText::default()`, equal to the primary's, which M152's guard alone would split. A query
   over the primary alone is still split.
5. **Round trips.** A `Sum` query over fields `a` and `b` of one segment costs the same
   sequential rounds as a query over `a` alone.
   - Its requests are `R(a) + R(b) - R(open)`. `R(f)` is a query over `f` alone, and
     `R(open)` is a query over a term no row holds, which opens the segment and reads no
     postings. So the footer, the delete vector and the sidecar are fetched once.
   - It reads each field's postings and norms bytes once, and the sidecar once.
   - Measured on the ranking alone, through `query_with_texts` and `query_with`, which fetch no
     rows: a row-fetch round's requests count distinct blocks among the hits, which do not add
     across queries.
   - Asserted with `pstore-blob`'s byte accounting and the testkit's depth counter, over a store
     with no cache in the stack.

## Test plan
Red first against the parent:
- 1, 2, 3's first and third points, 4 and 5 fail: the parent refuses text legs over different
  fields, and has no `_texts` entry points.
- 3's second point is green on the parent, and is a guard.
- 1's single-field point is a guard too.

VERIFIED.md rests each test's power on these mutations, each observed red:
- 1: `crates/pstore-engine/tests/it/multi_text.rs`. Mutations:
  - "every leg scores under `default`";
  - "every leg reads the first field's statistics";
  - "the fresh view's texts are not passed";
  - "`as_of` passes no texts".
- 2, 3: `crates/pstore-query/tests/it/multi_text.rs`. Mutations:
  - "the statistics are summed over every segment, carried or not";
  - "a field's terms are analyzed under `default`";
  - "a field no segment carries answers nothing";
  - "`run` accepts a phased share beside several fields".
- 4: `split.rs`. Mutations: "a multi-field query is split" (several fields under `fts` are
  split), "a primary query is not split".
- 5: `crates/pstore-query/tests/it/multi_text.rs`. Mutation: "each field reopens the sidecar".

## RA budget
Unchanged in rounds and requests:
- the sidecar is one object per segment, fetched in the open round whatever the field count
  (M151);
- every leg's postings are ranged reads in the legs' round, as today.

The new cost is CPU and memory: one decoded dictionary and one `Stats` per queried field,
bounded by M152's cap of 8 fields.

## Risks
- **A multi-field query is never phased.** It costs one server the whole query, as a
  further field does under M152. [M154](../M154/SPEC.md) carries a `FullText` and a `Stats` per
  field on the peer wire, and lifts both.
- **The filter analyzer is the primary's.** A token filter over a further field still analyzes
  with `fts` (M152's risk). M155 owns per-attribute declarations and their filters.

## Tasks
- M153.1: `Texts`, the per-field index and statistics in `run`, the new entry points, and the
  refusals (2, 3, 5).
- M153.2: the engine's `Texts`, and splitting only a query one `Part` describes (1, 4).
