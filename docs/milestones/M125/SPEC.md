# M125: global ranked prefetch limits

## Serves
D-27 rank fusion and D-73 prefetch request semantics.

## Delta
Apply each ranked prefetch limit to its union across segments, after shadow removal
and global score ordering, before RRF or max fusion. Select only that prefix using
the existing deterministic top-k helper. Sum fusion keeps its documented whole-leg
semantics and existing separate path.

## Acceptance criteria
1. With two matching segments, a prefetch limit of one contributes exactly one globally best hit, including deterministic tie-breaking.
2. Existing fusion, shadow, distributed-query, and sum tests pass. No blob request or round is added.
3. The local server benchmark records final digests and timings; changed hybrid results from corrected global limits are explicitly recorded.

## Test plan
1. `prefetch_limit_applies_to_the_global_union`: currently returns two hits for a global limit of one.
2. Run query, engine, server, and workspace all-feature tests and architecture depth/quality gates. Existing sum tests protect whole-leg semantics.
3. Run query-bench; timings are provisional, never a latency SLO.
Deterministic ordering and count assertions are gate-design rung 3.

## RA budget
No I/O added, zero LIST, unchanged sequential depth. Global prefix selection
reduces CPU and fusion memory; per-segment candidate fetching remains unchanged.

## Risks
Hybrid results may change because excess candidates no longer influence fusion.
The cut follows shadow filtering, so a newer row cannot displace the next live hit.

## Tasks
- M125.1 Enforce ranked global limits and record their ranking effect.
