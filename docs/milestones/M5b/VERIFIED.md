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
   `run.rs` 95.48%; workspace regions 95.28%. Mutation: **15 of 15 viable mutants caught**
   across `fuse.rs` and `run.rs`, zero missed — see below.


## Mutation

`./scripts/mutants.sh --file …` **in the `dev` container** — 6 CPUs and 16 GB, which is where
`dev/README.md` says the heavy work belongs — over M5's six new modules: **469 mutants,
411 caught, 42 unviable, 16 missed, zero timeouts**. 411 of 427 viable, and every one of the
16 is a **proven equivalent mutant**, recorded beside the code it lives in.

| module | caught | missed | unviable |
|---|---|---|---|
| `pstore-format/src/sparse.rs` | 228 | 16 | 8 |
| `pstore-format/src/text.rs` | 80 | 0 | 4 |
| `pstore-index/src/sparse.rs` | 19 | 0 | 13 |
| `pstore-index/src/text.rs` | 69 | 0 | 12 |
| `pstore-query/src/fuse.rs` | 9 | 0 | 1 |
| `pstore-query/src/run.rs` | 6 | 0 | 4 |

The 16: fourteen `|` against `^` over the **disjoint** sign, exponent and significand bits of
a binary16 — OR and XOR agree on every input that can reach them — and two in `put_impact`
(`max > 0.0` against `>= 0.0`, whose only reachable input is a term of all-zero impacts where
`0.0 / 0.0` casts to the same byte; `+ 128` against `- 128`, which differ by 256 in a `u8`).
**Excluding them, 411 of 411.**

⚠️ **Zero timeouts, and that number had to be earned.** `put_varint` with its loop condition
flipped does not terminate, and an earlier host run reported it as a timeout — a kill, but a
slow one. The floor of 300 s is what keeps a *survivor* from being filed the same way; see
below.

⚠️ **The first sweep's numbers were wrong in the flattering direction**, and the correction is
the point. `cargo-mutants` auto-set the limit to **20 s against a 20-second baseline**, so
every *surviving* mutant — which by definition runs the suite to completion — was cut off and
filed as a timeout. Three of those looked like the equivalence class above and were not: they
were the round-to-nearest tie term, and chasing them found the codec rounding ties **up** while
its comment claimed round-to-nearest. Both paths are IEEE round-half-to-even now.

## Named, not built

- **Weighted RRF and score fusion.** D-27 lists them as options and says "equal weights until
  the customer has measured". There is no eval set until full-text, so there is nothing to
  measure with.
- **OQ-63's `k`.** 60 ships; 10 is reported as a tuned value in current practice. `k` is a
  parameter precisely so that stays a measurement rather than a decision made here.
- **Cross-segment fusion.** Every criterion here is single-segment, and none of them would
  fail if the multi-segment case were wrong — because it does not exist yet.
