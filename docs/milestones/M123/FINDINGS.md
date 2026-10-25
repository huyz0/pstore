# Review of current main

Base: `4654acb` (M122). The initial checkout was at `0337112`; its review edits
were stashed before fast-forwarding. Findings already fixed upstream were not
reapplied. This ledger describes findings reproduced on current main.

| Finding | Effect | Fix and regression |
|---|---|---|
| Congested changes a limit counter but admits every request | A throttled backend still receives unrestricted concurrent attempts | M123: cancellation-safe admission shared by ordinary and conditional operations; `concurrency_admission_blocks_and_releases_on_cancellation` |
| Conditional writes bypass admission | WAL/metadata traffic could exceed the read-side limit | M123: same admission, preserving no-retry semantics; `releasing_capacity_wakes_a_conditional_write_waiter` |
| Zero ceiling and integer counter edges | A zero limit cannot recover consistently; long-lived counters and maximum recovery can panic in checked builds | M123: normalize zero, wrap the success counter, saturate recovery; three integer-edge regressions |
| Probe windows add to an untrusted sequence without checking | Corrupt metadata near u64::MAX panics in checked builds or wraps in release | M124: bound the window before addition and refuse exhaustion; `tail_probing_does_not_wrap_at_sequence_exhaustion` |
| Registration writes a registry larger than its decoder accepts | A 4097th lane makes every lane unreadable to recovery | M124: refuse growth at capacity, preserving all existing lanes; `registering_past_the_lane_ceiling_preserves_the_registry` |
| Ranked prefetch limits are per segment before fusion | More than the requested number of candidates influence RRF/max; ranking depends on segmentation | M125: select each leg's global prefix after shadow filtering; `prefetch_limit_applies_to_the_global_union` |

All named regressions were observed failing on current main before their fixes.
The individual milestone ledgers carry the commands and final validation status.

## Architecture and measurement scope

The review traced WAL create-if-absent, recovery discovery, cache claims, request
admission, cross-segment fusion, and their existing verification. M17 already
protects WAL collisions, M28 releases cancelled cache owners, and M119 already
uses top-k selection for fusion. Those obsolete findings needed no new patch.

No fix adds a blob request or LIST. M123 queues locally rather than changing the
retry budget. M124 refuses invalid growth before its write. M125 reduces ranked
fusion inputs without adding an I/O stage. Sum retains its whole-leg contract.

The release server benchmark uses 48,000 rows across 24 segments, 64 dimensions,
200 measured queries per kind after 10 warmups. The separate admission benchmark
isolates an in-memory head call. Both are provisional WSL2 measurements; neither
is a cloud latency, fleet throughput, or monthly dollar estimate. The benchmark
and architecture gate results are recorded in the verification ledgers.
