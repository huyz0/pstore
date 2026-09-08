# M5b — `prefetch[]`, `fusion`, and the concurrency that keeps depth flat

**Serves:** **D-73** (ship the `prefetch[]` + `fusion` request shape in v1 even though only one
retriever existed) and **D-27** (RRF by default, `k = 60`, weights only once measured).

**Depends on:** [M5a](../M5a/SPEC.md), which gives fusion a second retriever to fuse. Before it
there was nothing to fuse *with*, which is why D-73's shape had never been exercised.

## ⚠️ Why this is its own milestone

The roadmap folds fusion into M5a. Split, because M5a is six commits of layout, engine and
retrieval before a single line of ranking exists, and a spec whose criteria cannot survive two
review rounds is one that gets amended mid-implementation. The corpus's own sequencing
argument — sparse first *because* it is small — applies again one level down.

## ⚠️ The failure this milestone exists to prevent

D-73 is about a **request shape**, not an algorithm: *"The API is the hardest thing to change
later — harder than the format, because customers' code depends on it."* So the shape ships
with a retriever it does not yet have (`Text`), and that retriever is **refused by name**. A
`prefetch` entry silently dropped is worse than an error: the caller gets a plausible ranking
computed from half the retrievers they asked for.

The second failure is arithmetic disguised as concurrency. Two legs awaited in sequence give
the same answer as two legs joined, at twice the depth, and **every functional test passes**.
`store.rs:109` makes the same point about ranges: "width is free; depth is not".

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| RRF `k` | **60** (D-27), exposed as a parameter | OQ-63 reports `k = 10` as a tuned value in current practice. It is **not decided here** — `k` is a parameter so it can be decided by measurement later. |
| Fusion default | **unweighted RRF** | D-27: "equal weights until the customer has measured". Weighted RRF and score fusion are named options, not v1 defaults. |
| Hybrid depth | **3 from `HEAD`**, the same as a dense query alone | The whole claim. Two legs must cost the *max*, not the sum (which is 5). |
| Segment opens per hybrid query | **1** | Measured on `Accounted` over `MemoryStore` with **no cache in the stack**: `pstore-cache` singleflights identical concurrent reads (`cache.rs:145`), so a cached stack passes this criterion for code that opens twice. |

## Delta

**Adds**
- **`pstore-query`** (layer 4, already reserved in the workspace root's comments).
- `Prefetch` — `Dense { field, query, limit }`, `Sparse { field, query, limit }`, and `Text`,
  which exists in the type and is **refused at execution** with its own error.
- `Fusion::Rrf { k }`, and `fuse(&[Vec<Hit>], Fusion) -> Vec<Hit>` as pure ranking, no I/O.
- `query(store, keys, &[Prefetch], Fusion, top_k)` — opens the segment **once**, runs its legs
  concurrently over that open, and fuses.

**Does not add** — weighted RRF or score fusion (D-27: options, not defaults); OQ-63's `k`
answer, which needs an eval set that does not exist until full-text; **cross-segment fusion**
— legs fuse by row within one segment, and a stable cross-segment id is full-text's two-pass
IDF problem; reranking (D-29: a different product); the HTTP surface — this is the shape as
Rust types, and `api-design.md`'s JSON is a serialization of it, not a server.

## Acceptance criteria

1. RRF equals `Σ 1/(k + rank)` on a hand-computed case, `k` a parameter defaulting to 60.
2. A row returned by **both** legs outranks a row returned by one at the same best rank.
3. Fusion is independent of leg order: shuffling the prefetch list gives an identical ranking,
   ties included.
4. A hybrid query over one segment reads its footer **once** — exactly 1 `OpClass::Read` on the
   segment key in the open round, on the pinned uncached stack.
5. A hybrid query's sequential depth is **3 from `HEAD`** — the max of its legs, not the sum.
6. An unimplemented prefetch kind is refused with its own error, never silently dropped, and
   the error names the retriever.
7. A leg that matches nothing contributes nothing and is not an error; a query whose legs all
   match nothing returns an empty ranking rather than failing.
8. `limit` is per leg and `top_k` is over the fused list: a leg limited to 5 contributes at
   most 5 rows, and the fused answer is `top_k` long or shorter.
9. Region coverage ≥95% and mutation ≥80% on `pstore-query`, full gate set green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `rrf_is_the_reciprocal_rank_sum` | `1/rank` for `1/(k+rank)`; `k` ignored; ranks taken 1-based one place and 0-based another |
| 2 | `agreement_between_legs_outranks_a_single_leg` | fusion taking `max` of the contributions instead of the sum |
| 3 | `fusion_does_not_depend_on_leg_order` | ties broken by insertion order |
| 4 | `a_hybrid_query_opens_the_segment_once` | each leg opening independently — which passes 5 |
| 5 | `a_hybrid_query_is_no_deeper_than_its_deepest_leg` | `.await` per leg in sequence instead of a join |
| 6 | `an_unimplemented_retriever_is_refused` | the arm falling through to `Ok(vec![])` |
| 7 | `an_empty_leg_is_not_an_error` | `?` on an empty result; returning `UnknownField` for a field with no matches |
| 8 | `a_legs_limit_bounds_only_that_leg` | `limit` applied after fusion, so one leg's cheap rows crowd out the other's |

## RA budget

Measured **from `HEAD`**, as M3 and M5a are. *t* is the query's dictionary hits, *p* the dense
leg's probe count.

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Hybrid dense+sparse, cold | 0 | **3** — HEAD (1) ∥ {footer, dictionary, centroids} (2) ∥ both legs' ranges (3) | 1 + 3 + *t* + *p* (+ *p* again unless `rerank: none`) | 0 |
| Hybrid, warm | 0 | **1** | *t* + *p* | 0 |
| One leg | 0 | unchanged from that leg's own budget | — | 0 |

## Risks

- **Depth is the invariant, and it is invisible to every functional test.** Criterion 5 is the
  only thing standing between "concurrent" and "sequential and correct". It is asserted with
  the depth-counting store, not read from the code.
- **RRF discards score magnitude**, which is the price D-27 accepts for stability across score
  drift. A retriever that knows A is *far* better than B cannot say so. Named, not fixed;
  weighted fusion is the escape hatch and it is deliberately not the default.
- **Fusion by row ends at the segment boundary.** Every criterion here is single-segment, and
  nothing in them would fail if the multi-segment case were wrong — because it does not exist.
  Stated so it is a task later rather than a discovery.

## Tasks

| Id | Commit |
|---|---|
| **M5b.1** | `pstore-query`: the `prefetch[]` + `fusion` types, RRF as pure ranking, and the refusal that names the retriever |
| **M5b.2** | The concurrent `query` entry point — one open, depth flat, order-independent |
