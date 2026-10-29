# M145: the vendor's `Fuzzy` filter

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("vendor Fuzzy
configuration ... match exhaustive reference predicates for missing/null/empty arrays and nested
negation"), the remaining half of [DELIVERY.md](../../comparisons/DELIVERY.md)'s "Missing array
predicates and compatible fuzzy semantics" row. No D- or OQ- id covers it.

## Delta
Today `Fuzzy` is pstore's own shape from [M15](../M15/SPEC.md):
`[attr, "Fuzzy", {"value": v, "max_edits": k}]`, the whole of a scalar string within `k` edits.
The vendor's [query documentation](https://turbopuffer.com/docs/query) describes a different
filter under the same name, and a client written against it gets a `400` here.

After, beside the M15 shape, which is unchanged:
- **Shape.** `[attr, "Fuzzy", q, {"max_edit_distance": [{"min_query_chars": m, "distance": d}, ...],
  "case_sensitive": bool}]`, with `q` a string. `max_edit_distance` is required;
  `case_sensitive` is optional and defaults to `true`, as the vendor documents.
- **The distance.** With `n` the number of characters (Unicode scalar values) of `q`, `k` is the
  `distance` of the entry with the largest `min_query_chars` not above `n`. When every entry's
  `min_query_chars` exceeds `n`, the filter matches nothing, so `Not` of it matches every row.
  The vendor documents only the first-threshold rule; "largest entry not above `n`" is this
  spec's reading of "edits to tolerate by the query length".
- **What matches** ("fuzzy substring match against `string` or `[]string`"): a string holds
  when **some substring of it**, possibly empty, is within `k` Levenshtein edits of `q`, in
  characters. A scalar string attribute holds when it holds. An array holds when one of its
  string elements holds; other elements contribute nothing, and an array with no string element
  holds nothing. Anything else (absent, number, bool, null) holds nothing.
- **Case.** With `case_sensitive: false`, both `q` and the value compare after M15.2's
  [`trigram::fold`](../../../crates/pstore-format/src/trigram.rs), one character to one, so
  character counts and distances are unchanged by folding. With `true`, `M` and `m` differ by
  one edit, as the vendor's example says.
- **Refused** (`400 malformed filter`):
  - `q` not a string, or more than 256 characters (M15's bound);
  - the options not an object, any key besides the two, `case_sensitive` not a bool;
  - `max_edit_distance` missing, not an array, empty, or longer than 8;
  - an entry that is not an object of exactly `min_query_chars` and `distance`, both
    non-negative integers;
  - `distance` above 2, `min_query_chars` below `3 * (distance + 1)` (the vendor's rule), or
    entries whose `min_query_chars` are not strictly increasing;
  - the three-element `[attr, "Fuzzy", q]`, since the vendor requires the options;
  - a reserved attribute name (`$...` or `""`), as for every clause (M40);
  - beside a `$ref_new` reference in a write condition, as M15's `Fuzzy` is: `ref_new.rs`'s
    `costly()` gains the variant, which its `_ => None` arm would otherwise let through.
- **Representation.** `Predicate::FuzzySubstring(attr, needle, k, case_sensitive)`, with `k`
  resolved at the door. A query below every threshold parses as `Or([])`, which is false.
  Write conditions encode it as tag 13: the attribute, the needle's text, `k`, and the case flag
  as one byte (0 or 1). Decoding refuses `k` above 2 and any other case byte.
- **Pruning.** It never prunes a block. The M15.2 sketch holds only scalar strings, so a block
  of arrays would be skipped wrongly; widening the sketch is a format change for a later
  milestone. The filter is evaluated on the rows a mask already decodes.
- **Cost.** A row costs `O(|value| * |q|)` characters, by Sellers' algorithm (the edit-distance
  table with a free start and end in the value). `q` is at most 256 characters, and a filter
  counts this toward M15's cap of 16 patterns.

Unchanged except as listed: the M15 object shape and its whole-value meaning; `Glob`, `Regex` and the sketch;
storage formats. Not offered: a `fuzzy` schema flag. The vendor requires one before use, but
here the filter needs nothing from the schema, so a request without it is accepted.

## Acceptance criteria
1. **Oracle.** Over the alphabet `a A é É` (an ASCII and a non-ASCII cased pair): queries are
   every string of 3 characters (64) and the fixed list `aAéÉ`, `ééaaA`, `AAAAA`; `k` is 0, 1
   and 2; both case settings. Rows hold: absent, `5`, every string of 0 to 4 characters (341),
   `[]`, `[5]`, and, for each 4-character string `s`, `["", s]` and `[s, 5]` (so a match only
   in the second element, and a string beside a number, are both reached). `FuzzySubstring`,
   `Not` of it and `Not(Not)` of it each admit exactly what a reference admits. The reference:
   some substring of some string element, enumerated by start and end, has a full-table
   Levenshtein distance to `q` of at most `k`, comparing `trigram::fold`ed characters when
   case-insensitive. Each of admitted and refused occurs.
2. **Thresholds.** Through HTTP, with `[{3,0},{6,1}]`:
   - `turbopufer` (10 characters, `k = 1`) finds `"turbopuffer inc"`;
   - `turxo` (5 characters, `k = 0`) finds nothing, and `turbxo` (6 characters, `k = 1`)
     finds `"turbo"`. (A 4- or 5-character query cannot take `k = 1`: the vendor's rule puts
     its threshold at 6 or more.)
   - `tu` (2 characters) finds nothing, and `Not` of it finds every row.
   With `case_sensitive: false`, `TURBO` finds `"turbo"`; with the default it does not.
3. **HTTP.** Before and after the index's fold (rows in the WAL, then in segments), a scalar
   and a string-array attribute return, through a query filter and through `delete_by_filter`,
   the ids the criterion-1 reference computes over the same rows. One row matches only in its
   array's second element.
4. **Refusals.** Each refused shape above is `400`, `$ref_new` beside it included, and the M15
   object shape answers as before.
5. **Conditions.** Tag 13 round-trips with each `k` and case flag. `k = 3`, a case byte of 2,
   and tag 14 each decode as `None`.
6. **Pruning.** Over a zone with a sketch for the attribute whose block holds none of `q`'s
   trigrams, `could_admit` of `FuzzySubstring` and of `Not` of it is true, while M15's `Fuzzy`
   with the same value and `k` is false on that zone, so the fixture is one that prunes.

## Test plan
Red first against the parent: criteria 1, 5 and 6 do not compile (no variant); 2 and 3 fail
with a `400`. Criterion 4's refusals are guards, green on the parent, which refuses every
four-element `Fuzzy`, and so is 5's tag 14; VERIFIED.md records them as such.
- 1: `pstore-query` unit test. Mutations: "the start is not free" (whole-prefix matching),
  "the end is not free", "`k` off by one", "case folding ignored", "only the first array
  element is read", "an empty array holds".
- 2 to 4: `crates/pstore-server/tests/it/patterns.rs`, and `ref_new.rs` for `$ref_new`.
  Mutations: "the smallest entry not above `n` is chosen", "a query below every threshold
  matches everything", "`case_sensitive` defaults to `false`", "an array is read as no
  strings", "`costly()` omits the variant".
- 5: the `condition` round-trip tests. Mutation: "the case flag is not encoded".
- 6: a `pstore-query` unit test building the sketched zone. Mutation: "it prunes on the
  sketch" (M15's arm, copied).

## RA budget
Unchanged: no request is added; the filter runs on rows already decoded and never prunes.

## Risks
- **"Substring" is the vendor's word, not a definition.** This spec reads it as approximate
  substring matching (Sellers), the reading the vendor's example ("matches substrings within
  edit distance 1") fits. Conformance fixtures would reveal another.
- **No pruning** makes a selective vendor `Fuzzy` a scan of the mask's rows, as an unpruned
  `Glob` already is.
- **An older fold** decodes tag 13 as `None` and skips the guarded write (M135's risk).

## Tasks
- M145.1: `FuzzySubstring` in `pstore-query`: evaluation, `could_admit`, pattern count,
  condition tag 13 (criteria 1, 5, 6).
- M145.2: the parser's vendor shape, `costly()`, and the server tests (2, 3, 4).
