# Mutation Completion: Intent, Applied, and What a Write Response May Claim

**Status: architecture proof for workstream C of the
[delivery plan](../../comparisons/turbopuffer-next-plan.md), 2026-10-26.** It decides where
a write *completes*, what a response may then report, and what that costs. It changes no
existing guarantee. The first implementation slice is
[M129](../../milestones/M129/SPEC.md).

## 1. The problem, in one paragraph

A durable write today is acknowledged when its bundle is in the blob store (t2 of
[consistency-model.md §4](../03-metadata-consistency/consistency-model.md)). Patches,
conditions and by-filter operations are *deferred*: they are decided at a fold, against the
version current then (M13). So `documents_deleted` and `documents_patched` count ids a
client **asked** about, not rows that changed, and a conditional write's response cannot say
whether its condition held. That is honest for an intent and wrong for anything that calls
itself an affected count. A vendor's synchronous write reports what happened; ours cannot
until the fold that decides it has committed.

## 2. Two completion points

| Mode | Completes when | Linearization point | May report |
|---|---|---|---|
| **intent** (`durable`, unchanged) | the bundle PUT returns | create-if-absent of `(lane, seq)` | what was submitted, labelled submitted |
| **applied** (new) | HEAD's watermark for the lane reaches the engine's next sequence after the request's flush | the HEAD CAS of the last commit that folded any of its operations | exact outcomes, summed over those commits |

⚠️ **The fold is already a serializer.** `resolve` applies an index's deferred operations in
fold order against a running state, so two mutually exclusive conditions over one id cannot
both apply in one fold, and a fold that loses its CAS rebases and recomputes (M111). Fold
order is lane, then sequence: not arrival order, and nothing promises arrival order.
Applied completion adds no new ordering mechanism; it **reports** the one that exists.

⚠️ **One request can span commits** (spec review of M129). The server buffers a request's
upserts, patches, deletes and by-filter operations in separate calls, and a concurrent flush
can take `pending` between them, so one request can land in two bundles, folded by two
commits, possibly by two nodes. Completion therefore waits for the engine's whole next
sequence, and outcomes are **summed** over every commit in the window, never taken from one.

⚠️ **Applied is not a forced fold per request, and today's `fold()` would be one.** Every
`fold()` call runs its own HEAD read, lane registry read and tail probes, with nothing
coalescing concurrent calls; and joining a fold already in flight is wrong, because its
tails were read before the request's bundle existed. So applied mode adds a **single-flight
gate**: a waiter records its target (lane, next sequence) after its flush, then takes the
engine's fold gate; holding it, it returns at once if the last HEAD this engine read
already covers the target, and otherwise folds. Waiters queued behind one fold are covered
by it, because it started after their bundles landed. A fold folds every lane, so a node
that loses the CAS usually finds its target folded by the winner and stops (M111). Folds,
and therefore CAS attempts, scale with concurrent bursts per node, not with requests; what
applied mode costs is **latency**.

## 3. Attributing outcomes to a request

A request's operations share a buffer with other requests, and a concurrent request's
flush may carry them (M11.1). So a request has no bundle of its own, and `(lane, seq)` cannot
name it. Instead:

1. An applied request draws a random 128-bit **operation id** and stamps it on each of its
   operations as the reserved attribute `$req`, as `$op` already marks a deferred kind.
2. The fold treats a stamped operation as deferred, so it is resolved against the current
   version, which is what makes "deleted" mean "a version existed and was removed".
3. The fold tallies each stamped operation under its operation id, in **rows**: rows
   upserted, rows deleted (a version existed), rows patched (a row changed), and
   operations skipped (condition refused, nothing to patch, nothing to delete). A
   by-filter operation counts every row it changed, so a `delete_by_filter` over 50 rows
   reports `deleted: 50`.
4. ⚠️ **The tally is taken after every pass that can set a row aside**, not in `resolve`
   alone: the schema reject pass, the width pass after `resolve`, a replica's refusal and
   an index dropped mid-fold all discard operations. Those count as `rejected`, so the
   counters always sum to what was stamped.
5. The fold strips `$req` before sealing, in every branch of `resolve`: the plain-upsert
   branch does not pass through `cleaned`. A sealed row carrying `$req` is a bug, and M129
   tests that none does.

## 4. Where outcomes live, and how a waiter finds them

The node that commits knows its fold's outcomes in memory, so its own waiters cost nothing
to answer. A waiter whose bundle another node folded (the CAS loser) needs them durably.

- A commit that tallied any outcome writes **one outcome object** before its CAS,
  create-if-absent like a segment, keyed by its epoch and the writer's lane and attempt,
  under an `/out/` path that never ends in `.seg`, so `as_of` cannot read it as a segment.
  A losing attempt's object is buried as its other objects are (M23).
  `RA = +1 W per such commit`, never per request or document.
- HEAD gains a trailing, bounded list: the last **K = 32** `(epoch, outcome key)` pairs.
  Trailing is HEAD's compatibility mechanism (`head.rs` `encode`). Entries that fall off
  are buried in the graveyard, so GC reaps the object as it reaps a segment.
- The window's lower bound is the **highest epoch of any HEAD this engine read before the
  flush**, the same record the gate keeps, updated on every HEAD read and not only on this
  engine's own commits (round-2 review: a CAS loser never commits, so its own last commit
  falls arbitrarily far behind). Any commit that folds the request's bundles happens after
  their PUT, so its epoch exceeds every HEAD epoch observed before the flush.
- A waiter reads every listed outcome object with an epoch in `(lower bound, HEAD it
  read]`, **in one parallel round**, through an engine-wide cache so that waiters behind
  one fold share the GETs and the committing engine never re-reads its own. It sums its
  id's entries. Worst case: 32 parallel GETs, once per fold per engine, not per request.
- The list names only commits that tallied something, so a commit has fallen off exactly
  when the list is full and its oldest epoch is above the lower bound.
- If a commit in the window has fallen off the list, or its object was reaped (a 404),
  the response says `outcome_unavailable` and **never** invents counts.
- GC's live set includes HEAD's outcome list, so a listed object is never reaped.

No LIST, no lock, no per-record object, and no per-index timer. Invariant I1 holds: every
new object is created once, and the only mutable object touched is HEAD, by CAS.

## 5. Depth

An applied response waits on the whole fold: HEAD, lane registry, tail probes, bundle GETs,
the `prepare` pass, seal PUTs, the outcome PUT and the CAS, plus one parallel outcome round
for a waiter that did not commit. That is a data-dependent chain on a write's response, so
applied mode is an **opt-in outside D-34's budget**, exactly as `rerank: exact` is for
queries. `batched` and `durable` keep their depth. M129 records the measured depth.

## 6. Reads

- `strong` after an applied write sees it: the commit is in HEAD before the response, and
  `strong` reads HEAD afresh. No new read path is needed.
- A multi-leg request bound to one snapshot is a separate slice (C's snapshot criterion).
  It binds every leg to one HEAD read, `as_of` a single epoch; it is not in M129.

## 7. What this does not provide, and must not be claimed

- **Exactly-once.** A client that lost an applied response cannot learn its outcome,
  because it never learned the operation id. A client-supplied id, deduplicated against
  the retained outcome objects (bounded by K), is the follow-up.
- **Rollback safety.** A HEAD with the outcome list must write every optional section
  before it, and an old binary that re-commits drops the list, leaking the objects it named
  until a sweep exists. Rollback after enabling applied mode leaks bounded garbage. Until then a retried
  conditional write may apply twice, and the docs say so.
- **Atomic multi-index batches.** A request writes one index. Unchanged.
- **Partial filter-mutation progress.** A by-filter operation resolves inside one fold, as
  today. Progress and resume belong to a later slice, with its own cost budget.
- **Mixed-version rollout safety for free.** An old binary's fold would seal `$req` as an
  attribute and drop HEAD's outcome list on re-commit. Applied mode must therefore be
  refused unless the deployment declares every node new enough (`PSTORE_APPLIED_WRITES=1`),
  and the rollout order is: upgrade every node, then enable.

## 8. Costs

| Path | Intent (`durable`) | Applied |
|---|---|---|
| Bundle | 1 W per flush | same |
| Fold | amortized, scheduled | the fold the request joins: its reads, segment PUTs, 1 CAS |
| Outcomes | none | +1 W per commit that tallied one; ≤32 parallel R per fold per engine, in one round |
| Exact deletes | none | the fold reads the current version of each stamped id (segment reads it already does for any deferred operation) |

The extra reads for stamped unconditional deletes and upserts are bytes, not a new chain:
the fold already reads every segment of an index with a deferred operation in one pass
(`prepare`). M129 measures it rather than asserting it.
