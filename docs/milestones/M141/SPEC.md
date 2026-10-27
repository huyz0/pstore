# M141: a tenant's buffered rows are bounded

Spec review pending.

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
- *pending*: the summed footprint of rows buffered and not yet in a bundle. Reset by a
  successful flush, and unchanged by a failed one.
- *unfolded*: the summed encoded size of flushed bundles not yet known folded. These are
  the existing M9i.1 stamps, and the count drops as batches are pruned.

Two versioned limits per `Api`, set once at construction, with defaults and configuration
as M128's (they lower and never raise, and are validated before store/cache/bind).
They are added as version-1 fields of `PSTORE_REQUEST_LIMITS_CONFIG`:
- `tenant_flush_bytes`, default 8 MiB. A `batched` write whose rows would take *pending*
  above it first flushes the tenant's pending rows: one bundle PUT, the durable path
  unchanged. Its own rows are then buffered and stay `batched`. A failed flush refuses the
  write as a durable flush failure refuses today, and the write's rows are not buffered.
- `tenant_buffered_bytes`, default 64 MiB, at least `tenant_flush_bytes`. A write of any
  durability whose rows would take *pending + unfolded* above it is refused with
  `429 overloaded` and `Retry-After: 1` before anything is buffered. The tenant is added
  to the fold requests, as M9i.2's `not_folded` refusal does, so retrying can succeed.

The rows charged are the upserted documents, together with each patch's attributes and
each delete's id. Unchanged: the refusal tables for every other error, the 429 of M128,
query results, stored formats, write ordering and visibility, and the outcomes of
`durable` and `applied` writes. No process-wide buffered total in this slice: with
`PSTORE_ENGINES` set it is bounded by engines × `tenant_buffered_bytes`, and an unbounded
registry stays unbounded. Query byte reservations (D-55) and decoded-request structure
limits remain later A work.

## Acceptance criteria
1. A batched write that brings *pending* exactly to `tenant_flush_bytes` costs zero blob
   requests. One more row first flushes the earlier rows in exactly one PUT, and every row
   of both writes is then visible to queries.
2. A write that brings *pending + unfolded* exactly to `tenant_buffered_bytes` succeeds.
   One byte more gets 429 `overloaded` with `Retry-After: 1`, buffers nothing, and puts
   the tenant in the fold requests. Once a fold commits, the same write succeeds.
3. A second tenant's writes are unaffected while the first is at its limit.
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
3. `one_tenant_at_its_bound_leaves_another_alone`. Mutation caught: one count shared
   across engines.
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
