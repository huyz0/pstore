# Prior Art: turbopuffer

**Answers:** Q1
**Status:** Complete (v1)
**Retrieved:** 2026-09-05

The system `pstore` is modelled on. Understanding both what they did and **where their
design stops** is the fastest route to our own architecture.

## 1. What it is

A Rust search engine (vector ANN + BM25 + filters + aggregations) whose **source of truth is
object storage**, with RAM and NVMe purely as cache. Founded 2023 (turbopuffer Inc.,
Simon Eskildsen). Production users include Cursor, Notion (migrated off Pinecone), Linear,
Atlassian, Ramp, Grammarly, Superhuman.

Claimed scale: **1T+ documents, 10M+ writes/s, 25k+ QPS, 99.99% uptime**, no stateful
dependency in the critical path other than object storage.

## 2. Architecture as published

### Storage
- Object storage is the **source of truth**, not a tier. "Writes are durably committed to
  object storage."
- Three-tier hierarchy: object storage (~$0.02/GB) → NVMe SSD (~$0.1/GB) → RAM.
- **Each namespace is simply a prefix on object storage.** Namespaces are implicitly created
  on first insert.
- An **LSM tree built natively for object storage** (as opposed to ported from local disk).
- Shared-disk architecture; compute nodes stateless. "If a node dies, another will load into
  cache after a cold query (~500 ms)."

### Write path
1. Write lands in a memory buffer.
2. **Group commit** into a WAL file under the namespace's prefix on object storage.
3. Return success — durability is the WAL PUT.
4. Commit triggers an **async indexing job** that folds the WAL into the LSM tree.

Published numbers: p50 **165 ms for a 500 kB write**; ~10,000+ vectors/s per namespace;
**one WAL entry per second per namespace**, with concurrent writes batched into it (so a new
batch can wait up to ~1 s).

### Index
- **SPFresh** — a *centroid-based* (clustered, IVF-family) ANN index, explicitly chosen
  because it "minimizes roundtrips and write-amplification compared to graph-based indexes
  like HNSW or DiskANN."
- A centroids file for nearest-centroid lookup, plus cluster files laid out for object
  storage.
- Exact indexes for metadata filtering; BM25 for full text; trigram indexes for regex/glob.
- Recall target: **90–95% recall@10**.

### Query path
Cold query = **3–4 round trips at ~100 ms each** (≈400 ms for 1M docs):
1. metadata + filter index
2. centroid index → select clusters
3. cluster data
4. (document/attribute fetch)

Warm: **p50 14 ms** for 1M docs, having been cached on NVMe with **requests routed back to
the same node for cache locality**. Cold p50 874 ms for 1M docs. A "warm cache" pre-flight
API lets clients pre-heat.

### Consistency
- **Strong by default** — a subsequent query sees the write; costs a ~10 ms floor because
  it must check object storage.
- **Eventual mode** optional: sub-10 ms, searches up to 128 MiB of unindexed data, worst-case
  staleness "up to about one hour".

### Sharding
A namespace beyond single-index capacity is split into internal shards: **documents assigned
to shards by hash of id**, one **shared WAL**, queries fanned out and merged transparently.
Up to 256 TB per namespace with sharding.

### Coordination technique (from their queue post)
Their distributed queue is a single `queue.json` mutated by **CAS**, with:
- group commit to amortize ~200 ms writes,
- a **stateless broker** that owns all object-storage interaction so clients don't contend,
- the broker's address stored *inside* `queue.json`, heartbeats for liveness, and broker
  failover driven by CAS conflicts,
- **at-least-once** delivery via heartbeat recovery.

They state a practical ceiling of **~5 writes/second to a single file** under CAS, and that
the fix is to funnel through one process (a broker) rather than have N clients contend.

## 3. What we should copy

| Idea | Why |
|---|---|
| Object storage as source of truth, RAM/NVMe as pure cache | The entire cost thesis: $70/TB/mo vs $1,600–3,600. |
| Group commit into a WAL object | Turns the 12.5:1 PUT:GET price ratio from fatal into irrelevant. |
| Clustered (IVF/SPANN/SPFresh) index, not graph | Bounded round trips on high-latency storage. Non-negotiable. |
| Round-trip budget as a first-class design constraint | ≤3–4 sequential fetches on the cold path. |
| Prefix-per-tenant layout | Isolation, cheap idle tenants, natural S3 partitioning. |
| Stateless compute + cache-affinity routing | Any node serves any tenant; affinity is an optimization not a requirement. |
| Two consistency modes | Lets the 10 ms object-storage check be opt-out. |
| Hash-by-id sharding with a shared WAL | Simple, uniform, avoids range-split hotspots. |

## 4. Where their design stops — our opportunity

These are the specific constraints `pstore` sets out to beat.

### 4.1 Per-namespace write ceiling (~1 WAL entry/s, ~5 CAS writes/s per file)
The one-file-per-namespace CAS pattern caps a single tenant's commit rate at a handful per
second. Fine for many tenants writing a little; bad for one tenant writing a lot. It also
means write latency for a hot namespace is bounded below by the batch interval.

> **pstore direction:** multi-writer WAL. Each writer owns its own monotonic *lane*
> (`.../wal/{lane}/{seq}`), so N writers commit in parallel with **zero contention and zero
> CAS**. Ordering is established by a total order over `(seq, lane)` at read time, not by
> serializing writers. CAS is then needed only for *structural* changes (Pattern 10). This
> removes the single-file bottleneck entirely and is the main scaling difference from
> turbopuffer's published design.

### 4.2 The broker is a master
Their queue design explicitly funnels writes through a single stateless broker whose address
lives in the JSON file, with heartbeat-based failover. That is leader election with extra
steps — a small, well-hidden master, and a failover window.

> **pstore direction:** no broker, no leader, ever. Lane-per-writer removes the reason a
> broker exists (contention amortization). Where mutual exclusion is genuinely needed
> (compaction of a given shard), we use **optimistic work + CAS-on-publish**: any node may
> do the work; only one wins the commit; the loser discards. Wasted CPU is cheaper than a
> lock service, and it has no failover window.

### 4.3 Namespace = prefix, and discovery
A per-namespace prefix is right. But at millions of indexes, anything that enumerates
namespaces (admin, GC, rebalancing) becomes a listing problem.

> **pstore direction:** a **sharded catalog** of indexes, itself stored as CAS'd blobs under
> deterministic keys, so the set of indexes is readable in O(catalog shards) GETs and never
> by LIST. See `03-metadata-consistency/catalog-without-master.md`.

### 4.4 Scale target
Published material describes a cluster of Rust binaries with load-balancer routing and cache
affinity. Nothing suggests it is designed for **10,000 nodes** specifically; at that size,
routing tables, membership, and cache-affinity hashing become their own research problem.

> **pstore direction:** rendezvous hashing over a gossip-maintained (or blob-published)
> membership view, with bounded-load and churn-stable placement. See `04-cluster/`.

### 4.5 Eventual mode staleness up to ~1 hour
Worst-case staleness of an hour is a large window to expose to users.

> **pstore direction:** bounded-staleness reads with an explicit, client-specified bound
> (`max_staleness_ms`), implemented by cheap WAL tail probing (Pattern 10) rather than by
> "whatever the indexer got to."

## 5. Numbers to beat / match

| Metric | turbopuffer (published) | pstore target |
|---|---|---|
| Cold query, 1M docs | p50 874 ms / p90 444 ms (1M×768d) | ≤400 ms p50, ≤3 sequential RTs |
| Warm query, 1M docs | p50 14 ms, p90 10 ms | ≤10 ms p50 |
| Write latency | p50 165 ms @ 500 kB | ≤200 ms durable; ≤5 ms in `batched` mode |
| Per-index write rate | ~10k vectors/s, 1 WAL entry/s | **≥1M vectors/s per index** (lane parallelism) |
| Recall@10 | 90–95% | 90–95% configurable, with exact-rerank option |
| Max index size | 256 TB (sharded) | ≥1 PB |
| Cluster size | not stated | **10,000 nodes** |
| $/TB/month | ~$70 | ≤$70 |

## Sources

- [turbopuffer: fast search on object storage](https://turbopuffer.com/blog/turbopuffer)
- [turbopuffer — Architecture docs](https://turbopuffer.com/docs/architecture)
- [turbopuffer — Concepts docs](https://turbopuffer.com/docs/concepts)
- [How to build a distributed queue in a single JSON file on object storage — turbopuffer](https://turbopuffer.com/blog/object-storage-queue)
- [turbopuffer — Database of Databases](https://dbdb.io/db/turbopuffer)
- [TurboPuffer: Object Storage-First Vector Database Architecture — Jason Liu](https://jxnl.co/writing/2025/09/11/turbopuffer-object-storage-first-vector-database-architecture/)
- [Vector Podcast: Simon Eskildsen, Turbopuffer — Dmitry Kan](https://dmitry-kan.medium.com/vector-podcast-simon-eskildsen-turbopuffer-69e456da8df3)
