# M123: enforce adaptive blob concurrency

## Serves
D-1/D-2 blob request control; OQ-116 adaptive concurrency and retry interaction.

## Delta
Enforce Congested's existing AIMD limit at backend admission, including conditional
writes. Release capacity on completion and cancellation. Preserve retry budgets,
class forwarding, stat forwarding, and the conditional-write no-retry policy.
Normalize ceiling zero to one. Update AIMD counters atomically without overflow at the integer boundary.

## Acceptance criteria
1. At ceiling one, a second operation stays queued until the first completes or is cancelled.
2. A reduced limit refuses new admission while occupancy is at the limit; zero and maximum integer settings and success-counter rollover do not strand or panic.
3. Capacity release wakes a queued conditional write; it cannot bypass admission.
4. Existing blob and workspace tests and fast gates pass; the query benchmark's answer digests remain unchanged.

## Test plan
1. `concurrency_admission_blocks_and_releases_on_cancellation`: poll two operations; the unfixed second enters immediately.
2. `a_reduced_limit_blocks_new_admissions`: occupy two slots, reduce to one, release one; a third must remain Pending.
   Also run `zero_concurrency_still_allows_progress`, `recovery_at_the_largest_ceiling_does_not_overflow`, and `the_success_counter_can_cross_its_integer_boundary`.
3. `releasing_capacity_wakes_a_conditional_write_waiter`: a real Wake records notification; conditional writes stay queued until cancellation releases a slot.
4. Run blob tests, all-feature workspace tests, fast gates, and query-bench before/after without concurrent builds.
Deterministic admission tests are gate-design rung 3. Timings are provisional.

## RA budget
No new blob requests or LIST; no added sequential blob dependency.
Waiting for local capacity is cancellable. Admitted operations drain after a cut.

## Risks
Register wakeups before checking occupancy. A release between those steps must not
strand a waiter. A local limiter does not promise global or per-prefix fairness.

## Tasks
- M123.1 Enforce admission and record review and measurement evidence.
