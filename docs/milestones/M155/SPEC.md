# M155: several BM25 text fields over HTTP

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"). The [comparison](../../comparisons/turbopuffer-2026-10-25.md)
lists `full_text_search` as "default text field only; not arbitrary string/string-array
attributes". [M151](../M151/SPEC.md) to [M153](../M153/SPEC.md) built several text fields
below the server: segments carry them, an index declares them (`Declared.texts`), and one
query scores several. This milestone lets a client declare them and query them over HTTP. No
D- or OQ- id covers it.

## Delta
Today:
- `declared_fts` refuses `full_text_search` on any attribute but `text` ("full_text_search
  is on the text field `text` only"), and the server passes `Declared.texts: None`;
- a query's `text` is a string or an array of strings, each a BM25 leg over `text`;
- `GET /v1/indexes/{id}`'s `schema` reports the primary's `full_text_search` only.

After:

**Declaring.** In a write's `schema`, `full_text_search` (`true` or the options object M14
takes) may sit on any attribute name, with an optional `"type"` of `"string"` or
`"[]string"`.
- On `text` it declares the primary's `FullText`, as today.
- On any other name it declares a **further text field** with that `FullText`. The write
  passes every such name in `Declared.texts`; a write declaring none passes `None`.
- One parser reads both, so a further field's options are refused exactly as the primary's
  are (unknown option, out-of-range `k1` or `b`, `remove_stopwords` outside English, an
  unknown language or tokenizer).
- More than 8 further fields, a name beginning `$` or equal to `id`, or a name longer than 128
  bytes, is refused `400` before any write. These are the engine's own refusals
  (`texts_refusal`), and the server reports them as it reports `Refused`.
- A further field that no document of the declaring write carries is accepted, as the
  primary is: a sparse field may first appear in a later write. A declaring write with no
  documents is refused, as the primary's is.
- A further field's declared type other than `"string"` or `"[]string"` is refused, as the
  primary's is. Either accepted type admits a string or an array of strings: the declared
  type is not a check on the values, for the primary today or for a further field.
- In the declaring write, a document whose further field holds something other than a string
  or an array of strings is refused, naming the field, as M150 refuses the primary's.
- A later write that declares a different set, or different options, is refused by the engine
  with `SchemaConflict { what: "the further text fields" }` (M152), which names the reindex
  path. A later write that declares nothing, or the same set, is accepted.

**Querying.** A query's `text` is a string, or an array of 1 to `MAX_LEGS - 1` entries, each
either a string or an object `{"field": <name>, "query": <string>}`.
- A string is a leg over `text`, as today.
- An object is a leg over its `field`. `{"field": "text", "query": q}` is the same leg as `q`.
- An object with another key, without either key, or with a non-string value is refused
  `400`.
- The legs keep request order, so `fusion`'s weights name them as today: dense, then multi,
  then each text leg in order.
- A leg over a field no segment of the index carries, when some segment carries text, is
  refused `400 unknown_field`, **naming that field**. That includes the primary: an index
  whose rows hold `title` and never `text` refuses a string leg, naming `text`. A leg's field
  is any string; `""`, `$x` and `id` are fields no segment carries.
  - Today `run` refuses such a leg with `QueryError::Format(FormatError::UnknownField)`, which
    names no field, and the engine's `query_error` turns it into a generic
    `EngineError::Query`, answered `500`.
  - After, `run`'s text-field check (M151, M153) returns a new
    `QueryError::UnknownTextField(field)`, naming the first queried text field, by name, that
    no segment carries (`text_fields_of` sorts them). The vector checks are unchanged. The four tests that pin
    `Format(UnknownField)` for a query's text leg (`text_field.rs` twice, `multi_text.rs`'s
    line 310 and `several_fields.rs`, all in pstore-query) assert the new variant and the field
    it names: stricter, not weaker. `multi_text.rs`'s line 290 is `TextIndex::from_segment`'s
    format-level refusal, which stays.
  - The engine gains `EngineError::UnknownTextField(field)`, "no segment of the index holds a
    text field `{0}`", which `query_error` maps the new variant to. The server answers it
    `400 unknown_field`, as it answers `UnknownField`. The engine's M153 test
    `an_unknown_field_beside_a_known_one_is_refused` asserts the new variant.

**Reporting.** `GET /v1/indexes/{id}`'s `schema` gains `text_fields`: an object keyed by each
further field's name, whose value is that field's `full_text_search` object in the shape
`full_text_search` has today. It is omitted
when the index has none.

**Highlight.** `highlight` beside a text leg over a further field is refused `400`, naming it:
M63 marks the primary's terms in one attribute, and marking several fields is the plan's
"structured highlights", a later milestone.

**Unchanged:** the engine and the query layer but for the unknown-field error above, every
format and wire, a query whose legs are all strings over an index that carries `text`, and
filters. A token filter over a further field still analyzes with
the primary's analyzer (M152's risk), which [M156](../M156/SPEC.md) resolves.

## Acceptance criteria
1. **One further field ranks as its own index.**
   - One write to index `multi`, with no `text` declaration, declares `title` (`"string"`,
     `{"language": "english", "stemming": true, "k1": 1.5, "b": 0.6}`) and `tags`
     (`"[]string"`, `true`). Its documents hold `text`, `title` and `tags` in varying
     combinations, including rows without `title` and rows with `tags: []`.
   - Twin index `twin_title` holds the same ids, each with `text` set to its `title`, and
     declares `text` with `title`'s options. `twin_tags` does the same for `tags`.
   - A query `{"text": [{"field": "title", "query": q}]}` on `multi` answers the same ids, in
     the same order, with the same score bits, as `{"text": q}` on `twin_title`. Both name
     `"fusion": {"sum": {"weights": [1]}}`, so a score is the leg's BM25 and not RRF's rank. The same holds
     for `tags` against `twin_tags`, and for `{"field": "text"}` against the string form on
     `multi` itself.
   - Each holds before the first fold and after it, for at least two queries per field, one
     of which relies on `title`'s stemming ("Running" against rows holding "run").
2. **Several fields fuse.** On the same indexes, a query whose `text` is `[{"field": "title",
   ...}, {"field": "tags", ...}, q3]` with `top_k` 10 is checked against three twin queries:
   `title`'s on `twin_title`, `tags`'s on `twin_tags`, and `q3` as a string on `multi` itself,
   each under `Sum` with weight 1.
   - Under `"fusion": {"sum": {"weights": [1, 0.5, 2]}}`, each hit's score is the weighted
     sum of its scores in the twin answers, to 1e-5. `Sum`'s legs are whole, not cut at their
     limit (M9g.2), so the twins are unlimited, and a row a twin does not return adds
     nothing. The ranking is the 10 best of those sums, descending.
   - Under `max` with the same weights, each score is the largest weighted twin score. `Max`'s
     legs keep their limit, `top_k`, so the twins are cut at 10.
   - Rows are padded with words no query names, by different moduli, so that no two expected
     scores among the top 11 are within 1e-5 of each other. The test asserts that before
     comparing.
   - Before and after the first fold.
3. **The door.** Each of these is refused `400` before anything is stored, the message naming
   what the test names. A follow-up `GET /v1/indexes/multi2` answers `404`.
   - nine further fields;
   - a further field named `$x`, and one named `id`;
   - `{"type": "int", "full_text_search": true}` on `title`;
   - `full_text_search: {"k1": 4}` on `title`, and `{"tokenizer": "x"}`;
   - a document of the declaring write whose `title` is `5`, and one whose `tags` is
     `["a", 5]`.
4. **Fixed at creation.** After the write of criterion 1, a write to `multi` declaring `title`
   with other options is refused `400 schema_conflict`, naming "the further text fields" and
   "reindex". A write declaring `tags` alone is refused likewise. A write declaring nothing,
   and one repeating criterion 1's declarations exactly, are accepted. Each holds before the
   first fold, against the pending rows, and after it, against the HEAD's schema.
5. **The query syntax.**
   - `{"field": "title"}` with no `query`, `{"field": "title", "query": 5}`,
     `{"field": "title", "query": "a", "limit": 3}`, and `[]` are refused `400`.
   - On `multi`, before and after the fold, legs `[{"field": "title", ...}, {"field":
     "zzz", ...}]` are refused `400 unknown_field`, the message holding "text field `zzz`" and
     not `title`. (`zzz` sorts after `title`, so a refusal naming the first queried field
     would name `title`.) So are `[{"field": "nosuch"}]` beside a dense vector leg, and
     a leg over `$x`.
   - On an index whose rows hold `title` and never `text`, a string leg is refused `400
     unknown_field`, naming `text`, and a `title` leg answers.
   - `highlight` beside a `title` leg is refused `400`, naming the further field; beside a
     `{"field": "text"}` leg it marks as today.
6. **Reported.** `GET /v1/indexes/multi` after the fold reports `text_fields` as an object
   keyed by field name, each value the object `full_text_search` reports for the primary:
   `title`'s options as declared and `tags`'s defaults, each equal to what its twin reports as
   `full_text_search`. An index with no further field reports no
   `text_fields` key.
7. **Unchanged.** Every existing HTTP test passes unmodified but two cases, reversed on
   purpose. `sketch.rs`'s `a_declaration_that_cannot_mean_anything_is_refused_by_name` and
   `analyzer.rs`'s `what_an_analyzer_cannot_mean_is_refused` each refuse `full_text_search` on
   `title` as "on the text field". Each case becomes `{"title": {"type": "int",
   "full_text_search": true}}`, refused for its type, and its comment names this milestone.
   (The spec named only `sketch.rs`'s until the workspace run found `analyzer.rs`'s.)

## Test plan
Red first against the parent:
- 1, 2, 3, 4 and 6 fail: the parent refuses `full_text_search` on `title`.
- 5's object legs fail: the parent's `text` takes strings only. Its `[]` refusal is a guard,
  green on the parent.
- 7 is a guard.

VERIFIED.md rests each test's power on these mutations, each observed red:
- 1: "an object leg is read as a leg over `text`"; "a further field is declared with the
  default `FullText`"; "`Declared.texts` is not passed".
- 2: "text legs are reordered by field".
- 3: "a further field's options skip the parser's checks"; "a further field's document value
  is not checked".
- 4: none of its own. The conflict check is M152's, whose mutations its tests kill; this test
  shows the server passes the declaration a later write makes, which criterion 1's "not
  passed" mutation also breaks.
- 5: "`query_error` leaves `UnknownTextField` a generic query error"; "the refusal names the
  first text leg's field"; "a primary leg is never refused as unknown"; "highlight accepts a
  further-field leg"; "an object leg's unknown key is ignored".
- 6: "`text_fields` is always written"; "`text_fields` reports the defaults".

The tests live in `crates/pstore-server/tests/it/text_fields.rs`.

## RA budget
Unchanged. A query over several fields costs what M153 measured: the rounds of one field, and
requests that add but for one open. A write declaring further fields carries M152's `$texts`
on each declaring row.

## Risks
- **Readers first.** M152's HEAD section is written once an index declares a further field,
  and a build before M152 refuses that HEAD. M152 merges before this milestone, so a
  deployment upgrading from a build older than M152 must pass through a build carrying M152
  before serving M155's writes. There is no deployment yet. No switch gates it.
- **The token filter's analyzer** is the primary's for a further field until M156.
- **A later write is not type-checked** against the further fields, because a write that
  declares nothing does not know them. A non-string value under a further field there is
  stored and has no text, as the engine treats a row written below the door (M150).
- **Not split.** A query over a further field, or several fields, runs on one server until
  [M154](../M154/SPEC.md).

## Tasks
- M155.1: the declaration, the type check and the reported `text_fields` (1, 3, 4, 6, 7).
- M155.2: the object legs, `UnknownTextField` in the query layer and engine, and highlight's
  refusal (1, 2, 5).
