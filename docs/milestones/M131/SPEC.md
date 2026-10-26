# M131: one snapshot for every leg of a request

## Serves
The C/F row of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("a multi/facet
execution can bind every leg to the same tenant snapshot and durable-tail boundary"), on
[M9g](../M9g/SPEC.md)'s multi-query and [M65](../M65/SPEC.md)'s facets; D-34's read budget.

## Delta
Today every sub-query of a multi-query, and the facet leg beside a ranked query, reads HEAD
and this engine's unfolded rows on its own. A fold that commits while the legs start makes
them answer from different epochs, so a count and the hits beside it can disagree.

After: the legs of one request share one snapshot, taken once before any leg runs: one HEAD
(with the lane registry for `strong`, or the cached HEAD for `bounded`) and one view of the
index's unfolded rows. Every leg's segment reads and unfolded rows come from it, so every
epoch in the response is the same. Lane tails are still probed per leg (`settled`); a leg
that finds an unfolded write refuses the request, as today, rather than answering apart.

- Engine: `Engine::pinned(index, consistency, f)` takes the snapshot and runs `f` under it,
  through a task-local as M129's `applying` is (the legs run in one task; a leg that spawned
  would lose the pin, which criterion 3's HEAD count would show). Every live read of `index`
  at exactly that `Consistency` inside `f` uses it. A read at another consistency, of another
  index, or `as_of` an epoch is unaffected. `FreshView` becomes `Clone` (its fields are `Arc`
  or `Copy`).
- **Retries.** Inside a pin, a read never retries on its own (today a `bounded` cache hit
  that fails retries that leg fresh). `pinned` owns the one retry: when its snapshot was a
  cache hit and `f` returns any error, it takes a fresh snapshot (a HEAD read) and runs `f`
  once more. Any error counts, including a server refusal such as `index_not_found` from a
  stale cached HEAD, which a leg today does not retry; that is the one behaviour change.
- Server: legs share a level when their engine `Consistency` (after `session` is mapped,
  M11.1) is equal, `bounded` tolerances included: `eventual` beside `session` that maps to
  eventual is one level; `bounded` 100 ms beside 5,000 ms is two. A multi-query whose legs
  share a level and none has `as_of`, and a query with `facets` and no `as_of`, run pinned.
  Anything else runs as before; the docs say so.

Unchanged: single queries without facets, writes, the response shape (`epochs`,
`facets_epoch`), what each level promises, peers' shares (they read no HEAD), and depth (the
snapshot is the round each leg already spent on HEAD). Not done: one `settled` per request,
and one shared `Head` clone for strong ordered/aggregate legs.

## Acceptance criteria
1. A 3-leg multi-query during which another engine folds a new row (after the snapshot, before
   any later HEAD read) answers all 3 legs from one epoch, and the row is in no leg.
2. A ranked query with `facets` under the same interference has `facets_epoch == epoch`, and
   the interfering row, which matches the filter and would rank first, is neither a hit nor
   counted in the facets.
3. An `eventual` multi-query of N legs reads HEAD once, not N; a `strong` one reads HEAD and
   the lane registry once each.
4. A multi-query of an `eventual` leg then a `strong` leg, with another engine's unfolded
   write, is refused `not_folded` as before: the strong leg is not served at eventual.
5. A `bounded` pinned multi-query whose cached HEAD names a reaped segment answers, every leg
   at one fresh epoch, with exactly one HEAD read after the cached one.

## Test plan
1. `legs_share_one_snapshot` (engine, through `pinned`; server, through `queries`). The store
   reads the first HEAD's bytes, lets a rival (on the unwrapped store) fold a new row and
   commit, then returns the pre-fold bytes; every later HEAD read waits for that commit, so
   the parent's legs 2 and 3 see the new epoch.
   Mutation: ignore the pin and read HEAD per leg.
2. `facets_share_the_ranked_snapshot` (server, same store). Mutation: run the facet leg
   unpinned.
3. `a_pinned_request_reads_head_once` (engine and server, read counters), its legs a ranked,
   an ordered and an aggregate query. Mutation: pin only the first leg.
4. `mixed_levels_are_unpinned` (server). Mutation: run every leg at the first leg's level. A
   regression guard: the parent already refuses, so it is seen red only against its mutation.
5. `a_reaped_bounded_snapshot_retries_fresh` (engine: a cached HEAD, then `gc` reaps a segment
   it names). Mutations: no retry in `pinned`; a per-leg retry inside the pin that reads HEAD
   afresh (two epochs, or more than one HEAD read).
Each test but 4 observed red against the parent (or a stub `pinned` that just runs `f`) first.

## RA budget
An N-leg request: HEAD reads N → 1 (`eventual`, `strong`, and `bounded` on a cache miss),
lane-registry reads N → 1 (`strong`); segment, block and lane-tail reads unchanged; a
`bounded` failure retries the whole request (its legs twice) on that rare path. W 0, LIST 0,
depth unchanged.

## Risks
The pin is taken before any leg runs, so a leg sees HEAD as of the request's start rather than
as of its own start: never older than its level allows, because the snapshot is taken at that
level. A future `tokio::spawn` inside a leg would drop the task-local silently; criterion 3's
HEAD count is the guard.

## Tasks
- M131.1 Engine: `pinned`, the read paths taking it, the retry.
- M131.2 Server: multi-query and facets pinned, docs, tests.
