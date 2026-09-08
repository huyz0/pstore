# M5b — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

1. **RRF is `Σ 1/(k + rank)`** — `rrf_is_the_reciprocal_rank_sum`
   (`cargo test -p pstore-query --test fusion`), hand-computed: a row ranked 1st and 2nd
   scores 1/61 + 1/62 = 0.032522, a row appearing once scores 1/63 = 0.015873. Ranks are
   **1-based**; zero-based makes the gap between first and second smaller than between any
   other pair. `k_is_a_parameter_and_changes_the_answer` pins `k` itself, on a fixture built
   on the trade `k` controls — at any fixed `k` the *order* of a two-leg fusion is usually
   unchanged, so a test that only checks order cannot see `k` being ignored.
2. **Agreement outranks a single leg** — `agreement_between_legs_outranks_a_single_leg`: a row
   second in both legs beats a row first in one and absent from the other. This is the
   property that makes hybrid worth having, and `max` instead of `+` passes every other test
   in the file.
3. **Fusion is order-independent** — `fusion_does_not_depend_on_leg_order`, three legs
   shuffled, rows **and scores** compared.
4. **One open per hybrid query** — `a_hybrid_query_opens_the_segment_once`
   (`--test hybrid`), counting reads whose span ends at the end of the object, which is what
   `Segment::open` alone does. ⚠️ Measured on `Accounted` over `MemoryStore` with **no cache**:
   `pstore-cache` singleflights identical concurrent reads, so with a cache in the stack the
   wrong code passes. Observed red by giving the dense leg its own `Segment::open`: **2
   against 1**.
5. **Depth is the max of the legs, not the sum** — `a_hybrid_query_is_no_deeper_than_its_deepest_leg`,
   which measures each leg alone and both together. Observed red by awaiting the legs in
   sequence: **3 rounds against 2 and 2 alone**.
6. **An unimplemented retriever is refused by name** — `an_unimplemented_retriever_is_refused`
   and `a_refused_retriever_costs_no_requests`, which asserts it is refused before any I/O.
   ⚠️ **Amended after M5c.** This criterion was written against `Prefetch::Text`, which M5c
   implemented; the shape's unimplemented occupant is now `Prefetch::Trigram`, the test asserts
   the error names `"trigram"`, and `Runnable` gained a `Text` variant. **The mechanism is
   unchanged** — the type that reaches the runner still cannot express an unimplemented
   retriever, so there is one refusal rather than two matching arms — and D-73's point is that
   the shape carries retrievers that do not exist yet, so as long as one does not, this
   criterion has something to be about. Recorded rather than silently re-pointed:
   `check-verified.py` only checks that a named test *resolves*, which is exactly the drift a
   ledger exists to catch.
7. **An empty leg is not an error** — `an_empty_leg_is_not_an_error`: a sparse leg matching
   nothing leaves the dense leg's hits intact, and a query whose every leg matches nothing
   returns an empty ranking rather than failing.
8. **`limit` is per leg, `top_k` is over the fused list** — `a_legs_limit_bounds_only_that_leg`
   and `top_k_bounds_the_fused_answer_and_no_leg`.
9. **Gates** — `./scripts/gates.sh` green; `pstore-query/src/fuse.rs` **100%** regions,
   `run.rs` 95.48%; workspace regions 95.28%.

## Named, not built

- **Weighted RRF and score fusion.** D-27 lists them as options and says "equal weights until
  the customer has measured". There is no eval set until full-text, so there is nothing to
  measure with.
- **OQ-63's `k`.** 60 ships; 10 is reported as a tuned value in current practice. `k` is a
  parameter precisely so that stays a measurement rather than a decision made here.
- **Cross-segment fusion.** Every criterion here is single-segment, and none of them would
  fail if the multi-segment case were wrong — because it does not exist yet.
