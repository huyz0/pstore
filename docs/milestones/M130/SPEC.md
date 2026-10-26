# M130: affected ids, and one pinned order inside a mixed write

## Serves
D-34's explicit API contract. Second slice of workstream C in
[the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("actual affected
counts/IDs, mixed-operation ordering"), on [M129](../M129/SPEC.md)'s applied completion.

## Delta
An applied write may add `"affected_ids": true`. Its `applied` object then carries
`ids: {upserted, deleted, patched}`: for each kind, the **distinct** ids an operation of
that kind changed, sorted, unioned over every commit that folded the request. Counts stay
per operation, so a list can be shorter than its count (`x` upserted twice is
`upserted: 2`, `[x]`), and an id changed by two kinds is in both lists (upserted then
deleted is in `upserted` and `deleted`). An id the fold set aside is in no list.
Refused with 400 `bad_request` beside any durability but `applied`, beside a by-filter
operation (its ids are unbounded until a later slice bounds them), or when the request
names more than 10,000 distinct ids.

Engine: such a request is stamped `$req = "<hex>:ids"`, so an M129 fold strips it like
any `$req` and never seals it. The fold collects ids only for stamps ending `:ids`; the
outcome object gains a trailing ids section. ⚠️ An M129 waiter refuses trailing bytes,
so ids need every node at M130: `PSTORE_APPLIED_WRITES=2` enables them, `1` keeps M129's
behaviour and refuses `affected_ids` with `unsupported_durability`.

Order, documented and tested, not changed: inside one request, upserts, then patches,
then deletes, then `patch_by_filter`, then `delete_by_filter`, even when a concurrent
flush splits the request across two bundles (one lane, ascending sequences). Between
concurrent requests the order is **unspecified** (fold order depends on when flushes land
and on lane ids, and requests on one engine interleave in one bundle); the docs say so.

Unchanged: counts, costs and refusals of every request that does not ask for ids; no
idempotency key, no by-filter progress, no conditions over new values.

## Acceptance criteria
1. With `affected_ids`, each list is exactly the distinct ids that kind changed, across present/absent ids, true/false conditions, no-op patches, a repeated id, an id upserted then deleted, and a set-aside row.
2. A request folded by another engine, in two commits, returns the union of both commits' ids.
3. Without `affected_ids` the response has no `ids`, and the outcome object has no ids section; an M129 object still decodes.
4. `affected_ids` beside `batched`/`durable`, beside a by-filter operation, over 10,000 distinct ids, or with `PSTORE_APPLIED_WRITES=1` is refused before any blob request.
5. One request carrying an upsert, a patch and a delete of one id ends in the documented state, including when a flush between its buffer calls splits it into two bundles.
6. No sealed row carries the stamp.

## Test plan
1. `affected_ids_are_what_the_fold_decided` (engine and server). Mutation: report the submitted ids.
2. `ids_union_across_commits_and_engines`. Mutations: ids from the local tally only; from one commit only.
3. `ids_are_opt_in` and `an_m129_outcome_object_decodes`. Mutation: always write ids.
4. `affected_ids_refusals_cost_nothing` (server, request counters). Mutations: drop the by-filter refusal; accept at level 1.
5. `one_request_applies_in_the_documented_order` (with `flush_with_interference_for_test` splitting it). Mutation: buffer deletes before patches.
6. `ids_are_never_sealed`. Mutation: strip only an exact `$req` match, not its value.
Each observed red against the parent (or a stub) before the change.

## RA budget
Requests: unchanged against M129 for every request. Bytes: an outcome object grows by the
ids of the requests in its commit that asked (≤10,000 per request, ×3 kinds), and every
waiter in its window reads them, including waiters that did not ask.

## Risks
A commit holding many id-asking requests makes a large outcome object every loser reads;
if that matters, a later slice moves ids to their own object. Clients may expect arrival
order between their own concurrent writes; the docs now say it is not given.

## Tasks
- M130.1 Engine: `:ids` stamp, per-request id sets, ids section, union in `await_applied`.
- M130.2 Server: `affected_ids`, level-2 flag, refusals, response, order docs and tests.
