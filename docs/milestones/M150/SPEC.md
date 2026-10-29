# M150: BM25 over a string-array text field

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"), the string-array half of [DELIVERY.md](../../comparisons/DELIVERY.md)'s
"Multi-field/array BM25 ..." row; the [comparison](../../comparisons/turbopuffer-2026-10-25.md)
lists `full_text_search` as "default text field only; not arbitrary string/string-array
attributes". Several text fields per index is a later milestone. No D- or OQ- id covers it.

## Delta
Today the text field is a string only. The HTTP door refuses an array under it
(`to_document`: "is the text field and must be a string", pinned by `arrays.rs`'s
`what_an_array_cannot_hold_is_refused`), and `declared_fts` refuses a declared type other than
`"string"`. Below the door, four places read "has text" as a non-empty `Value::Str`:
`text::build_with`, the engine's `has_text` (behind `text_of`, the M36 stamp and its conflict
check), `seal`'s `text_field` filter, and the fresh view's `wants_text` (whether unfolded rows
are scored at all).

After:
- **The door.** `attributes.<text field>` may be a string, as today, or an array whose every
  element is a string; an array holding anything else is refused (`400`), naming the field.
  The top-level `text` key stays a string. A declared `"type"` of `"[]string"` with
  `full_text_search` is accepted beside `"string"`.
- **One helper.** `pstore_format::text::strings(&Value)` yields a string's one string and an
  array's string elements in order (any other element is skipped, for rows written below the
  door), and nothing for anything else. All four places read it:
  - **has text** is "some yielded string is non-empty" in `has_text`, `seal` and
    `wants_text`, so `[]` and `[""]` have no text, exactly as an absent field;
  - **tokens**: `build_with` analyzes each yielded string alone with the index's analyzer and
    concatenates the results, in order; the field length is their number.
- **Scoring.** BM25 is unchanged: an array row scores exactly as a row holding its strings
  joined by a space, since a space ends a token under every analyzer and nothing crosses it.
- **Reversed on purpose**: `arrays.rs`'s `{"text": ["a"]}` refusal becomes
  `{"text": ["a", 5]}`, an array holding a non-string, which is still refused.

Unchanged: the query side, BM25 parameters, the dictionary and postings formats, the single
text field per index, filters (M138 already reads arrays), and highlights (an array field is
not marked; structured highlights over arrays are a later milestone).

## Acceptance criteria
1. **Tokens.** For each analyzer of {default, English stemming + stopwords, case-sensitive,
   ASCII folding} and rows that hold: absent, `5`, `""`, a string, `[]`, `[""]`, `[5]`,
   `["a b", 5, "c"]`, and arrays of 1 to 3 strings drawn from a fixed list, `build_with` over
   the rows produces the same postings, dictionary and fieldnorms as over the same rows with
   every array replaced by its string elements joined by `" "`.
2. **HTTP, arrays only.** Two indexes, one whose rows all hold the text field as arrays of
   strings and one with the same arrays joined by a space, answer the same `text` queries with
   the same ids, order and scores, before and after the fold. Then a third index mixing string
   and array rows does likewise against its joined twin.
3. **Has text.** In an index whose other rows hold array text, a row whose text field is `[]`
   or `[""]` is returned by no `text` query, before and after the fold, and a row with
   `["", "fox"]` is.
4. **The door.** An array text field holding a non-string, and a declared type other than
   `"string"` or `"[]string"`, are refused; `"[]string"` with `full_text_search` is accepted.
5. **Engine.** A writer whose text field is `title` is refused with `SchemaConflict { what:
   "the text field" }` when it writes a row whose `title` is `["beta"]` into an index whose
   recorded text field is `body`, while rows whose `title` is `[]` or `[""]` are accepted, as a
   row without `title` is; and the first fold of only array rows records its field and
   writes postings (a text query finds them).
6. **Highlights** of an array text field return no marks (a guard).

## Test plan
Red first against the parent: 1 and 5 fail (an array row has no tokens and no text); 2 to 4
fail at the door, a `400` for the array write and for `"[]string"`; 6 cannot be set up on the
parent (no array text is stored) and is a guard after. VERIFIED.md records each red.
- 1: `pstore-format` unit test. Mutations: "only the first element is read", "elements are
  joined without a separator before analysis", "a non-string element ends the array".
- 2 to 4, 6: `crates/pstore-server/tests/it/`. Mutations: "`wants_text` reads strings only"
  (2's unfolded half), "`seal` reads strings only" (2's folded half), "the field length counts
  elements, not tokens".
- 5: `crates/pstore-engine/tests/it/schema.rs`. Mutations: "`has_text` reads strings only",
  and "`[""]` has text", which criterion 3 cannot see: a `[""]` row has no tokens either way.

## RA budget
Unchanged: the same postings and dictionary per segment; no request is added.

## Risks
- **The vendor does not document BM25 over `[]string`.** This spec reads it as one field whose
  tokens are the elements' tokens in order. Per-element field lengths or a max over elements
  would rank differently; conformance fixtures would reveal it.
- **Rows already folded** with an array text field have no postings; a later fold of them
  re-indexes them, as any re-fold does. Nothing is migrated.

## Tasks
- M150.1: `text::strings`, and `build_with`, `has_text`, `seal` and `wants_text` on it (1, 5).
- M150.2: the door, `declared_fts`, the reversed `arrays.rs` case, and the HTTP tests (2, 3,
  4, 6).
