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
  because it never learned the operation id. ⚠️ **Superseded by §9** (M132): an
  idempotency key makes a retried applied write apply at most once, or be refused.
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

## 9. Idempotency keys: at most once, or an explicit refusal (M132)

§7 left a retry after a lost response free to apply twice. This section closes it for
applied writes, within a stated retention, and says exactly what happens outside it.

### 9.1 The problem is retention, not detection

Detecting a retry is easy while the first attempt's outcome is listed in HEAD: the fold
finds the key there. The hard case is a key whose first attempt's commit has **fallen off
the list**: absent from 32 entries, the key looks new, and a retry would apply again. A
key that carries nothing but random bits cannot tell "never applied" from "applied and
forgotten". So a key must carry **when it was issued**, in a unit the fold can compare
against what HEAD still lists.

### 9.2 The key, and HEAD's horizon

A key is `<e0>-<32 hex>`: an epoch and 128 random bits. The server issues one
(`POST /v1/indexes/{index}/idempotency-keys`) with `e0` set to the issuing engine's M129
mark, which is at most the HEAD epoch. A client may also mint its own. **The door refuses a
key whose `e0` is above HEAD**: when `e0` exceeds this engine's mark, it reads HEAD once
and refuses if `e0` is still above it. So every attempt of a key reaches the store after
HEAD reached `e0`, and every commit that folds one has an epoch above `e0`, because HEAD
only moves forward and a commit CASes on a HEAD at least as new. Epoch arithmetic on `e0`
saturates; a bundle can never stop a fold.

HEAD gains one trailing field, **`keys_since`**: every commit with epoch above it that
tallied an applied write is in HEAD's outcome list. Commits maintain it:

- Every commit carries it forward, because commits copy the HEAD they read.
- ⚠️ **Every M132 fold raises it** to the epoch of any entry it drops from the list, and
  decides keys, whatever the engine's level. An engine that dropped an entry without raising
  would leave a retry covered with its winner forgotten (spec review, round 2).
- An engine started for keys (level 3, §9.6) also **initialises** it: any commit it makes,
  fold, gc or compact, on a HEAD without the field sets it to the epoch of the HEAD it read.
  Nothing is known about earlier commits; this one, if it tallies, is listed. That is the
  flag's only effect. A deployment below level 3 never initialises it, so it writes the
  bytes M131 writes.

⚠️ **It must exist before keys are used, not appear with the first key.** If the first
keyed fold set it, it would set it to its own read epoch. That is above the `e0` of every
key issued before any commit happened, so a key would expire on first use at any tenant
where something else committed meanwhile (spec review, round 2). Keys issued on a tenant
before its first level-3 commit can still expire that way, once, at enablement. That costs
a retry with a new key, never a double application.

A HEAD that lost the field, because a pre-M132 binary re-committed it during a rollback,
reads as "nothing known before this HEAD", and the next level-3 commit starts over from
there. So a HEAD re-commit can only expire keys. ⚠️ **A pre-M132 fold, though, applies
every keyed attempt it folds**: it reads the stamp as an opaque request id. The rollback
order is therefore: stop sending keys, let every keyed bundle fold, then downgrade.

The **horizon** of a HEAD is `keys_since`, or its own epoch when the field is absent. A key
is **covered** by a HEAD when `e0 >= horizon`: every commit after `e0` that applied
anything is still listed.

Every attempt of a keyed write keeps its own random operation id, as today, and adds the
key to its stamp. The tally stays per attempt, so two attempts never merge.

### 9.3 The fold rule

Each fold attempt, after reading HEAD and before any pass that applies or sets aside an
operation, decides every key its stamped operations carry. It decides them once for the
whole fold, across indexes, in bundle order (lane, then sequence, then position):

1. **Decided.** The key has a winning attempt, named in an outcome object HEAD lists (as
   the winner, or as the winner a duplicate was refused for) or by an earlier attempt in
   this fold. An operation of the winner proceeds; one attempt can span bundles (§2). Any
   other attempt's operation is discarded unapplied, counted `rejected`, and its attempt is
   recorded as a duplicate of the winner.
2. **Expired.** Otherwise, if the HEAD it read does not cover the key, the operation is
   discarded and its attempt recorded as expired. A commit that fell off the list may have
   applied this key, so nothing of it may apply now.
3. **Winner.** Otherwise this attempt wins the key, and its operations proceed as any
   stamped operation does.

To apply rule 1 the fold reads the listed outcome objects it has not cached, in one
parallel round per attempt, and only when a keyed operation is present. A listed object
that is gone (404) means a newer HEAD dropped it, so this attempt's CAS would fail: the
attempt is retried from a fresh HEAD. Any other read error fails the fold, as a bundle read
error does.

The commit's outcome object records each attempt it decided a key for: the key, and won,
duplicate (with the winner), or expired. A losing CAS recomputes everything against the new
HEAD, as the tally already does (M111), so the decision is made against exactly the HEAD
the commit replaces.

**Why at most once.** Suppose two attempts of one key both applied, at commits `c1 <= c2`.
If `c1 = c2`, rule 1's in-fold check refused the later attempt. Otherwise `c2` read the HEAD
`c1` produced or a later one, call it `H`. If `H` lists any record naming the winner of
the key, rule 1 refused `c2`'s attempt. If not, `c1`'s entry was dropped by some commit up
to `H`, which raised `keys_since` to at least `c1`. Or the field was lost by a rollback, in
which case `H`'s horizon is its own epoch, at least `c1`. Either way the horizon is at
least `c1 > e0`, so rule 2 refused it. Each case contradicts the assumption. Nothing here
uses time, a lock or a lease; the CAS on HEAD is the serialization point it always was.

### 9.4 What a waiter reports

A waiter reads its window `(mark, HEAD]` as today. Its own attempt's records are always
in it, because every commit that folds the attempt follows the mark, unless the window
itself fell off the list, which is today's `outcome_unavailable`.

| Its own records in the window | Response |
|---|---|
| none refused | its own outcome, as today |
| a duplicate of attempt W | W's outcome, summed over every listed commit after `e0`, with `replayed: true`; when HEAD does not cover the key, or an object was reaped, `replayed: true` with `outcome_unavailable` |
| expired, the window intact, and none of its operations applied | refused, `409 idempotency_expired`: nothing applied |
| expired, beside applied operations | refused, `409 idempotency_partial` (§9.5) |
| expired, and the window fell off the list | refused, `409 idempotency_partial`: it may be partly applied, and what fell off is exactly what would say |

⚠️ **A keyed waiter whose window fell off still reads its own records** in the commits
still listed after its mark (round-2 spec review). Its refusing commit is listed unless 32
more tallying commits land before the waiter reads, which would need them inside one
gate-held wait; then it answers `outcome_unavailable`, as for any fallen window (round 3). Today's waiter answers `outcome_unavailable` before looking, which would report
an expired attempt as applied. Without a refusing record, a fallen window still answers
`outcome_unavailable`.

A replayed answer is W's whatever this attempt's body was: a retry must resend the same
write. Comparing bodies would need W's operations stored, which is a per-record cost; the
docs say so instead. A replay reports what W's folds decided **so far**: if W's request
failed between two flushes, or its second bundle is not yet folded, the replay counts only
what was applied.

### 9.5 The one partial case

One attempt can span bundles (§2). If its first bundle wins at `c1`, and its second is
folded only after `c1` and every record naming it have left the list, rule 2 refuses the
second, and the attempt is half applied. The waiter says so (`idempotency_partial`) rather
than reporting success. It needs the two flushes of one request to be 32 tallying commits
apart, and M132 tests the response rather than the odds.

### 9.6 Cost and limits

- No new object: the decisions ride in the outcome object a commit with applied writes
  already writes. HEAD gains one `u64`, written only at level 3, plus the empty optional
  sections before it on a HEAD that had none.
- A fold attempt with keyed operations reads every listed outcome object it has not
  cached: at most 32 parallel GETs in one round per attempt, then about one per commit.
  That is on the fold, so on applied mode's latency, which is already outside D-34.
- **Retention is 32 tallying commits of the tenant, not a duration.** A key issued at a busy
  tenant can expire in seconds; the client then gets a new key and retries. That is a
  refusal, never a double application.
- Keys are per tenant, not per index: a key reused on another index is a duplicate.
- Mixed versions: an M131 fold would ignore keys and apply every attempt, and an M130
  decoder refuses an object with a keys section. Keys are therefore refused unless every
  node is started with `PSTORE_APPLIED_WRITES=3`.
