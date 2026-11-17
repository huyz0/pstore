# Decisions on the lessons from hsearch, OpenSearch serverless and lucene-rust

**Started:** 2026-11-17. **Source:** [lessons-from-hsearch-opensearch.md](lessons-from-hsearch-opensearch.md),
whose twelve lessons Huy asked to be implemented in ranked order ("Doall"). This file records,
per lesson, what was built and in which milestone, or why it was not, with the numbers that
decided it. Every pstore number here is `provisional`: measured in a cloud container against
an in-memory store, relative only.

| # | Lesson | Outcome |
|---|---|---|
| 1 | Bundle writes across tenants | Measured (M277). **Not built**: see below |
| 2 | Request budgets that ratchet both ways | Built (M278): `docs/research/11-design/request-budgets.md`, `scripts/check-budgets.py` |
| 3 | Concurrent one-winner probes on every backend | Built (M279): three conformance probes; RustFS and Azurite pass |
| 4 | Weighted admission with a reserve, per-tenant cache floors | Built as two settings (M280): borrowing **on** by default, a cache share **off**; the floor not built, see below |

## Lesson 1: cross-tenant WAL bundling

### What pstore pays today (M277)

Read off the request counter through the handler, 100 tenants, one-row writes:

| Workload | PUTs |
|---|---|
| `durable`, a tenant's first write on a process | 2 per tenant (bundle + lane registration, once per lane lifetime) |
| `durable`, every later write | **1 per tenant** |
| a fold | 2 per tenant (segment + HEAD CAS) |
| `batched`, any write, and a fold tick | 0, until the tenant's 8 MiB flush bound |
| 8 concurrent `durable` writes, one tenant, 20 ms a request | 2 (writes arriving during a flush share the next one) |
| 64 concurrent `durable` writes, 64 tenants, 20 ms a request | **64** |

So hsearch's finding holds for pstore's `durable` writes and for nothing else: a sparse
durable writer pays 1 PUT per write, the 0.968 of hsearch's lane-per-index row, and arrivals
from different tenants never share an object however close together they are. A `batched`
writer already pays nearly nothing, because its rows wait in memory for the flush bound.

### What a cohort bundle would save

Example: one node serving 10,000 tenants, each making one `durable` write a minute, S3 at
$0.005 per 1,000 PUTs, a fold an hour.

| | today | cohort, 250 ms window |
|---|---|---|
| WAL PUTs | 167/s, about $2,160 a month | 4/s, about $52 a month |
| fold PUTs | 5.6/s, about $72 a month | unchanged |

On that workload the WAL is 97% of the request bill and a cohort cuts it by 40 times. On a
workload of `batched` writers, or of few tenants a node, it saves nothing.

### What it would cost

- **Latency.** Sharing needs a window: a durable write would wait up to the window (hsearch
  and OpenSearch's shell use 250 ms) for its PUT, where today it waits for one PUT.
- **The tenant stops being the unit of the WAL.** pstore's lane, its registry
  (`lanes.rs`), its recovery probe and its reap are all per tenant. A shared bundle can be
  reaped only once every tenant in it has folded past it, which needs either a cross-tenant
  watermark (a read of every tenant's HEAD per reap) or reference counting, and both are new
  correctness surface beside the ones M17, M39 and OQ-91 took several milestones to close.
- **Folds by another node.** Any node may fold a tenant. To find the tenant's rows in a
  shared lane it did not write, it must either read every bundle of that lane since the
  tenant's watermark (requests that scale with other tenants' traffic, which the Never list
  forbids) or be told which ones, which is a per-tenant index of a shared lane: a further
  write per window.
- **Blast radius and BYOK**, as the lessons doc records.

### Decision

**Not built now.** The saving is real and large, but only for one workload: many sparse
tenants a node writing `durable`. Every such tenant can already choose `batched` and pay
nothing, at the RPO the deploy guide states. Building a cohort lane is a change to the WAL's
unit of ownership, sized L, with a latency cost on the very writes it serves. It returns to
the plan when a real deployment's meter (M109) shows `durable` WAL PUTs from sparse tenants
are most of a bill. M277's tests then become the regression baseline, and the design starts
from D-41 plus the reap and foreign-fold questions above, as a setting
(`wal_bundling = tenant | cohort`, `tenant` the default) per the project's rule.

## Lesson 4: admission and cache shares

Both built in M280 as cluster settings, each default chosen from a deterministic measurement
(numbers in [M280's ledger](../milestones/M280/VERIFIED.md)):

- **Borrowing admission, default on.** A hot tenant alone holds 56 of 64 slots instead of 8,
  and small tenants arriving one a step, or in bursts the reserve holds, are refused no more
  than under the fixed cap. It loses one workload: a burst of 24 new tenants while the hot
  tenant holds every slot it may is refused past the reserve, 160 of 240, where the fixed cap
  refused none. Three wins of five.
- **A per-tenant cache share, default off.** At a quarter share, a cold scan beside four small
  tenants left them every hit, where one shared LRU left them none. But a tenant alone on a
  node with a working set of half the quota went from every hit to none, and so did two
  tenants of 24 entries each in a 64-entry quota. One win of four.

**Not built: the cache floor.** hsearch paired a floor (16 KiB a tenant) with its ceiling. A
floor needs eviction to skip protected entries, which is a scan of the LRU per eviction once
many tenants sit under their floors, where every other step of the memory tier is
O(log entries) (M28). The ceiling alone already gives the measured protection: the scanner
cannot hold more than its share, so the rest of the quota is the small tenants'.

**No per-index override, for either.** The project rule asks for a cluster default and a
per-index override. Admission runs before the request's index is known and before any HEAD
is read, and the cache sits beneath the engine and keys by object. Reading an index's (or a
tenant's) setting at either place is a blob request per request, which the cost model
forbids. Both stay cluster settings; a per-tenant weight can be carried in the signed
credential (M127) when one is needed, at no request.
