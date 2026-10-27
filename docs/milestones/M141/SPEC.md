# M141: a tenant's buffered rows are bounded

Spec review round 1 failed with two majors (how *pending* is kept, and check-then-charge
races); round 2 passed with minors. Two minors stay open for implementation: the
reservation is trued up to the stored rows' footprint (stamps and condition marks
included) when it converts, and the concurrency test forces the interleaving with a
barrier, not by relying on the held PUT alone.

## Serves
D-56 (per-tenant memory quotas) in [memory management](../../research/09-rust-stack/memory-management.md).
Workstream A ("per-tenant buffered bytes") in [the delivery plan](../../comparisons/turbopuffer-next-plan.md).

## Delta
Today a `batched` write stays in the process until some `durable` write flushes it
(M9i: the scheduled fold never flushes), and unfolded durable batches stay until a fold.
Nothing bounds either, so one tenant's batched writes grow without limit.

Add `Document::footprint()` to `pstore-format`: a deterministic estimate in bytes.
It counts the id's bytes, 4 per dense component, 8 per sparse pair, and per attribute its
key's bytes plus 8 per number, bool or datetime and the string's bytes per string, summed
over an array's elements. The engine keeps two counts per tenant, from memory only:
- *pending*: the summed footprint of every `Document` in `Memtable::pending`, which also
  holds tombstones, patches, by-filter operations and their encoded conditions. It is
  kept inside `Memtable`, under its lock: added in `buffer`, subtracted for exactly the
  rows `landed` drains (a flush snapshots pending, and rows that arrive during its PUT stay
  charged), and for exactly the rows dropping an index or a fold's drop path removes.
- *unfolded*: `Engine::unfolded()`, the summed encoded size of flushed bundles not yet
  known folded (the M9i.1 stamps from `pruned` on).
A row's charge changes unit when it is flushed: a footprint before, its bundle's encoded
share after. So the sum can pass a bound after admission by the difference, which the
measurement records. Neither count is RSS.

**Reservations, so check and charge are one step.** Before the first buffer call of a
request, the server asks the engine for a reservation of the request's total footprint.
Under the memtable lock the engine checks the bounds below and adds the reservation to
*pending*. The request's buffer calls then convert it into rows, and the reservation is
released on refusal, failure or cancellation (drop guard). Two concurrent writers cannot
both pass a check that only one of them fits.

Two versioned limits per `Api`, set once at construction, with defaults and configuration
as M128's (they lower and never raise, and are validated before store/cache/bind).
They are added as version-1 fields of `PSTORE_REQUEST_LIMITS_CONFIG`:
- `tenant_flush_bytes`, default 8 MiB: [write-path-and-wal.md](../../research/05-storage-engine/write-path-and-wal.md) §4's `max_bytes` batch
  bound. When a `batched` write's reservation would take *pending* above it, the tenant's
  pending rows are first flushed: one bundle PUT, the durable path unchanged. Then the
  reservation is retried once. A failed flush refuses the write as a durable flush
  failure refuses today, and nothing of it is buffered.
- `tenant_buffered_bytes`, default 64 MiB and at least `tenant_flush_bytes`: half of
  §6's 128 MiB unindexed cap per shard, because a tenant has no shard of its own here and
  several share a process. A reservation that would take *pending + unfolded* above it is
  refused with `429 overloaded` and `Retry-After: 1`, whatever the durability, before
  anything is buffered. The tenant is added to the fold requests, as M9i.2's
  `not_folded` refusal does, so a retry can succeed once a fold commits. With scheduled
  folds off (`PSTORE_FOLD=off`) nothing takes the request, and the refusal says the
  operator must fold (`POST /v1/admin/fold`). A request whose own footprint is above the
  bound can never fit: it is refused with `413 request_too_large`, not 429.

Unchanged: the refusal tables for every other error, the 429 of M128, query results,
stored formats, write ordering and visibility, and the outcomes of `durable` and `applied`
writes. No process-wide buffered total in this slice: with `PSTORE_ENGINES` set it is
bounded by engines × `tenant_buffered_bytes`, and an unbounded registry stays unbounded.
Query byte reservations (D-55) and decoded-request structure limits remain later A work.

## Acceptance criteria
1. On a tenant whose lane is already registered (one earlier durable write), a batched
   write that brings *pending* exactly to `tenant_flush_bytes` costs zero blob requests.
   One more row first flushes the earlier rows in exactly one PUT, and every row of both
   writes is then visible to queries.
2. A write that brings *pending + unfolded* exactly to `tenant_buffered_bytes` succeeds.
   One byte more gets 429 `overloaded` with `Retry-After: 1`, buffers nothing, and puts
   the tenant in the fold requests. Once a fold commits, the same write succeeds.
3. A second tenant's writes are unaffected while the first is at its limit. Of two
   concurrent writes that each fit alone but not together, exactly one is admitted.
   Rows buffered during a flush's PUT stay charged after it, and dropping an index
   returns its rows' charge. A request larger than the bound is a 413.
4. A failed pre-flush leaves the rows already pending, the counts and the write's own
   rows exactly as they were before the request.
5. The new configuration fields follow M128's refusals: zero, above the default,
   `tenant_buffered_bytes` below `tenant_flush_bytes`, and unknown fields.
6. The existing server and engine suites, cost/depth gates and coverage floors pass.

## Test plan
1. `batched_rows_flush_at_the_tenant_flush_bound`, with the boundary computed by
   `footprint()` and asserted on request counters. Mutations caught: a `>=` comparison,
   never flushing, flushing after buffering.
2. `buffered_rows_refuse_at_the_tenant_bound`, with a fold disabled and then run.
   Mutations caught: not counting *unfolded*, a missing fold request, buffering before the
   check.
3. `one_tenant_at_its_bound_leaves_another_alone`, `concurrent_reservations_admit_one`
   (the fault store holds the first request's flush PUT), and
   `rows_buffered_during_a_flush_stay_charged`. Mutations caught: one count shared across
   engines, checking outside the lock, resetting *pending* on flush, not subtracting on
   an index drop.
4. `a_failed_preflush_changes_nothing`, using the fault-injecting store. Mutations caught:
   resetting *pending* before the PUT succeeds, buffering the new rows anyway.
5. Unit tests of the configuration reader and of `footprint()`, one per value kind.
   Mutation caught: a value kind not counted.
6. `scripts/gates.sh`, the recall, NDCG and depth scripts, and `scripts/coverage.sh`.

## RA budget
A batched write at its flush bound pays 1 W, the same bundle PUT a durable write already
pays. That cost scales with bytes, not with records or time. Every other write pays
0 W / 0 Rseq / 0 Rpar / 0 LIST added. A refusal costs no request. No timer, and no
state per idle tenant beyond the engine the registry already holds.

## Risks
`footprint()` is an estimate, not RSS: it under-counts allocator and map overhead. Record
measured RSS against it in VERIFIED.md, and do not call it a memory bound. A batched
client that writes faster than folds commit will see 429s where it saw unbounded growth;
that needs a migration entry. A pre-flush turns some batched writes into a PUT's latency.

## Tasks
- M141.1 Implement footprint accounting, the pre-flush and the refusal, with
  configuration, tests, a migration entry and evidence.
