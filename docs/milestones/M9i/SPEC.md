# M9i — Visibility: a scheduled fold, and `consistency`

**Serves:** the `consistency` row of
[`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md) ("other
processes see a write only after an operator `fold`"), and D-39: the fold rate is
**adaptive and size-driven, never a fixed timer**, and idle indexes cost zero. Ninth and
last of M9.

⚠️ **Split before starting**, as M9h was:
- **M9i.1** — a fold scheduled inside the server, on D-39's size-or-age triggers. It bounds
  how long another process waits to see a durable write, and adds no request for a tenant
  with nothing to fold.
- **M9i.2** — the `consistency` request field. Specified when M9i.1 lands, because what
  `eventual` promises is exactly the bound M9i.1 sets.

## M9i.1 — a scheduled fold

### What is true today, and stated wrongly

- A query reads HEAD, the segments HEAD names, and **this process's** memtable. No read path
  reads a lane bundle.
- So a durable write from process A is **invisible** to process B until some fold commits
  it. It is not read slowly in the meantime.
- `UNSCHEDULED`'s `fold` duty and `docs/deploy.md` say "unfolded bundles are read on every
  query". That is false, and M9i.1 corrects both.

### Delta

**The trigger — per tenant, from memory, zero requests to decide.**
- The engine keeps, beside each durable (flushed, unfolded) batch, the instant it became
  durable (`tokio::time::Instant`, so a paused test clock governs it) and the encoded size of
  its bundle.
- `Engine::unfolded()` returns the oldest instant and the byte sum over those batches **not
  already known folded**, i.e. with a sequence at or above `pruned`. It reads nothing.
- (Spec review, B1.) Two ways a folded batch could otherwise stay in `durable` and keep a
  tenant due forever, each costing a fold's requests per tick, are closed:
  - A fold that finds **nothing** to fold now prunes to the HEAD it read, as a committing
    fold does. Nothing is left because another process folded this lane.
  - A flush whose batch a concurrent fold already folded pushes it with a sequence below
    `pruned`. `unfolded()` ignores it, and the next `prune` removes it and bumps the
    generation, as today. (Spec review, round 2: dropping it at flush instead would leave a
    cached fresh view serving its rows, because only `prune` bumps the generation. The drop
    is removed rather than patched.)
- A tenant is **due** when `unfolded()` is non-empty and either:
  - the oldest is at least `age` old, or
  - the bytes are at least `bytes`.

**The policy and the entry points** (spec review, M3):
- `FoldPolicy { period, age, bytes }`. Its defaults are `period` 1 s, `age` **1 hour** and
  `bytes` 1 MiB: D-39's own triggers (spec review, M2). An operator wanting faster
  cross-process visibility lowers `age` and pays for it; see Risks.
- `Api::fold_due(&self, policy) -> FoldTick { folded, nothing, failed, deferred }` folds
  every due tenant this process holds an engine for, at most 4 at once.
  - It snapshots the engines under the map's lock and releases the lock **before** folding,
    so a request that needs `api.engine()` never waits behind a fold (M4).
- `run_folds(api, policy, stop)` loops: `fold_due`, then sleep `period`. Ticks never
  overlap. When `stop` resolves, it starts no further tenant fold, awaits only the folds
  already in flight, and returns. A cut-off tick is safe: a fold commits by CAS or not at all.
- `serve_folding(api, listener, shutdown, Option<FoldPolicy>)` serves and runs `run_folds`.
  It stops both on the one signal, and awaits the fold loop before returning (M4). `None`
  runs no loop.
- `serve` keeps its signature and runs no loop, so existing callers and tests are
  unchanged. `main.rs` calls `serve_folding` with the policy `Config` parsed.

**Failure backs off** (spec review, M1):
- A tenant whose fold fails is not retried on the next tick. Its delay starts at `period`
  and doubles per consecutive failure, capped at `age`, with the next attempt at failure
  time plus the delay.
- It is held in memory per tenant, reset by a success, and counted as `deferred` while it
  waits.
- So a permanently failing tenant costs O(log) attempts over the cap's span, then one per
  `age`. That is not one per tick.

**Metrics** (spec review, M5). `/metrics` gains `pstore_fold_total{outcome="folded|nothing|
failed"}`, with no tenant dimension.

**Cost.** Deciding costs no request.
- A fold happens only for a tenant holding a batch not known folded. So a tenant that is
  idle, batched-only, or already folded elsewhere costs **zero** after at most one `nothing`
  fold, which prunes it.
- The rate bound: per writing tenant per writing process, at most one fold per `age`, plus
  one per `bytes` written, plus the failure backoff. Requests scale with writes and bytes,
  never with time alone (AGENTS.md; D-39).

**The visibility bound** (spec review, m3). While the writer lives, a durable write is
visible to other processes within about `age + period` plus one fold's latency. Less if its
bytes trip the threshold, more if due tenants outrun the 4-way concurrency.

**Config.**
- Environment variables:
  - `PSTORE_FOLD_PERIOD_MS`, `PSTORE_FOLD_AGE_S` and `PSTORE_FOLD_BYTES`: positive integers;
  - `PSTORE_FOLD`: unset, or exactly `off` to disable the loop, in which case the operator
    folds, as today.
- Anything else is refused at startup, naming the variable: `0`, negative, non-numeric, or
  a `PSTORE_FOLD` value other than `off`.
- `docs/deploy.md`'s environment table gains the four rows.
- An `age` of 0 is reachable only through `FoldPolicy` in tests.

**What it does not do.**
- It never **flushes**. A `batched` write stays in memory, as M3 says.
- It folds only tenants this process holds, but a fold is tenant-wide: it folds **every**
  live lane of the tenant, a dead writer's included (spec review, m1). What stays unfolded
  is a tenant whose only writer died, or restarted, and that no live process writes again.
  The `fold` duty is rewritten to say exactly that, restarts included, and to drop the false
  claim that bundles are read on every query.
- No lock, lease or leader. Processes folding one tenant race on HEAD's CAS, as operator
  folds always could.

### Acceptance criteria

1. **Another process sees the write.** Two `Api`s share one store, with policy `age` 0. A
   durable write through A:
   - is not found through B;
   - is found through B after one `A.fold_due`, with no admin call;
   - and a second `fold_due` reports nothing folded.
2. **Idle costs nothing** (B1). A `fold_due` issues **zero** blob requests, counted by the
   accounted store, in each of these states:
   - (a) every tenant folded by this process;
   - (b) only batched rows;
   - (c) this process's durable batch was folded by **another** `Api`. After one `nothing`
     fold, a further `fold_due` issues zero requests;
   - (d) a flush that lost to a concurrent fold leaves `unfolded()` empty. It is built with
     `pstore-testkit`'s gated store, holding the flush's PUT response until a fold has
     committed.
3. **Size triggers.** With `age` at 1 hour and `bytes` between two tenants' unfolded sizes,
   only the larger tenant is folded.
4. **Age triggers.** Under tokio's paused clock, with `age` 60 s: a durable write is not
   folded at 59 s, and is folded at 60 s.
5. **The loop runs, and stops.** Under the paused clock, `run_folds` with `period` 1 s and
   `age` 2 s folds a durable write within 3 s with no one calling `fold_due`, and returns
   once `stop` resolves. Separately, a real-clock `serve_folding` with small values:
   - makes a durable write visible to a second `Api`;
   - returns after its shutdown signal;
   - and no fold request is counted after it returns.
6. **Failure backs off.** A tenant whose fold fails (an injected fault) is `failed` once, then
   `deferred`. Over 64 ticks of `period` with the fault persisting, and `age` 64 periods, it
   is attempted at most 8 times. Once the fault clears and its delay expires, it is folded.
7. **A fold does not block requests** (M4). With one tenant's fold held on a gated store, a
   write to another tenant through the same `Api` completes.
8. **Two folders, one tenant** (m2). Two `Api`s write one tenant and call `fold_due`
   concurrently. Both writes are visible through each, and neither reports `failed`.
9. **Metrics.** After criterion 1, `/metrics` shows `pstore_fold_total{outcome="folded"}` at
   1. After criterion 6's first tick, `failed` is at 1.
10. **Config.** The defaults apply when the variables are unset. `off` disables the loop.
    `0`, `-1`, `x`, and `PSTORE_FOLD=on` are each refused, naming the variable.
11. **The duty tells the truth.** `/v1/admin/duties` and `docs/deploy.md`:
    - say a scheduled fold runs;
    - say a tenant whose only writer died or restarted still needs an operator fold;
    - never claim bundles are read on every query.
    `the_duties_endpoint_reports_every_unscheduled_duty` still holds.
12. `./scripts/gates.sh` passes; `./scripts/mutants.sh` over the diff misses 0.

### Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `fold_due` does not exist | a due tenant skipped |
| 2 | (c) and (d) fold forever today | an idle tenant folded or probed; `nothing` not pruning; the straggler counted as unfolded |
| 3 | (after 1) | the byte threshold ignored or inverted |
| 4 | (after 1) | age compared the wrong way; age from the newest batch |
| 5 | `run_folds` does not exist | the loop not run; the period ignored; `stop` ignored |
| 6 | (after 1) | no backoff; backoff never reset; one failure ending the loop |
| 7 | (after 1) | the engines lock held across a fold |
| 8 | (after 1) | a lost CAS counted as failure |
| 9 | the series does not exist | an outcome miscounted |
| 10 | the variables are unknown | a bad value accepted |
| 11 | the duty text is unchanged | — |

### RA budget

- User paths: unchanged.
- Background: zero requests per tick while nothing is due; one fold per due tenant, at the
  operator fold's cost; the rate bound above.

### Risks

- **Pricing a shorter `age`.** D-39 prices a 1-hour age trigger at about $3.6k a month
  fleet-wide. A 60 s age is 60 times the folds for every continuously written tenant.
  That is the operator's trade, and the default does not make it.
- **Contention.** Several processes writing one tenant fold it independently, and a loser's
  sealed segments are orphaned until GC reaps them. Each lost CAS re-reads the tail and the
  bundles, up to 24 attempts.
- **Fold cost grows with lanes.** Each fold pays HEAD probes for every lane ever registered,
  which grows with the number of nodes that ever wrote the tenant.
- **Memory.** Engines are never evicted: the check is O(tenants held) in memory per tick.
- **`scripts/byoc.sh`** asserts another process cannot see an unfolded write. That holds
  under the 1-hour default and would not under a short `age`.

## M9i.2 — `consistency`

To be specified when M9i.1 lands.
