# M129: applied write completion with exact outcomes

## Serves
D-34's explicit API contract; Q10's write visibility (consistency-model.md §4).
First slice of workstream C in [the delivery plan](../../comparisons/turbopuffer-next-plan.md),
per the [mutation-completion proof](../../research/11-design/mutation-completion.md).

## Delta
Add `durability: "applied"`: flush as `durable` does, then wait until HEAD's watermark for
the engine's lane reaches the engine's next sequence after that flush. The response adds
`completion: "applied"`, `operation` (128-bit hex id) and `applied`
`{upserted, deleted, patched, skipped, rejected}`, summed over every commit that folded
the request's operations, or `applied: null` with `outcome_unavailable: true` when one of
those commits' outcomes was retired or reaped. Counts are rows changed (a by-filter
operation counts every row it changed); `skipped` counts operations that changed nothing;
`rejected` counts operations a reject, width, replica or drop pass discarded.

Engine: an applied request stamps its operations with reserved attribute `$req`; the fold
resolves stamped operations against current versions, tallies them after every
set-aside pass, and strips `$req` in every `resolve` branch before sealing. A commit that
tallied anything first creates one outcome object (create-if-absent, `/out/` key from
epoch, lane and attempt, never ending `.seg`). HEAD gains a trailing list of the last 32
`(epoch, outcome key)` pairs; a pair that falls off is buried, and GC's live set includes
the list. A single-flight fold gate per engine: a waiter holding it returns if the last
HEAD read (highest epoch) covers its target, else folds. Waiters read outcome objects for
epochs in `(highest HEAD epoch this engine read before the flush, HEAD read]` in one
parallel round, through an engine-wide cache. A tallying commit has fallen off exactly
when the list is full and its oldest epoch is above that bound.

Applied mode is refused with 400 `unsupported_durability` unless the process was started
with `PSTORE_APPLIED_WRITES=1`, because older binaries would seal `$req` and drop the
list. The deferred-operation guard admits `applied` as it admits `durable`. Branch/copy
requests keep refusing any durability. Applied mode is an opt-in **outside D-34's depth
budget**, as `rerank: exact` is (proof §5).

Unchanged: `batched` and `durable` responses, costs, depth and semantics; existing
counters stay labelled as submitted; no exactly-once claim, client-supplied id, multi-leg
snapshot binding or by-filter progress. Those are later C slices.

## Acceptance criteria
1. An applied write reports exact counts across existing/absent ids, true/false conditions, no-op and real patches, deletes of present/absent ids, and a `delete_by_filter`/`patch_by_filter` over many rows, matching a query of the final state.
2. 100 applied writers across 4 engines on one store, each conditionally claiming one id, produce exactly one `upserted: 1`; the other 99 report `skipped: 1`; the stored row is the winner's.
3. A request whose operations another engine folded, wholly or across two commits, reports the summed exact counts; a fold that lost its CAS reports them too. 0 LIST.
4. A `strong` query issued after an applied response returns the written rows, on another engine too.
5. No sealed row carries `$req`; HEAD's list never exceeds 32; a retired outcome object is buried and reaped by GC, never while listed; a waiter whose window holds a retired or reaped commit reports `outcome_unavailable`.
6. Without `PSTORE_APPLIED_WRITES=1` applied mode is refused before any blob request. `batched`/`durable` cost and response tests pass unchanged.
7. 32 concurrent applied requests on one engine, all flushed before the first gated fold reads its tails, perform at most 2 folds (counted by commit attempts), and a commit that tallied outcomes costs exactly 1 W more than the same commit without. One applied write's W, Rseq depth, Rpar and LIST are recorded (LIST = 0); latencies are provisional.
8. Rejected rows: an applied write whose rows the schema reject pass discards reports them as `rejected`, and the counters sum to the stamped operations.

## Test plan
1. `applied_counts_are_what_the_fold_decided` (server). Mutation: count submitted ids (M13's counters) instead of fold outcomes.
2. `exclusive_conditions_apply_once` (engine; 100 tasks, 4 engines, `MemoryStore`). Mutation: evaluate each condition against the base, not the running state.
3. `a_request_folded_elsewhere_sums_its_outcomes` (interference hook: another engine folds part, then the rest) and `a_cas_loser_reads_the_winners_outcome`. Mutation: answer from the local tally or from one commit only.
4. `strong_reads_see_an_applied_write`. Mutation: respond after the flush, not after coverage.
5. `req_is_never_sealed`, `outcome_list_is_bounded_and_reaped`, `a_retired_outcome_is_unavailable` (32 tallying commits interposed). Mutations: skip the strip in the plain-upsert branch; skip the burial; drop the list from GC's live set; return zeroed counts when absent.
6. `applied_needs_the_rollout_flag`, request counters show 0. Existing cost tests are the control.
7. `concurrent_applied_writes_share_a_fold` and `applied_cost_is_one_outcome_put_per_commit`. Mutations: fold per request; one outcome PUT per request.
8. `rejected_rows_are_counted`. Mutation: tally inside `resolve` only.
Every new test is observed red against the parent before the change.

## RA budget
Intent modes: unchanged. Applied: bundle 1 W + the gated fold (its HEAD, registry, probes,
bundle reads, `prepare`, seal PUTs, 1 CAS) + 1 W outcome per tallying commit + ≤32 Rpar in
one round per fold per engine. 0 LIST. Rseq depth = the fold's + 1; measured, opt-in.

## Risks
Fold latency becomes write latency; a large deferred backlog makes applied writes slow,
not wrong. Stamped upserts add fold bytes (measured, criterion 7). Mixed-version clusters
are unsafe until every node is new: the flag is the operator step, and rollback leaks the
listed outcome objects. HEAD grows by at most 32 short entries and writes every optional
section before it.

## Tasks
- M129.1 Engine: `$req` stamping, tally after set-aside passes, strip in every branch.
- M129.2 Engine: outcome objects, HEAD's bounded list, burial, GC live set, window lookup.
- M129.3 Engine: single-flight fold gate and coverage wait.
- M129.4 Server: `durability: "applied"`, rollout flag, response fields, cost and docs.
