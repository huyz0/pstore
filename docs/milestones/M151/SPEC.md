# M151: several text fields in one segment

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"), the multi-field half of [DELIVERY.md](../../comparisons/DELIVERY.md)'s
"Multi-field/array BM25 ..." row. This is the first of three steps:
- **M151:** the segment format and the query layer;
- **M152:** the engine's schema, seal, fresh view, stamp and compaction;
- **M153:** the HTTP declaration and the vendor's `BM25` / `Sum` / `Max` ranking expressions,
  with per-field statistics for a query that reads several fields.

No D- or OQ- id covers it. D-30 (corpus statistics ride in the dictionary sidecar) and D-34
(the round-trip budget) constrain it.

## Delta
Today a segment carries at most one text field:
- its `TextFields` section names it;
- its postings are one `TextPostings` section, and its row lengths one `Fieldnorms` section;
- its term dictionary is one sidecar object (`{segment}.tdict`, `PSTORETD` version 1), which
  also carries the corpus summary (rows, total tokens).

A field with no text in any row is written as no text at all: no `TextPostings`, so no
`TextFields`. A text leg over such a segment answers nothing and adds nothing to the
statistics (M30).

After:
- **The fields a segment carries.** A segment carries a text field only if some row has text
  in it, by M150's `has_text`. Asked for fields `[a, b, c]` where no row has text in `b`, it
  carries `[a, c]`, in that order. This is today's one-field rule, applied per field.
- **One carried field: unchanged, byte for byte.** Its postings, fieldnorms, `TextFields`
  section and dictionary bytes are what a one-field build writes today.
- **Several carried fields (`N >= 2`): one sidecar, several dictionaries.** All integers are
  little-endian, as in `PSTORETD`.
  - `TextFields` names the `N` fields in order. A name never repeats.
  - `TextPostings` is field 0's postings, then field 1's, and so on, with no gaps.
  - `Fieldnorms` is field 0's row lengths for every row, then field 1's, and so on. Field
    `i`'s length of row `r` is entry `i * rows + r`.
  - The sidecar is a container, `PSTORETM` version 1:
    - a header: magic (8), version (2), `N` (2);
    - a directory of `N` entries, each the dictionary's byte length (4), and the start (8)
      and length (8) of its postings within `TextPostings`. The postings spans start at 0,
      follow each other with no gap or overlap, in field order;
    - the `N` dictionaries, back to back, in field order, each exactly what a one-field build
      of that field writes (`PSTORETD` version 1, offsets relative to that field's own
      postings). Nothing follows the last.
  - So **a carried field's slice is a one-field build of that field**, and a reader of one
    field reads exactly what it reads today, through narrower ranges.
- **Build.** `text::build_fields(docs, &[(name, &Analyzer)])` returns a new `text::Fields`
  holding:
  - the carried names;
  - the postings, the dictionary sidecar and the fieldnorms (`N * rows` of them).

  Each field is built by `build_with` alone, and the carried ones are joined as above. When
  no field has text, nothing is carried and `Fields` is empty: no postings, no fieldnorms, no
  sidecar, as a one-field build of such a field writes no `TextPostings` today.
  `Built` and `build_with` are unchanged: `fieldnorms` stays one per row. A repeated name, or
  an empty list, is refused (`None`). `vec_index::try_build_all_fields` takes the field list
  and refuses what `build_fields` refuses (`FormatError`). The existing builders pass a
  one-element list and are otherwise unchanged.
- **Read.**
  - `text::Dictionaries::decode(raw, n)` checks the sidecar on its own. For `n = 1` it is one
    `PSTORETD` dictionary, as today. For `n >= 2` it is a `PSTORETM` container, and it returns
    `None` for:
    - a header `N != n`;
    - a directory or dictionary that overruns, or trailing bytes;
    - postings spans that do not start at 0 and follow each other;
    - a dictionary that does not decode.

    Each field's dictionary is a `Bytes::slice` of the fetched buffer, never a copy.
  - `Dictionaries::decode_shared` keeps the whole decoded container per fetched buffer (M122's
    cache), charged the buffer's length plus every decoded dictionary's heap.
  - `TextIndex::for_field(segment, dictionaries, field)` opens a carried field. It checks the
    segment's sections, and refuses (`Corrupt`):
    - the last postings span not ending at the `TextPostings` length;
    - for `N >= 2`, a `Fieldnorms` length other than `N * rows * 4`, with `rows` that field's
      dictionary's;
    - a `TextFields` name that repeats.

    For one field it checks nothing new. Its postings and norms ranges are the field's
    sub-ranges, so `search_with`, `summary_for` and `list_bytes` are unchanged.
  - `TextIndex::from_segment` (and `_shared`) opens the segment's only field, and refuses a
    segment carrying several (`UnknownField`). No caller is handed the wrong field's postings
    by default.
- **Query.**
  - A text leg opens the field it names, and its statistics, in `open_part` and in the merged
    path alike, are that field's.
  - The `FullText` a query is given (its analyzer, `k1`, `b`) is the queried field's. That
    holds because of the next rule.
  - **One field per query.** A query whose text legs name different fields is refused
    (`QueryError`, named), before any I/O, over the whole leg list: in `run` and
    `scan_part` beside the collect of runnable legs, and in `open_part` before it opens
    anything, since a phased share's peer calls it with raw legs. A query over several fields
    needs per-field statistics and analyzers: that is M153's.
  - **A segment that does not carry the field** answers that leg with nothing, and adds
    nothing to its statistics, as a segment with no text does today (M30). That is what a
    segment sealed before a field had text needs.
  - **A field no segment carries** is refused (`UnknownField`) when some segment of the query
    carries text and none carries the field. This is today's wrong-field refusal, kept across
    segments: a typo is never an empty answer. `run` decides it from the footers it opens for
    every target, phased shares' included, beside the existing `nowhere` refusal for named
    vector fields. It needs no request and no peer's answer.

    `open_part` never refuses a field its own segments lack. It returns that field's empty
    statistics, because the field may live in another share's segments.
  - `Opened` keeps the decoded index per field.

Unchanged:
- every request, round trip and depth (see RA budget);
- `Stats`, BM25 and the phased-share wire format;
- the engine, compaction (it refuses inputs whose `text_fields()` differ; M152 changes it) and
  the server. Nothing above the query layer asks for a second field until M152;
- the highlight, the fresh view and the `text` request key.

## Acceptance criteria
1. **One field is unchanged.** For each analyzer of {default, English stemming + stopwords,
   case-sensitive, ASCII folding} and a fixed set of rows: `build_fields` over one field
   equals `build_with` in postings, dictionary and fieldnorms. A segment from
   `try_build_all_fields` with one field has the same segment bytes and `.tdict` as one from
   `try_build_all_with`, over rows where the field has text.
2. **A field's slice is its one-field build.**
   - Rows hold fields `a`, `b`, `c` and `e`: each a string, an array, absent or empty, with a
     different analyzer per field. `e` has no text in any row.
   - Built with `[a, b, e, c]`, the carried names are `[a, b, c]`.
   - For each carried field `f`: its dictionary from `Dictionaries::decode`, its
     `TextPostings` slice and its fieldnorms slice equal `build_with(rows, f)`'s.
   - `[a, a]` and `[]` are refused.
3. **Query equivalence.**
   - Two segments are built by `try_build_all_fields` with `[a, b, c]`. In the second, no row
     has text in `c`, so it carries `[a, b]`.
   - For every field `f` and a fixed set of queries, `pstore_query` text results agree with
     the same query over the same rows built with `[f]` alone, each under `f`'s `FullText`:
     the same ids and order, and scores equal to 1e-6.
   - This holds for a single text leg, two text legs over `f`, and a hybrid of a vector leg
     and a text leg.
   - For `c`, the second segment answers nothing and adds nothing to the statistics, exactly
     as its `[c]`-only twin carries no text.
4. **Refusals.**
   - `Dictionaries::decode` returns `None` for each sidecar malformation listed in the Delta,
     and for a truncation at every byte of a valid container.
   - `for_field` refuses each section mismatch listed.
   - `from_segment` over a three-field segment is `UnknownField`.
   - A query with text legs over `a` and `b` is refused, through `query`, through
     `query_rows_split` with a phased share, and through `open_part`.
   - A leg over `d`, which no segment carries, is `UnknownField`.
   - `open_part` over a share whose segments carry text but not `c` returns empty statistics
     for `c`, not an error. Through `query_rows_split`, a query whose phased share's segments
     carry `c`, beside local segments that do not, answers as in criterion 3.
5. **Round trips.** A text query over field `b` of a three-field segment costs the same
   number of sequential rounds and requests as one over a one-field segment. The extra bytes
   it reads are the sidecar's directory and the two other dictionaries, inside the object
   fetched anyway, and nothing else. This is asserted with `pstore-blob`'s request and byte
   accounting.

## Test plan
Red first against the parent:
- Criteria 1 to 4 do not compile: `build_fields`, `Fields`, `Dictionaries`, `for_field` and
  `try_build_all_fields` are absent.
- Criterion 5's three-field half does not compile.

A compile failure shows only that the API is new. So VERIFIED.md will rest each test's power
to fail on the named mutations below, each observed red.
- 1's first half, 2 and 4's decode: `pstore-format` integration tests (`tests/it/text.rs`).
  Mutations:
  - "the fieldnorms are not offset by field";
  - "postings starts are not shifted";
  - "a one-field build writes a container";
  - "a field with no text is carried";
  - "decode accepts `N != n`";
  - "decode accepts a gap between spans".
- 1's second half, 3, 4's other refusals and 5: `pstore-query` integration tests
  (`tests/it/`). Mutations:
  - "`for_field` opens field 0";
  - "the statistics are field 0's";
  - "text legs over different fields are accepted" (once each for `query` and `open_part`);
  - "`from_segment` opens field 0 of several";
  - "a segment without the field is `UnknownField`" (the parent's rule);
  - "a field no segment carries answers nothing";
  - "`open_part` refuses a field its segments lack".

## RA budget
Unchanged in rounds and requests:
- the sidecar is still one object, fetched beside the footer (D-30);
- a text leg still fetches its postings and its norms in one ranged read.

The sidecar grows by the other fields' dictionaries. That cost is bytes, not requests, and it
grows with the fields an index declares, which M153 will bound.

## Risks
- **An older reader** sees a `PSTORETM` sidecar as malformed, and fails a text query over that
  segment loudly rather than wrongly. No writer produces one until M152, so deploying M151
  changes nothing on disk.
- **The sidecar is fetched whole** for every text query, so a field that is rarely queried
  costs every query its dictionary bytes. Per-field sidecars would trade those bytes for a
  request per field queried. This spec keeps requests constant, as AGENTS.md prefers, and
  M153's bound on field count caps the bytes.
- **A segment without the field answers nothing.** A field added after some segments were
  sealed is then ranked only over the segments that carry it. That is the honest answer: the
  other segments hold no text for it.

## Tasks
- M151.1: `build_fields`, `Fields`, the container, `Dictionaries` (1's first half, 2, 4's
  decode).
- M151.2: `for_field`, `try_build_all_fields`, the query layer (1's second half, 3, 4, 5).
