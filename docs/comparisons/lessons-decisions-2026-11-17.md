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
| 5 | One GC test across every pin kind | Built (M281): nine pins in one table; it found an external index's codes missing from GC's live set, fixed |
| 6 | Pin the S3 client's retry policy | Built (M282): conditional writes never retried, everything else pinned at 10 retries; no setting, see below |
| 7 | IVF tuning from lucene-rust | Measured (M283). **None built**; the exact scan of a filter's admitted rows is the one worth building next, see below |
| 8 | An open-loop write-throughput ladder | Built (M284): `scripts/write-ladder.sh`. It found two ceilings, see below |
| 9 | Warm a joining node from its peers | **Not built**: no multi-node deployment to measure the dip it is for, see below |
| 10 | Avoid a consensus journal | Confirmed: nothing built, the Never list already excludes it |
| 11 | Avoid inferring "latest" from a listing in GC | Confirmed: GC reaps from HEAD's own plan (M281's table pins it) |
| 12 | BYOK decides lane sharing | Recorded as an input for workstream I, below |

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
  refused none. Three wins of five. And one found later by CI: with more than 32 `applied`
  writes of one tenant in flight across its servers, an outcome can fall off HEAD's list
  before its writer reads it, so the write answers `outcome_unavailable`; the fixed cap held
  four servers to exactly 32.
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

## Lesson 5: one GC test across every pin kind

Built in M281. The live set GC checks before deleting is now one function that destructures
every field of HEAD, so a new field cannot be added without deciding whether it names an
object, and one table test buries a key under each of nine pins (live segment, branch,
borrowed segment buried again, `as_of` window, delete vector, quarantine, outcome, external
codes, replica). Its first run found one gap: an external index's codes blob was not in the
live set, so a codes blob buried by a commit that failed after its write landed would have
been deleted while HEAD still named it. Snapshots need no row: a snapshot (M236) is a HEAD
read, and its `as_of` is the window row.

## Lesson 6: the client's retry policy

Built in M282, and the hazard was real. Against a fake S3 that stores a conditional PUT and
then answers `503`, `object_store`'s own retry repeated the PUT, the repeat failed its own
`If-None-Match`, and pstore got `Lost`: for a bundle, "another writer took this lane"; for a
HEAD commit, a lost race. Azure's client did the same. Conditional writes now go through a
second client of the same bucket with no retries, so the answer is an I/O error, which M17
resolves by reading the bundle back, and which a fold or a request already retries at its
own level. Every other request keeps `object_store`'s 0.14 defaults, now written in
`retries()` rather than inherited.

**No setting**: the repeat is wrong whenever the first attempt landed, and nothing can tell
the client whether it did. The measured cost is that a `5xx` on a conditional write is no
longer hidden by up to ten retries; it reaches paths built to handle exactly that.

The same work found a second copy of the live set, in `bury_into`, missing an external
index's codes, so a registration that landed and then erred buried its own codes. It now
uses M281's `head_keys`.

## Lesson 7: IVF tuning

Measured in M283 (`crates/pstore-index/examples/ivf_tuning.rs`, numbers in
[M283's ledger](../milestones/M283/VERIFIED.md)), at the recall gate's scale. None built:

- **Centroid ranking: not built.** At the gate's 100 centroids it is 4.7% of a query's CPU
  with lists of 200 rows and 0.2% with pstore's default of 4,000. lucene-rust's 76% needs
  6,437 centroids, which at pstore's list size is a 25-million-row segment. And a query's
  CPU is microseconds beside its three round trips to the blob store. A compressed centroid
  table already exists for the case that grows (`probe_sq8`, M97).
- **A final unconstrained Lloyd round: not built.** On the clustered corpus it moved no
  vector and changed no recall; on the uniform one it moved 0.7% and changed recall by under
  half a point either way. It costs 30% more build time, and it lifts the capacity bound that
  D-8 makes a correctness property. lucene-rust's gain (10% moved, 0.949 to 0.955) was on
  real embeddings; it returns to the plan with a real-embedding corpus in the gate.
- **Skipping lists with no admitted row: not built.** Below 100 admitted rows of 20,000 it
  skips about half the lists; above 300 it skips almost none, unless the filter follows the
  vectors. Wherever it skips much, reading the admitted rows outright is cheaper still.
- **An exact scan of the admitted rows: not built yet, and the one worth building.** A
  filtered dense leg reads every list's codes and int8 records, 9.28 MB a 20,000-row
  segment at 384d. The admitted rows' own vectors are 1.5 KB each, so below about 6,000
  admitted rows (30%) reading them is fewer bytes, and exact. It needs the mask before the
  vectors, a fourth round on a cold query, which D-34 forbids; with the mask held by the
  server's filter cache (M234) it needs none. So it would be: when a segment's mask is cached
  and admits fewer rows than a byte threshold, score those rows exactly. A new query path,
  sized M, with recall and bytes both measured against today's, as a setting.
- **SOAR spilling: not built**, as lucene-rust found and the lesson says. pstore's own
  boundary replication is the same idea and the engine already builds without it (M5g).

## Lesson 8: the write ladder

Built in M284 (`scripts/write-ladder.sh`, numbers in [M284's ledger](../milestones/M284/VERIFIED.md),
`provisional`: in process, every store request 20 ms). Two ceilings, neither CPU:

- **Many tenants: the server's 64 active requests.** At 2,000 durable writes a second
  across 64 tenants, every write is acknowledged at 23 ms p50 and 24 ms p99; at 4,000, 42%
  are refused `429`. Little's law says why: 64 slots over a 22 ms write is about 2,900 a
  second. Bulks of 10 hit the same knee at ten times the documents. This is OpenSearch's
  finding exactly: the in-flight cap, not the cores, is the limit, and it should scale with
  what a request waits on. `PSTORE_REQUEST_LIMITS_CONFIG` can only lower it (M128), so
  raising it is a decision for the owner, not taken here.
- **One tenant: about 50 durable writes a second through the server.** At 50 a second the
  PUTs nearly equal the writes (142 for 150) and p99 is 202 ms; at 100, a third are refused.
  The engine alone shares bundles as designed: 100 writes 5 ms apart cost 31 PUTs (a probe run once and not kept). Through
  the HTTP handler the same arrivals share far less, so the write path above the engine adds
  something serial a request; not found within this milestone, and the next thing to look
  at. A sparse durable writer is unaffected; a hot one should use `batched`.
- **The flush does not overlap encoding with the PUT in flight**: `flush_inner` encodes under
  the `flushing` lock. At these bundle sizes encoding is microseconds beside a 20 ms PUT, so
  it is not either ceiling; it would matter only for bundles near the 8 MiB flush bound.

## Lesson 9: warming a joining node from its peers

**Not built.** The lesson asks for the post-scale-out dip measured both ways before a
default is chosen, and that needs a multi-node deployment against a real bucket, which this
project does not have (workstream J). The cache tier that would be warmed already survives a
restart on its own disk (D-23, M20), and a rolling deploy, the common case, loses
nothing. A scale-out is the case left: the new node's first queries are cold (30 to 60 times
warm, `affinity-and-coldstart.md`). When a deployment's meter shows that dip, the design is
the lesson's: `warm_from_peers`, cluster default off, over the existing peer transport,
capped in bandwidth and in time before the node turns ready.

## Lessons 10 and 11: confirmations

Nothing built, on purpose. A consensus journal is a leader by another name, which the Never
list forbids; the first dial for faster acknowledgement remains the flush window
(`batched` and `durable` already are that dial). GC reads HEAD and reaps from its graveyard,
never from a listing; M281 put every pin HEAD holds in one table so a change that weakens
that fails a test.

## Lesson 12: BYOK and lane sharing

An input for workstream I, kept here so it is not rediscovered. hsearch's model at 10M
indexes, 10% active: per-index payload keys on a shared lane cost 0.455 to 0.905 requests a
write at 5% to 50% BYOK, against 0.487 to 1.212 with an own lane for BYOK tenants. pstore's
lanes are per tenant today (lesson 1 kept them so), so BYOK is per tenant and costs nothing
in lane sharing; if cohort lanes are built, a BYOK tenant takes the `tenant` setting.
