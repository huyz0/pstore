# M9b — Filters: a query answers only the documents a predicate admits

**Serves:** **D-16** (filtering composes with the clustered index, with an exact fallback)
and D-34. Second of M9 ([`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md)).

## Delta

**Wire.** `POST /v1/indexes/{index}/query` accepts `filters`, turbopuffer's spelling:
- `[attr, op, value]` with `op` one of `Eq NotEq In NotIn Lt Lte Gt Gte`;
- `["And", [f, …]]`, `["Or", [f, …]]`, `["Not", f]`, nesting freely.
`attr` is an attribute name, or `id`, which **always** means the document id: an attribute
named `id` is now refused at the write door (⚠️ amended at spec review — otherwise
`["id","Eq","x"]` had two meanings). Values are JSON integers (`i64`) or
strings; `In`/`NotIn` take an array of them. Anything else is `400 bad_request` naming what is
wrong, before any request is issued.

**Semantics**, exact, per document:
- `Eq v`: the attribute is present and equal (same type). `Eq null`: the attribute is **absent**.
- `NotEq v` is `Not(Eq v)` — so it admits documents lacking the attribute; `NotEq null`: present.
- `In [vs]`: equal to one of them; `NotIn` is `Not(In)`.
- `Lt Lte Gt Gte`: integers numerically, strings by bytes; a type mismatch or an absent
  attribute is **false**. `null` is refused here.
- `And []` is true, `Or []` false. `null` inside `In`/`NotIn` is refused.

**Engine.** Filtering is **before the limit, never after it**: post-filtering a top-`k` returns
fewer than `k` whenever the predicate is selective, silently. So, per segment:
1. **The mask.** The segment's data blocks — minus those a zone map proves cannot match — are
   read and each row's attributes evaluated: the set of rows the predicate admits. ⚠️ A zone
   is `(min, max)` over the rows holding an **integer** under that name, and says nothing of
   rows lacking it or holding a string. So only integer `Eq`/`In`/`Lt`/`Lte`/`Gt`/`Gte` prune;
   strings, `Eq null`, and **anything under `Not`** never do (`Not` of "cannot match" is not
   "every row matches"); `And` prunes if a child does, `Or` if all do. A missing zone — a
   zone-free segment (M9a) included — never prunes. One
   `get_ranges`, issued **concurrently with the legs**: its ranges are known once the segment is
   open, exactly as the legs' are, so it adds **no round-trip depth**.
2. **Exhaustive legs.** With a filter, each leg returns every candidate rather than its top
   `limit`: the dense leg probes **every** list (D-16's `p_effective` at its clamp — `p` costs
   bytes, not depth, `filtering.md`) and keeps them all (`k` = the segment's row count; the
   ladder then keeps every candidate whatever `oversample` is); text and sparse keep every
   scored row.
3. Each leg's hits are masked, **then** truncated to `limit`, then fused.
The unfolded rows are the fresh segment and go through the same path, and `as_of` takes the
filter too.

**Does not change:** any unfiltered query — same legs, same limits, same requests; the format.
⚠️ A filtered answer is **not** "the unfiltered answer minus rejected rows" — that is the
post-filter this milestone refuses — and under RRF, masking shifts ranks within each leg, so
fused order can differ (amended at spec review).

## Acceptance criteria

1. Each operator, `And`/`Or`/`Not`, `Eq null`/`NotEq null`, a cross-type comparison, and `id`
   answers with exactly the documents the semantics above admit, over unfolded rows, after a
   fold, and with `as_of`.
2. A selective filter on a **clustered** segment with replication (`replicas: 1`) returns
   `top_k` matches whose rows sit in lists the unfiltered probe does not reach.
3. A filtered dense, text and hybrid query each return only admitted documents. **Per leg**: a
   single-leg filtered answer equals the same leg run with `limit ≥ rows`, restricted to
   admitted rows, then truncated to `limit`. A block holding `{n: 5}` and `{}` is not pruned by
   `NotEq n 5`.
4. A filtered query's **round-trip depth** equals the unfiltered query's; an unfiltered query's
   request count is unchanged.
5. Malformed filters — unknown op, wrong arity, float or bool value, `null` with `Lt`, `In`
   without an array, `null` in `In` — and an attribute named `id` on write, are
   `400 bad_request`.
6. `./scripts/gates.sh` passes; `./scripts/mutants.sh` over the changed lines misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | no `filters` field (ignored) | an operator's comparison flipped; absent treated as present; comparing through `Value`'s `Ord` (`Int < Str`) |
| 2 | the post-filter shape | a leg truncated before the mask; `p` not raised |
| 3 | as 1 | the mask applied to one leg only; `Not` pruning as `!could_admit` |
| 4 | the mask fetched after the legs | a sequential mask round |
| 5 | accepted today | coercion, silent drop of a malformed clause |

## RA budget

Depth unchanged: the mask's one `get_ranges` per segment rides the legs' round. Requests: at
most +1 Rpar per segment per filtered query (0 when every block is pruned), same round. Bytes
grow with a filter: every list's codes, and the unpruned blocks — which carry the `text` body.
The resolve round then refetches blocks the mask already held; reusing them is left undone.

## Risks

- Exhaustive legs on a large clustered segment read every list: correct, and priced in bytes.
  D-16's selectivity planner (and D-17's feedback) is what trims it; not this milestone.
- String zone maps do not exist, so a string predicate prunes no block.
- Exhaustive `Rerank::Fast` finds each candidate's list by a linear scan of the probed lists:
  O(rows × lists) CPU per segment per filtered query. Unmeasured here; D-16's planner bounds it.

## Tasks

- **M9b.1** — the predicate, its parser, the mask, exhaustive legs, the tests, this ledger.
