# pstore Architecture

**Synthesizes:** D32 — the payoff document. Every claim here traces to a research doc.
**Status:** v1 proposal

---

## 1. In one paragraph

`pstore` is a search engine (vector ANN + BM25 + filters + hybrid) whose only durable
dependency is a blob store, for **data and metadata alike**. The unit of tenancy is an
**index**. Nodes are identical, stateless, and own nothing; RAM and NVMe are pure cache. There
is **no master, no leader election, no external metadata store, and no lock service** — state
transitions are compare-and-swap on a single blob per index, which the storage layer itself
fences. Bulk writes never touch the CAS path: each writer appends to its own **lane**, so
10,000 nodes can write to one index concurrently with zero coordination. Reads are budgeted at
**three sequential blob round trips**, which is what forces a clustered (SPANN/SPFresh) vector
index rather than a graph one — and which, as a bonus, gives us the better architecture for
filtered search.

## 2. The five constraints that generated the design

| # | Constraint | Source | Consequence |
|---|---|---|---|
| C1 | A PUT costs 12.5 GETs; LIST is PUT-priced and returns ≤1000 keys | [cost-and-latency](../02-object-storage/cost-and-latency.md) | Group commit; deterministic keys; **LIST is banned** |
| C2 | ~30 ms per round trip; ~100 ms budget | [cost-and-latency](../02-object-storage/cost-and-latency.md) | ≤3 sequential fetches ⇒ clustered index, not graph |
| C3 | Blob CAS exists everywhere now (S3 since 2024) | [api-semantics](../02-object-storage/api-semantics.md) | A masterless metadata plane is finally possible |
| C4 | CAS on one key ≈ 5 writes/s | [manifest-and-cas](../03-metadata-consistency/manifest-and-cas.md) | Lanes; partitioned registers; CAS only for structural change |
| C5 | Stateless ⇒ no rebalancing ⇒ elasticity in seconds | [routing-and-placement](../04-cluster/routing-and-placement.md) | 10,000 nodes is tractable; placement is a hint |

## 3. System diagram

```
                    ┌─────────── clients ───────────┐
                    │  HTTP/JSON  ·  gRPC  ·  binary │
                    └───────────────┬────────────────┘
                          plain round-robin L4/L7 LB
                                    │   (no affinity config needed)
   ┌────────────────────────────────┼────────────────────────────────┐
   │        10,000 IDENTICAL, STATELESS pstore nodes                 │
   │  ┌──────────────────────────────────────────────────────────┐  │
   │  │ API · planner · vectorized exec · SIMD scan               │  │
   │  │ writer (lanes, group commit)                              │  │
   │  │ indexer · compactor · GC   ← optimistic, CAS-on-publish   │  │
   │  │ gossip (SWIM+Lifeguard): membership, load, epoch hints    │  │
   │  ├──────────────────────────────────────────────────────────┤  │
   │  │ CACHE (pure cache, no durability)                         │  │
   │  │   RAM : manifests · index sections · centroids · FSTs     │  │
   │  │   NVMe: quantized vectors · postings · docs  (persists    │  │
   │  │         across restarts — immutable ids, never stale)     │  │
   │  ├──────────────────────────────────────────────────────────┤  │
   │  │ BlobStore trait: coalescing, congestion control,          │  │
   │  │                  per-tenant request accounting            │  │
   │  └──────────────────────────────────────────────────────────┘  │
   └────────────────────────────────┬────────────────────────────────┘
                                    │
                   ┌────────────────▼─────────────────┐
                   │  BLOB STORE — the only durable    │
                   │  tier.  S3 / GCS / Azure / R2     │
                   │  data + metadata + cluster seed   │
                   └───────────────────────────────────┘

   NO master · NO etcd/ZK · NO Postgres · NO DynamoDB · NO lock service
```

## 4. Data model

| Concept | Definition |
|---|---|
| **Index** | Unit of tenancy, isolation, schema, and billing. Millions of them. |
| **Shard** | Horizontal partition of an index by `hash(doc_id)`. Fixed at creation, resharding is a rewrite. |
| **Document** | Id + optional vector(s) + typed attributes. |
| **Segment** | Immutable object: data blocks + index section + footer. |
| **Lane** | Single-writer append-only WAL stream. |
| **Epoch** | Monotonic version of an index's committed state. A snapshot. |

## 5. The object layout

```
{hash4}/idx/{index_id}/
    HEAD                                  ← the ONLY mutable object. CAS'd.
    m/{epoch}.manifest                    ← immutable; base + delta chain
    branches                              ← CAS'd; refcount roots for GC
    s{shard}/
        wal/{lane_id}/{seq}.wal           ← immutable, write-once, NO CAS
        lanemap/{gen}                     ← 8 KiB bitmap of live lanes
        seg/{level}/{epoch}-{ulid}.seg    ← immutable segments
        dv/{segment_id}/{epoch}.dv        ← delete vectors (roaring)
        claims/{work_hash}                ← advisory only, never correctness
{hash4}/cat/...                           ← sharded catalog (enumeration only)
{hash4}/clu/ROSTER                        ← gossip seed
```

Every key is **derivable**. Nothing is ever discovered by LIST.

## 6. The write path

```
client → any node → per-(index,shard) buffer → group commit (8–64 MiB or 50 ms)
       → ONE PUT to {lane}/{seq}                          ← durable, ack
       → (async) indexer folds lanes into a segment
       → (async) CAS HEAD to publish the new epoch
```

- **RA(write batch) = 1 W. CAS operations per write = 0.**
- N writers ⇒ N lanes ⇒ **no contention at any N**.
- Readers find lanes via an 8 KiB bitmap + forward probing, never LIST.
- Three durability modes: `durable` (blob PUT, ~50–250 ms), `batched` (peer-replicated,
  ~1–5 ms, bounded loss), `async`.

→ [write-path-and-wal](../05-storage-engine/write-path-and-wal.md)

## 7. The read path

```
RT-A (1 hop, wide):  HEAD (usually 304) ‖ index sections ‖ centroids ‖ filter columns
   compute:          filter → bitmap; estimate selectivity; pick plan; score centroids
RT-B (1 hop, wide):  p posting lists / clusters, across all segments and shards
   compute:          SIMD scan of 1-bit codes ∧ bitmap → top(k·oversample)
RT-C (1 hop, wide):  rerank codes + documents for survivors
   compute:          rerank ladder, cross-shard merge, RRF fusion
```

Cold ≈ 100–400 ms. Warm ≈ 0 blob requests, compute-bound, **target ≤10 ms p50**.

Plan selection is **free** (all inputs cached) and is **cache-aware** — if the exact-scan data
is warm and the ANN data is cold, exact scan may be both faster and more accurate.

→ [query-path](../08-query-engine/query-path.md)

## 8. Indexing

| Layer | Choice | Why |
|---|---|---|
| Vector ANN | **SPANN-family clustered**, LIRE/SPFresh maintenance | Fixed 2 round trips vs. a graph's data-dependent chain (C2); also wins filtered search |
| Small indexes (<~100 MB) | **Exact brute-force** | Faster, exact, zero maintenance — and this is *most* indexes |
| Quantization | **RaBitQ 1-bit** scan tier, int8 rerank, f16 exact | 32× compression ⇒ more vectors per round trip; **no per-tenant training**, unlike PQ; theoretical error bound, unlike PQ |
| Full text | **Tantivy behind a custom `Directory`**; block-max metadata hoisted into the cached index section | A skipped block is a skipped *network fetch* |
| Filtering | Adaptive pre-filter / post-filter / exact, roaring bitmaps, zone maps, always an exact fallback | Pre-filter composes with cluster pruning; graph indexes can't do this |
| Hybrid | **RRF** by default (robust to score drift), weighted optional; two-pass IDF for correct cross-shard BM25 | |

→ [vector-index-survey](../06-indexing/vector-index-survey.md) ·
[quantization](../06-indexing/quantization.md) ·
[filtering](../06-indexing/filtering.md)

## 9. Coordination — the part with no precedent

| Traditional need | `pstore` answer |
|---|---|
| Metadata store | HEAD blob + CAS |
| Leader election | none — **fencing is free** because CAS rejects stale writers |
| Distributed lock | none — advisory claims only; a violated claim wastes CPU, never corrupts |
| Scheduler / work queue | `needed_work(manifest)` — a **pure function**, regenerated from state; nothing to lose or recover |
| Service discovery | one GET of `clu/ROSTER`, then gossip |
| Rebalancing | none — nodes own nothing |
| Failover | none — no ownership to fail over |
| Split-brain resolution | none — both partitions serve correctly, at lower cache hit rate |

**Invariant I1:** *No node ever mutates an object another node might read, and every state
transition is a CAS on a single key conditioned on the exact version the actor observed.*

→ [manifest-and-cas](../03-metadata-consistency/manifest-and-cas.md) ·
[ownership-and-leases](../04-cluster/ownership-and-leases.md)

## 10. How we beat turbopuffer

| Dimension | turbopuffer | pstore |
|---|---|---|
| Per-index write rate | ~1 WAL entry/s, ~10k vectors/s | **lanes** ⇒ blob-rate-limited, target ≥1M vectors/s |
| Coordination | a stateless **broker** (a hidden master, with a failover window) | none |
| Staleness in eventual mode | "up to about one hour" | **client-specified bound** |
| Fleet scale | not stated | **10,000 nodes** (LRH + gossip) |
| Metadata for millions of indexes | prefix-per-namespace | sharded blob catalog, enumeration in **1 parallel round** |
| Recall control | fixed 90–95% target | **client-selectable rerank ladder** |
| Time travel / branching | branching offered | branching is **1 PUT**; epochs optionally public |

## 11. What we deliberately do not build

- Multi-document transactions, cross-index consistency, global ordering.
- Model inference / embedding generation / reranking models (different product, would add a
  second stateful dependency).
- A control plane, a metadata service, or any process that must be running for the data to be
  readable.
- Cross-region strong consistency.
- Anything depending on Azure leases, append blobs, or non-portable primitives.

## 12. The biggest risks

| Risk | Where | Mitigation |
|---|---|---|
| LIRE's guarantees may not survive batching into immutable-segment rewrites | [OQ-51](../00-plan/open-questions.md) | **Prototype and measure early — this is the #1 technical unknown** |
| CAS throughput/contention worse than the ~5/s estimate | OQ-5 | Partitioned per-shard registers; measure first |
| Lane tail discovery costs more than expected | OQ-22 | Model probe-vs-pointer; the lane bitmap is the hedge |
| Warm latency doesn't reach 10 ms | OQ-75 | It's a compute problem; SIMD + zero-copy + Tokio/rayon split |
| Cold-query ratio too high in practice | OQ-57 | Shadow warming, persistent NVMe cache, warm API |
| S3 Vectors commoditizes the category | — | Compete on hybrid + filtering + warm latency + BYOC, not on $/GB |

## 13. Reading order for a newcomer

1. [cost-and-latency](../02-object-storage/cost-and-latency.md) — the physics
2. [request-efficiency-patterns](../02-object-storage/request-efficiency-patterns.md) — the language
3. [manifest-and-cas](../03-metadata-consistency/manifest-and-cas.md) — the novel part
4. [write-path-and-wal](../05-storage-engine/write-path-and-wal.md) — the scaling part
5. [vector-index-survey](../06-indexing/vector-index-survey.md) — the algorithm choice
6. this document
