# M135: `AnyLt`, `AnyLte`, `AnyGt` and `AnyGte` over array attributes

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "`AnyLt/AnyLte/
AnyGt/AnyGte` ... match exhaustive reference predicates for missing/null/empty arrays and
nested negation." The [comparison](../../comparisons/turbopuffer-2026-10-25.md) lists them as
missing from the HTTP parser.

## Delta
Today a filter clause `[attr, "AnyLt", v]` is `400 malformed filter` (unknown operator).

After: `[attr, op, v]` with `op` one of `AnyLt`, `AnyLte`, `AnyGt`, `AnyGte` is
`Predicate::AnyCmp(attr, Lt | Lte | Gt | Gte, v)`.

- **Meaning.** True when the attribute is an array holding at least one element `e` with
  `e op v`, compared exactly as the scalar `Lt`/`Lte`/`Gt`/`Gte` compare (`compare`: numbers
  as one line, strings by bytes, bools `false < true`, datetimes by instant, an RFC 3339
  string literal against a datetime element). An element of another type is false for that
  element only.
- **Not an array.** An absent attribute (null is absent, M9a), a scalar, and an empty array
  are false. So `Not(AnyLt)` admits them, as `NotContains` admits rows without the array.
- **The literal** is what `Lt` accepts: a scalar. An array, an object or `null` is
  `400 malformed filter`, as for `Contains`.
- **Where it applies.** Everywhere a filter does, because they share the parser and
  `Predicate`: query filters, `patch_by_filter`, `delete_by_filter`, and write conditions.
  A condition carries it in the M13 binary form under a new tag, `12`, followed by the
  attribute, the operator code (1 to 4; `Eq`'s 0 decodes as `None`, so `holds` never sees
  `Eq`) and a scalar value (an array decodes as `None`). An older fold reads tag 12 as
  `None` (its decode rule for an unknown tag) -- see Risks.
- **Pruning.** `could_admit` is `true`: an array has no zone (M9h.2).
- **Peers.** A peer part carries the filter's JSON; a peer of an older build answers 400 for
  the unknown operator, and the coordinator runs that share itself (M54's rule).

Unchanged: every existing operator, `Contains*` included; the scalar `Lt`/`Lte`/`Gt`/`Gte`,
which stay false on an array attribute; no `AnyEq`, which is `Contains`; the analyzer, token
and pattern predicates; storage formats. `$ref_new` stays refused as the value of an `Any*`
operator, as under every operator but the six M133 names.

## Acceptance criteria
1. **Oracle.** Over a corpus of rows built from every combination of: the attribute absent,
   a scalar int, float, string, bool and datetime, an empty array, and arrays of one or two
   elements drawn from those scalars, and literals of each type equal to an element, between
   elements, and outside them (an int literal against float elements and the reverse; both
   bools; RFC 3339 strings against the datetime): `AnyCmp` admits exactly the rows a direct reference (`any` over
   the elements of an array, else false) admits. So do `Not(AnyCmp)`, `Not(Not(AnyCmp))`, and
   `And`/`Or` of an `AnyCmp` with a `Contains` on the same attribute.
2. **HTTP.** Over folded and unfolded rows, each of the four operators returns exactly the
   expected ids, over number arrays and string arrays (compared by bytes), and
   `["Not", [attr, "AnyGt", v]]` returns the absent, scalar and empty-array rows, and the
   string arrays. Literals equal to an element tell `Lt` from `Lte` and `Gt` from `Gte`. (The API
   has no datetime arrays, M9h.3; criterion 1 covers datetime elements.)
3. **Refusals.** `AnyLt` with `[1]`, `null`, or `{"k": 1}` is `400` for its literal, not
   for an unknown operator.
4. **Conditions.** An `AnyCmp` round-trips through the condition encoding, and every
   truncation of it, operator code 0 or 5, an array literal, and tag 13 decode as `None`.
5. **By filter.** `delete_by_filter` with `["n", "AnyGte", 5]`, over rows folded and not,
   deletes exactly the rows with an element at least 5.
6. **Pruning.** Over the typed-pruning corpus (arrays beside scalars outside the block's
   zone), `AnyCmp` and `Not(AnyCmp)` admit through the zones what they admit by brute force.

Spec review: one round, eight findings, all taken (the by-filter fold risk, tag-13 and
operator-code decoding, a red criterion 3, pruning as criterion 6, equal literals).

## Test plan
Red first against the parent: 1, 4 and 6 do not compile there (no `AnyCmp`); 2 and 5
answer 400 for the unknown operator; 3 fails on the parent's "unknown operator" text.
- 1: `pstore-query` unit test `any_comparisons_match_the_reference`. Mutations: "a scalar
  attribute is compared as a one-element array", "`all` instead of `any`", "an empty array
  is true", "`Lt` evaluated as `Lte`" (each operator swapped with its neighbour).
- 2: `crates/pstore-server/tests/it/arrays.rs`, `any_comparisons_find_an_element_by_order`.
  Mutations: each operator's parse arm swapped with another's.
- 3: `what_an_array_cannot_hold_is_refused`, extended. Mutation: "the literal goes through
  `serde` as a `Value` without the scalar check".
- 4: `every_predicate_round_trips` and `every_truncation_and_every_unknown_tag_is_none`,
  extended (its unknown tag moves from 12 to 13). Mutations: "operator code written as
  `Eq`", "tag 12 accepts `Eq`", "tag 12 accepts an array literal".
- 5: `crates/pstore-server/tests/it/arrays.rs`, `delete_by_filter_takes_any_comparisons`.
  Mutation: none beyond 2's; it proves the by-filter path uses the same parser.
- 6: `crates/pstore-query/tests/it/typed_pruning.rs`, its `predicates()` extended. Mutation:
  "`AnyCmp` prunes on the scalar zone".

## RA budget
Unchanged: no new blob request. The predicate is evaluated on rows a mask already decodes
(`names()` includes the attribute), and it never prunes a block.

## Risks
- **Vendor semantics for a scalar attribute.** turbopuffer types `Any*` over array
  attributes; whether it errors on a scalar attribute is not pinned in its docs. False is the
  reading consistent with `Contains`. A conformance fixture against the vendor would reveal a
  difference.
- **An older fold skips the operation.** A condition or by-filter operation holding tag 12,
  folded by a build before this one, decodes as `None`, and a `None` condition admits
  nothing (`Conditions::of`): the write is skipped, not applied, and nothing reports it. M15
  and M66 added tags with the same exposure. pstore makes no mixed-version fold promise yet
  (rolling upgrades are workstream I); that promise must cover every tag.
- **Mixed-type arrays.** pstore stores arrays as written; an element of another type is
  skipped, never an error. The oracle pins this.

## Tasks
- M135.1: `Predicate::AnyCmp`, evaluation, pruning, names, and the condition tag, with
  criteria 1, 4 and 6.
- M135.2: the HTTP parser arms, with criteria 2, 3 and 5.
