# M203: numeric scoring expressions: `Attribute`, literals, `Saturate`, `Decay` and `Dist`

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Represent
... Attribute, Saturate, Decay and Dist as a typed expression tree. Whole-corpus results
match a small exhaustive oracle, including zero-score exclusion and invalid negative score
domains". The rest of [M182](../M182/SPEC.md)'s E row. No D- or OQ- id covers it.

The vendor's [query reference](https://turbopuffer.com/docs/query), read 2026-11-09:
- `["Attribute", "clicks"]` scores a row by its value. "turbopuffer will reject `Attribute`
  operators applied on signed types such as int and float without the Max operator", as in
  `["Max", [0, ["Attribute", "clicks"]]]`.
- `["Saturate", e, {"midpoint": m, "exponent": p}]` is `x^p / (x^p + m^p)`, `exponent`
  defaulting to 1; it "implicitly takes the max of attribute values with zero".
- `Decay` "takes the same configuration parameters and is implemented as
  `Decay(x) = midpoint^exponent / (x^exponent + midpoint^exponent)`".
- `["Dist", ["Attribute", "published_at"], "2026-02-03T12:13:14"]` is "the distance between an
  origin point and an attribute value". On a datetime field "the `midpoint` can be provided
  either as a number of milliseconds, or as a duration string" in `s`, `m`, `h`, `d`, `w`.
- Missing values, an exponent's range, and Dist over numbers are not documented.

## Delta
1. **`pstore_query::Expr` gains five nodes**, each counting toward M182's 64 nodes and depth 8:
   - `Attribute(name)`: the row's value when it is an int, a `uint64` or a float, else **no
     value** (missing, a string, a bool, a datetime, an array). A float beyond `±1e30` reads
     as `±1e30`; every int and `uint64` is within it.
   - `Literal(f64)`: a number, finite, `|n| <= 1e30`.
   - `Saturate(e, m, p)` and `Decay(e, m, p)`: with `x = max(e, 0)`, `x^p / (x^p + m^p)` and
     `m^p / (x^p + m^p)`, evaluated as `1 / (1 + (m/x)^p)` and `1 / (1 + (x/m)^p)` so no
     power overflows. `Saturate(0) = 0`, `Decay(0) = 1`. **No value scores 0 under both**: a
     row without the attribute gets no boost.
   - `Dist(attribute, origin)`: the first argument is an `Attribute`. With a number origin
     (`|o| <= 1e30`), `|v - o|` for a numeric value; with a datetime origin, `|v - o|` in
     **milliseconds** for a datetime value. Any other value is no value.
   - Elsewhere no value is `0`: in `Sum`, `Max`, `Product`, and at the root.
2. **Parameters.** `midpoint` is a number in `[1e-9, 1e30]`, or a duration string `<n><unit>`,
   `n` a decimal of up to 15 digits with an optional fraction, `unit` one of `s m h d w`,
   read as milliseconds (`"6h"` is `21,600,000`) and then held to the same range (`"0s"` is
   refused). `exponent` is a number in `(0, 16]`,
   default 1, a narrowing of the undocumented. A datetime origin is an RFC 3339 instant;
   one with no offset, as the vendor's example is, is UTC.
3. **Signed domains.** Each node is non-negative or signed, by its form alone:
   - non-negative: `BM25`, a filter, `Saturate`, `Decay`, `Dist`, a literal `>= 0`, a `Sum`
     of non-negative clauses, `Product(w, e)` of a non-negative `e`, a `Max` with **any**
     non-negative clause (it is at least that clause);
   - signed: `Attribute`, a negative literal, anything else.
   - **The root must be non-negative**, refused `400` otherwise. So every kept score is
     `>= 0`, as in M182, and the vendor's "reject ... without the Max operator" holds even
     for a `uint64` attribute, a deliberate narrowing: the type of a field is not read.
4. **Evaluation in `f64`.** `Max` takes the largest of its clauses, signed ones included
   (M182 folded from 0, the same answer while every clause was `>= 0`). With every leaf
   bounded by `2e30` and M182's limits (weights at most `1e3`, 16 clauses, depth 8), every
   intermediate is below `1e63`: finite, never NaN. The wire's `f32` score is the `f64`
   narrowed and clamped into `[f32 smallest positive, f32::MAX]`, so a kept row never
   reports `0` (M182's guarantee, which a literal of `1e-50` would otherwise break); rows
   are ordered by the `f64`.
5. **Kept rows** are M182's: a score exactly `0.0` is dropped, then score descending, id
   ascending. `Max([0, Attribute a])` keeps exactly the rows whose `a > 0`.
6. **HTTP.** `["Attribute", name]`, a JSON number as a clause, `["Saturate", e, params]`,
   `["Decay", e, params]` (params an object holding `midpoint` and optionally `exponent`),
   `["Dist", ["Attribute", name], origin]`. The three are read so only when their second
   element is an array, so a filter `["Dist", "Eq", 3]` on an attribute named `Dist` stays a
   filter. Any pair of two strings stays an order, as in M182: `["Attribute", "asc"]` orders
   by an attribute named `Attribute`, and `["Attribute", "clicks"]` is refused (a bare
   `Attribute` root is signed anyway). A root that is not an array is refused, as before.
   In `Product`, the first number is the weight. No value is a property of the clause
   itself: `Decay(Attribute n)` with `n` missing scores `0`, but `Decay(Sum([Attribute n]))`
   scores `1`, as the `Sum` is `0`.
7. **Refused `400`:** a signed root; a literal not finite or beyond `1e30`; a midpoint or
   exponent out of range, missing, not a number, or a malformed duration; an unknown params
   key; `Dist` whose first argument is not an `Attribute`, or whose origin is neither a
   number nor a datetime string; a wrong arity for any of the five.

**Not changed:** M182's nodes, limits, scan and rounds; `rank_by` orders; native `boost`.

**Not covered:** geo origins (no geo type exists); `Dist` over an arbitrary expression;
`Attribute` of a datetime outside `Dist`; pruning (WAND); `compute_attributes` (F).

## Acceptance criteria
1. **Oracle.** `numeric_expressions_match_the_oracle`, in `crates/pstore-query/tests/it/`:
   M182's corpus with a signed int `n`, a float `f` and a datetime `at`, each missing on some
   rows and `n` a string on a few. 300 seeded trees over every node kind (M182's and the
   five), up to depth 5, root non-negative. A `Saturate` or `Decay` argument is an
   `Attribute`, a `Dist`, a literal, or the sum of an attribute and a literal, so its
   magnitude stays in `1e-3..1e6`. The reference evaluates the vendor's formulas as
   written (`x^p / (x^p + m^p)`), from the raw values. Kept set exact, scores within `1e-4`
   relative, order as M182's AC1.
2. **Signed domains.** `signed_roots_are_refused`: `Attribute`, a negative literal, `Sum` of
   a signed clause, `Product` of one, `Max` of only signed clauses are refused by `check`;
   `Max([0, Attribute])`, `Saturate(Attribute)` and `Max` of a signed and a BM25 clause pass.
3. **Zero and no value.** `missing_values_score_zero`: `Max([0, Attribute n])` keeps exactly
   the rows with `n > 0`; `Decay(Attribute n)` keeps none of the rows lacking `n` and scores
   exactly 1 for `n <= 0`; `Dist` with a number origin gives no value for a datetime; a float
   of `1e300` reads as `1e30`. `wire_scores_stay_positive`: a score reports the
   smallest positive `f32` for `1e-50`, a positive subnormal for `1e-40`, and `1e60` reports `f32::MAX`.
4. **HTTP.** `numeric_expressions_over_http`, in `crates/pstore-server/tests/it/`: the
   vendor's three examples (Saturate of clicks, Decay of negative reviews, Dist with `"6h"`)
   answer as their definitions compute; a datetime origin with and without `Z` agree;
   `["Attribute", "asc"]` is still an order; each refusal in Delta 7 is `400`.
5. **Depth.** M182's `an_expression_query_takes_three_rounds` with an `Attribute` clause:
   3 rounds, no extra request.
6. **Gates:** fmt, clippy, the workspace tests (the two IPv6 tests NOT-RUN in this
   container), and the named mutations below each observed red.

## Test plan
Each test is seen red first: `rank_by` refuses the new nodes on the parent.

| AC | Mutation |
|---|---|
| 1 | `Saturate` as `Decay`; the exponent ignored; `Max` folding from 0; `Dist` without `abs`; no value as 0 under `Saturate` |
| 2 | `Max` non-negative only when every clause is; `Product` of a signed clause non-negative |
| 3 | `Saturate`/`Decay` not clamping a negative to 0; a datetime as a number under `Dist`; the float clamp removed; the wire narrowing unclamped |
| 4 | a duration's unit ignored; an offset-less origin refused; `["Attribute", x]` always an expression |
| 5 | an attribute read issuing its own request |

## RA budget
Unchanged from M182: W 0, List 0, Rseq 3, Rpar per segment its footer, delete vector, one
dictionary per BM25 field, and one coalesced block plan. Attributes come from the blocks the
scan reads anyway.

## Risks
- **Undocumented corners.** Missing values, exponents and number origins are our reading;
  a pinned-SDK comparison (B) could find them different. Each is stated in `docs/deploy.md`.
- **Float order.** The reference's direct formula and the stable one differ in the last
  bits; AC1 compares within `1e-4`. Kept sets agree over the oracle's arguments (`|x|` and
  `m` within `1e-3..1e6`, AC1), where both forms are `0` exactly at `x = 0` and positive elsewhere.
  At extremes the stable form can underflow to `0` (`Decay` of `1e30` with a midpoint of
  `1e-9`), and that row is dropped, as M182 drops an `f32` idf that rounds to `0`.

## Tasks
- **M203.1** The spec, reviewed.
- **M203.2** The five nodes, the domain check, HTTP, tests, docs.
