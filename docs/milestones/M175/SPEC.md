# M175: null elements in scalar arrays

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): its
"nullable scalar arrays" round-trip target. Follows [M174](../M174/SPEC.md), which made a
top-level `null` an absent attribute and kept a `null` array element refused.

## Delta
Today an array attribute holds scalars ([M9h.2](../M9h/SPEC.md)), and a `null` element is
refused. After:
- **Written.** An array may hold `null` elements, undeclared or under a known type (M173):
  each non-null element is read as today, each `null` is a null element in its place, and
  `Kind::holds` admits null elements, so `[null]` or `[null, "6f1e…"]` meets a recorded
  `uuid` at the door and at the fold. A patch may set such an array. Unchanged: a top-level
  `null` is absent (M174), a patch's removes; a filter literal is a scalar (an array
  literal, or `null` beside `In`/`Contains*`/`Any*`, refused 400), `["a", "Eq", null]`
  stays "absent"; a text field's elements, further or pre-tokenized (M150, M155), are
  strings, so a `null` there is refused.
- **Stored.** `Value::Null`, value tag 9 (no payload), in segment blocks, bundles,
  `skip_value` and the deferred-condition codec, **only inside an array**: each decoder
  refuses tag 9 at the top level as corrupt, and `check_storable` refuses a top-level
  `Null`, so no zone, key or sketch sees one. No zone or version changes: arrays have no
  zone map, and a reader before M175 refuses tag 9 loudly. Deploy readers first.
- **Answered.** A null element answers `null` in its place, order kept, everywhere a value
  is answered, unfolded, folded and after a restart.
- **Compared.** A null element is ordered by no comparison, so `Contains*`, `Any*` and their
  negations read it as matching nothing (`["a", "NotContains", 1]` holds for `[null]`).
  Facets, `group_by` and `limit_per` compare arrays element by element: a null element
  equals only a null element, and sorts after every other element kind. `rank_by` is
  unchanged: arrays tie, and the next key decides.

## Acceptance criteria
1. Undeclared `a: [1, null, "x"]` and `a: [null]`, and `u: [null, "6f1e…"]` declared `uuid`,
   then once `uuid` is recorded an undeclared `u: [null]` and patches setting `a: [2, null]`
   and `u: [null, "6f1e…"]`: each answers exactly its array (the uuid lowercase) projected,
   unfolded, folded and after a restart, beside a second attribute projected alone.
2. Over rows holding `[1, null]`, `[null]`, `[1]` and `[]`: `Contains 1` gives the first and
   third; `NotContains 1` the second and fourth; `AnyGt 0` the first and third;
   `["a", "Eq", null]` none.
3. A facet over `[1, null]`, `[1, null]`, `[1, 2]` and `[1]` answers the buckets `[1]` (1),
   `[1, 2]` (1) and `[1, null]` (2), in that order, each spelled with its `null`.
4. Refused, 400, nothing buffered: `null` in an `In`, `Contains` or `AnyGt` literal; an
   array literal; `[["x"], null]`; a declared `uuid` array with a malformed element; a
   `null` in the text field's array; a patch setting `u: [2, null]` once `uuid` is recorded.
5. Format: tag 9 round-trips inside an array in blocks, bundles and the condition codec;
   tag 9 at the top level is refused by each decoder; `skip_value` passes over it; a
   top-level `Value::Null` is refused by `check_storable`; `Kind::holds` admits null
   elements and still refuses a wrong-kind one.
6. Existing tests pass, the codecs' fixtures and the key order extended, losing no case;
   the `[1, null]` refusals of
   `upsert_columns::null_in_an_upsert_is_absent_and_in_a_patch_removes` and
   `arrays::what_an_array_cannot_hold_is_refused` become acceptances (criterion 1's).

## Test plan
Server tests in `crates/pstore-server/tests/it/null_elements.rs`; format and condition tests
beside their codecs' fixtures. Red before: 1 to 3 on the parent; 4 is a regression guard,
seen red under its mutation. Mutations, each to be killed: tag 9 not written or not read, or
not skipped (1, 5); `holds` refusing a null element (1); a null element matching a
`Contains` or an `Any*` (2); a null element read as the group key's absent, or equal to
everything (3); a null filter literal accepted (4); a top-level null stored or decoded (5).

## RA budget
Unchanged: a null element is one byte inside an array already stored; no request is added.

## Risks
- Every exhaustive `match` on `Value` gains an arm. The compiler finds each; the risk is an
  arm that treats `Null` as a match, which criterion 2 and the mutations cover.
- A rollback after a tag-9 segment is written leaves an older reader refusing it.

## Tasks
- M175.1: `Value::Null`, tag 9 in the codecs, `check_storable`, `Kind::holds`.
- M175.2: the server's reading and answering, filters' and orders' treatment, docs.

## Review
Spec review round 1 (block, four majors, all taken): `rank_by` ties arrays, so the order
claim moves to facets, with buckets asserted by value; a null element is not the group
key's absent; no `$ref_new` path carries an array, so the condition codec is a unit test;
`Kind::holds` admits null elements, with a write after the type is recorded. Minors taken:
`Eq`/`In` never compare arrays; text fields keep refusing; top-level tag 9 refused by each
decoder and `skip_value` covered; a patch case.
Round 2 (approve with one major, taken): the patch case names its attribute, and the
uuid one's conflict is a refusal. Minors taken: a facet pair that differs only in the null
element; criterion 4 is a regression guard. During implementation a second existing test
refusing `[1, null]` was found and is named beside the first.
