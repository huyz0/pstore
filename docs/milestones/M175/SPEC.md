# M175: null elements in scalar arrays

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): its
"nullable scalar arrays" round-trip target. Follows [M174](../M174/SPEC.md), which made a
top-level `null` an absent attribute and kept a `null` array element refused.

## Delta
Today an array attribute holds scalars ([M9h.2](../M9h/SPEC.md)), and a `null` element is
refused. After:
- **Written.** An array may hold `null` elements, undeclared or under a declared type
  (`["6f1e…", null]` declared `uuid`): every non-null element is read as today, each `null`
  is a null element in its place. A top-level `null` is still absent (M174), and a patch's
  `null` still removes. A filter literal holding `null` (in `In`, `Contains*`, `Any*` or
  an array `Eq`) is still refused, 400: `["a", "Eq", null]` stays M9b's "absent".
- **Stored.** `pstore_format::Value::Null`, value tag 9 (no payload), in segment blocks,
  bundles and the deferred-condition codec (a `$ref_new` condition may copy such an array).
  Only as an array element: `check_storable` refuses it at the top level, so no zone,
  key or sketch ever sees one. Arrays have no zone map, so the zone flags are unchanged.
  A reader before M175 refuses tag 9 as unknown: deploy readers first; an older fold
  decodes such a condition as unreadable, as for M171's tags.
- **Answered.** A null element answers `null` in its place, order kept, in projections, the
  unfolded view, after a fold and after a restart.
- **Compared.** A null element equals no literal and is ordered by no comparison, so
  `Contains`, `Any*`, `NotContains` and the rest read it as a value that matches nothing
  (`["a", "NotContains", 1]` holds for `[null]`). Whole-array equality (`Eq`, `In`, facets,
  `group_by`, `limit_per`) compares structurally, a null element equal only to a null
  element. `rank_by` over arrays orders a null element after every other element kind.

## Acceptance criteria
1. Undeclared `[1, null, "x"]`, `[null]`, and `[null, "6f1e…"]` declared `uuid` answer
   exactly those arrays (the uuid lowercase) in a projection, unfolded, folded and after a
   restart.
2. Over rows holding `[1, null]`, `[null]`, `[1]` and `[]`: `Contains 1` gives the first and
   third; `NotContains 1` the second and fourth; `AnyGt 0` the first and third; `AnyLt 5`
   the same; `["a", "Eq", null]` none of them (each holds `a`).
3. A facet over `[1, null]`, `[1, null]` and `[1]` gives two buckets, counts 2 and 1;
   `rank_by` asc over `[1, null]`, `[1, 2]` and `[1]` gives `[1]`, `[1, 2]`, `[1, null]`.
4. Refused, 400, nothing buffered: a `null` element in a filter literal (`In`, `Contains`,
   `AnyEq`); `[["x"], null]` (still nested); a declared `uuid` array with a malformed
   non-null element.
5. Format: tag 9 round-trips inside an array in blocks, bundles and the condition codec; a
   top-level `Value::Null` is refused by `check_storable`; a tag-9 array in a `$ref_new`
   condition applies at the fold.
6. Existing tests pass, the codecs' fixtures and the key order extended, losing no case;
   `upsert_columns::null_in_an_upsert_is_absent_and_in_a_patch_removes`'s `[1, null]`
   refusal becomes an acceptance (criterion 1's).

## Test plan
Server tests in `crates/pstore-server/tests/it/null_elements.rs`; format and condition tests
beside their codecs' fixtures. Red before: 1 to 4 on the parent. Mutations, each to be
killed: tag 9 not written or not read (1, 5); a null element matching a `Contains` or an
`Any*` (2); null elements equal to everything in a group (3); a null filter literal
accepted (4); a top-level null stored (5).

## RA budget
Unchanged: a null element is one byte inside an array already stored; no request is added.

## Risks
- Every exhaustive `match` on `Value` gains an arm. The compiler finds each; the risk is an
  arm that treats `Null` as a match, which criterion 2 and the mutations cover.
- A rollback after a tag-9 segment is written leaves an older reader refusing it.

## Tasks
- M175.1: `Value::Null`, tag 9 in the codecs, `check_storable`.
- M175.2: the server's reading and answering, filters' and orders' treatment, docs.
