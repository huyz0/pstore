# Decisions on the lessons from hsearch, OpenSearch serverless and lucene-rust

**Started:** 2026-11-17. **Source:** [lessons-from-hsearch-opensearch.md](lessons-from-hsearch-opensearch.md),
whose twelve lessons Huy asked to be implemented in ranked order ("Doall"). This file records,
per lesson, what was built and in which milestone, or why it was not, with the numbers that
decided it. Every pstore number here is `provisional`: measured in a cloud container against
an in-memory store, relative only.

| # | Lesson | Outcome |
|---|---|---|
| 1 | Bundle writes across tenants | Measured (M277). **Not built**: see below |

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
