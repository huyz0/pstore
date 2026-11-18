# M275: the facade's schema in the vendor's names, read and written

## Serves
Workstream B's facade ([M268](../M268/SPEC.md)): [M272](../M272/VERIFIED.md) found `schema()`
answering `{}` for a namespace with a text attribute, since it passed on only the native
summary's recorded attribute types (M173). No decision is reopened; no request is added.

## Delta
**Read.** `GET /v1/namespaces/{ns}/schema`, and `schema` in `metadata()`, translate the whole
native summary's `schema` instead of its `attributes`:
- `vector` (when `dims` > 0) and each named dense field (`vectors`): `type` `[N]f32`, or the
  recorded `[N]f16` / `[N]i8` (M195), and `ann: {"distance_metric"}` with the index's metric:
  pstore builds ANN by rule for every dense field (M198).
- The primary text field and each further one (M152): `full_text_search`, the native analyzer
  object as the summary answers it. **No `type`**: pstore does not record whether a text field
  holds strings or string arrays, and the SDK reads a missing type as `None` (checked against
  3.1.0's `construct_type`).
- Each regex attribute (M15.2): `regex: true`.
- Each recorded attribute (M173): `type`, `uint64` named the vendor's `uint`, the rest as
  they are. A recorded type holds a scalar and an array of it alike (M175), so it is named as
  the scalar. `id`: its kind (M176), `uint64` as `uint`.
- Entries for one name merge. `null` before a first fold, as before.

**Write.** A vendor write's `schema` names its types as the native write reads them: `uint` is
`uint64`; `[]uint`, `[]uuid` and `[]datetime` are their scalar kind; `{"type": t}` alone is `t`;
`ann: true` is dropped, being what pstore does by rule. Any other name or key is passed as
written, for the native write to accept or refuse (`string`, `int`, `float` and `bool` are
not declared natively and stay refused).

Unchanged: every native route; a plain string, int, float or bool attribute is not listed,
because nothing native records it.

## Acceptance criteria
1. A namespace written through the facade with an f16 `vector`, two text fields, a regex
   attribute, `datetime`, `uint` and `[]datetime` attributes and `uint` ids answers exactly the
   expected vendor schema, each analyzer the native one; `metadata()`'s `schema` is the same.
2. A namespace with a named f32 field and a primary text field answers both, the index's
   metric under each `ann`.
3. The vendor's declarations above are accepted (they were refused before).
4. The pinned SDK reads `schema()` as typed models: the vector's `type` and `ann`, the text
   field's analyzer and `None` type, the id's kind.

## Test plan
1, 2, 3. `facade_schema_lists_every_recorded_declaration` in `tests/it/facade.rs`, red before
   (the write refused). Mutations: `uint64` not renamed; `[]datetime` not mapped; `ann: true`
   kept; the primary text field given a type; metadata's old schema; a named field typed at
   `vector`'s width.
4. `clients/turbopuffer/test_sdk.py`, test_the_sdk_against_the_facade, red before (`[]`).

## RA budget
Unchanged: the native summary's one read, translated.

## Risks
- A text field holding string arrays reads as no type, not `[]string`. Recording it would be a
  format change for a facade nicety; listed in the manifest's note.

## Tasks
- M275.1 the spec, the translation both ways, the tests, the manifest note, VERIFIED.
