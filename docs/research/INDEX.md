# pstore Research Index

Master index of all research. **Read this first; it is the map.**
Every doc states which research question(s) it answers. Question IDs (`Q1`…`Q43`) are
defined in [`00-plan/research-plan.md`](00-plan/research-plan.md).

**Project:** `pstore` — masterless, object-storage-native search engine (vector + BM25 +
filters) in Rust. Unit of tenancy is an **index**; unit of commit is a **tenant**. Blob store is the *only* durable tier,
for data *and* metadata. Target: 10,000 nodes, **~50M indexes across 1M tenants**, minimal
blob-API spend.

---

**Status: research phase complete.** 40 documents, 43 research questions answered, 149 open
questions logged. Scale target: **1M tenants × up to 50 indexes = ~50M indexes**, 10% of
tenants active in any second. Next step is [M0 in the roadmap](11-design/roadmap.md) — measure the
substrate before writing an engine.

## The five load-bearing conclusions

If you read nothing else:

1. **A PUT costs 12.5 GETs; LIST is priced like a PUT and returns ≤1000 keys.** Therefore:
   batch writes ruthlessly, read freely, and never LIST.
   → [`02-object-storage/cost-and-latency.md`](02-object-storage/cost-and-latency.md)
2. **~30 ms per round trip + a 100 ms budget = ~3 sequential fetches.** This single fact
   disqualifies graph ANN indexes (HNSW/DiskANN) from the cold path and selects a clustered
   (IVF/SPANN/SPFresh) index.
   → [`06-indexing/vector-index-survey.md`](06-indexing/vector-index-survey.md)
3. **Every major cloud now has compare-and-swap on a blob** (S3 since Aug/Nov 2024). A
   masterless system with no external metadata store became possible only recently — this is
   the design's whole premise.
   → [`03-metadata-consistency/manifest-and-cas.md`](03-metadata-consistency/manifest-and-cas.md)
4. **CAS on a single key tops out at ~5 writes/s, and per-*index* batching has a cost floor
   independent of data volume** ($216k/month at 1M indexes even at a 60 s flush). So bulk
   writes touch neither CAS nor a per-index object: each node writes one **cross-index bundle**
   per window, and freshness is served from a replicated in-memory memtable so the batch window
   never appears in the time-to-searchable budget.
   → [`05-storage-engine/write-path-and-wal.md`](05-storage-engine/write-path-and-wal.md),
   [`05-storage-engine/batching-and-visibility.md`](05-storage-engine/batching-and-visibility.md)
5. **Stateless nodes ⇒ no rebalancing ⇒ elasticity in seconds.** This is what makes 10,000
   nodes tractable; cache affinity is an optimization, never a correctness requirement. It also
   means **S3 is our cross-AZ replication (free), so AZ loss is a cold-start event, not a data
   event** — while careless cross-AZ traffic would cost 10–100× the entire blob bill.
   → [`04-cluster/routing-and-placement.md`](04-cluster/routing-and-placement.md),
   [`04-cluster/az-topology.md`](04-cluster/az-topology.md)

---

## 00 — Plan
| Doc | Answers | Status |
|---|---|---|
| [research-plan.md](00-plan/research-plan.md) | — | The plan: 43 questions, 9 phases, method. |
| [open-questions.md](00-plan/open-questions.md) | D36 | **149 open questions, risk-ranked.** Tier 1 is what could invalidate the architecture. |

## 01 — Prior art
| Doc | Answers | One-line finding |
|---|---|---|
| [turbopuffer.md](01-prior-art/turbopuffer.md) | Q1 | Object-storage LSM + SPFresh + group-commit WAL; ceiling is ~1 WAL entry/s/namespace and a broker that is a hidden master. |
| [object-storage-native-systems.md](01-prior-art/object-storage-native-systems.md) | Q2 | Everyone who built ZDA pre-2024 needed an external metadata store. That constraint expired. |
| [vector-search-landscape.md](01-prior-art/vector-search-landscape.md) | Q3 | Three tiers; clustered indexes independently win *filtered* search, which is the market's weakest spot. |

## 02 — The substrate: blob stores
| Doc | Answers | One-line finding |
|---|---|---|
| [api-semantics.md](02-object-storage/api-semantics.md) | Q4 | Portable primitive set = ranged GET + atomic PUT + CAS. Not portable: append, leases, multi-range GET. |
| [cost-and-latency.md](02-object-storage/cost-and-latency.md) | Q5 | 12.5:1 write:read price; ~30 ms RTT; $70/TB-mo vs $1,600–3,600 for RAM-resident. |
| [request-efficiency-patterns.md](02-object-storage/request-efficiency-patterns.md) | Q6 | Ten reusable patterns + banned anti-patterns. The project's design language. |
| [rust-object-store-crates.md](02-object-storage/rust-object-store-crates.md) | Q7 | `object_store` (Arrow) wrapped in our own `BlobStore` trait for accounting + congestion control. |

## 03 — Metadata without a master
| Doc | Answers | Status |
|---|---|---|
| [manifest-and-cas.md](03-metadata-consistency/manifest-and-cas.md) | Q8 | One CAS'd HEAD **per tenant** = a linearizable register. **Fencing is free** — the storage layer rejects stale writers, so no locks are needed for correctness. |
| [catalog-without-master.md](03-metadata-consistency/catalog-without-master.md) | Q9 | Keys are derived from the index id, so the hot path needs no catalog at all. Enumeration = one parallel round over fixed-width buckets. |
| [consistency-model.md](03-metadata-consistency/consistency-model.md) | Q10 | Snapshot isolation + three client-chosen read modes. Freshness is a parameter, never a mystery. |

## 04 — Cluster: 10,000 nodes, no master
| Doc | Answers | Status |
|---|---|---|
| [membership.md](04-cluster/membership.md) | Q11 | SWIM+Lifeguard gossip (proven at 10K+), seeded by one GET from the blob store. Membership is an optimization; a wrong view costs cache hits, never correctness. |
| [routing-and-placement.md](04-cluster/routing-and-placement.md) | Q12 | **Rendezvous hashing is O(N) and disqualified above ~100 nodes.** Use Local Rendezvous Hashing (O(C)) + bounded-load skipping. |
| [ownership-and-leases.md](04-cluster/ownership-and-leases.md) | Q13 | Optimistic work + CAS-on-publish. Duplicate work is an economics problem, not a correctness one. `needed_work(manifest)` is a pure function — no scheduler, no queue, no recovery. |
| **[epoch-propagation.md](04-cluster/epoch-propagation.md)** | Q40 | **Don't gossip epochs — the committer computes who cares and unicasts.** Flooding 10k nodes costs ~$355k/mo in cross-AZ vs ~$107 targeted. Propagation doesn't remove the blob GET, it moves it off the query critical path; a node never serves state learned from a peer. |
| **[az-topology.md](04-cluster/az-topology.md)** | Q41 | **S3 is our cross-AZ replication and it's free** (Standard spans ≥3 AZs, no transfer charge). So run independent per-AZ cells: zero inter-AZ cost, and an AZ loss degrades to a cold-start event because nodes own nothing. Watch the NAT-gateway landmine ($116k/mo for a routing mistake). |
| **[gray-failure.md](04-cluster/gray-failure.md)** | Q42 | An AZ degraded-but-alive costs 17–67× effective latency with every liveness check green. Per-AZ cells removed the cross-AZ traffic that would reveal it — buy it back for ~$16/mo (probe mesh) + ~$10/mo (blob health bulletin, which survives an inter-AZ partition). Peer-relative outlier detection; drain by **self-eviction**, so no control plane is in the recovery path. |
| [load-and-hotspots.md](04-cluster/load-and-hotspots.md) | Q14 | Size skew → shards; rate skew → **dynamic replication factor, free because nodes own nothing**; cold start → singleflight + centroid-first cache fill. |

## 05 — Storage engine
| Doc | Answers | Status |
|---|---|---|
| [write-path-and-wal.md](05-storage-engine/write-path-and-wal.md) | Q15 | **The central mechanism.** Per-writer lanes + group commit ⇒ 1 PUT per batch, 0 CAS, no contention at any writer count. Tail found by probing + an 8 KiB lane bitmap. *(Partly superseded by the row below.)* |
| **[batching-and-visibility.md](05-storage-engine/batching-and-visibility.md)** | Q32 | **Per-index batching has a $216k–$13M/month floor at 1M indexes at any batch size.** Bundle across tenants (PUTs scale with nodes, not indexes) and serve freshness from a replicated memtable — ~1 ms visibility with hour-scale batching. Corrects the Express One Zone and multipart-cost claims elsewhere. |
| [file-format-and-layout.md](05-storage-engine/file-format-and-layout.md) | Q16 | Self-describing segment; one `Range: -N` suffix GET bootstraps it. No sidecars. Parquet rejected (random access); Lance is a real alternative. |
| [compaction.md](05-storage-engine/compaction.md) | Q17 | On blob storage, space amp is cheap and **read amp is expensive** (30 ms/hop) — so bias leveled, bound segments per query to ~10. Compaction costs <$0.001 in requests; CPU is the constraint. |
| [mutations-and-mvcc.md](05-storage-engine/mutations-and-mvcc.md) | Q18 | Immutable ⇒ MVCC, time travel, and **branching for one PUT** all fall out for free. Deletes via roaring delete vectors. |

## 06 — Indexing
| Doc | Answers | Status |
|---|---|---|
| [vector-index-survey.md](06-indexing/vector-index-survey.md) | Q19 | HNSW and DiskANN need data-dependent hop chains ⇒ dead on 30 ms storage. SPANN/SPFresh needs a **fixed 2**. Small indexes use exact scan — and most indexes are small. |
| [quantization.md](06-indexing/quantization.md) | Q20 | **RaBitQ over PQ**: no per-tenant training (decisive at millions of tenants) and a real error bound (PQ has none and fails badly on some data). 1-bit = 96 GB per 1B vectors. |
| [full-text-search.md](06-indexing/full-text-search.md) | Q21 | Inverted indexes already are ranged-read structures. Tantivy behind a custom `Directory`. Block-max metadata must live in the *cached* index section — a skipped block is a skipped network fetch. |
| **[modalities-and-sequencing.md](06-indexing/modalities-and-sequencing.md)** | Q39 | Dense, sparse, and BM25 are one structure: postings with a generic impact payload. Build the general data model (named plural vectors, optional sections, `prefetch[]`+`fusion`) in v1; ship one retriever at a time. **Sparse before BM25** — it's exact, so it's far cheaper. |
| [filtering.md](06-indexing/filtering.md) | Q22 | Graph indexes collapse under selective filters (islands/dead ends). Clustered indexes compose with pre-filtering. **Our round-trip choice hands us the better filtering architecture for free.** |
| [incremental-maintenance.md](06-indexing/incremental-maintenance.md) | Q23 | LIRE/SPFresh touches only boundary vectors: 1% of DRAM, <10% of cores vs. global rebuild. Adapting it to immutable objects is **the design's biggest open risk (OQ-51)**. |

## 07 — Caching
| Doc | Answers | Status |
|---|---|---|
| [cache-hierarchy.md](07-caching/cache-hierarchy.md) | Q24 | Class-aware admission, not one big LRU: centroids and index sections must never be evicted by bulk traffic. `foyer` for the hybrid RAM+NVMe tier. **Immutable ids ⇒ cache entries never need invalidation.** |
| **[disk-space-management.md](07-caching/disk-space-management.md)** | Q34 | **Endurance binds before capacity.** Flash DLWA goes 1.3→3.5 from 50%→100% utilization, so cache max is ~64% of the device and fill is capped at ~67 MB/s — a cold node takes ~10 h. A full disk must degrade to bypass mode, never fail. Closes OQ-54 and OQ-57. |
| **[storage-to-cache-ratio.md](07-caching/storage-to-cache-ratio.md)** | Q37 | **R = managed bytes ÷ resident bytes** is the economic thesis as one number. 31.6× from quantization + tier separation, × 1/f from the clustered index ⇒ ~316:1. **Capacity never binds; QPS binds by 1–2 orders of magnitude.** Store generously, cache stingily. |
| [affinity-and-coldstart.md](07-caching/affinity-and-coldstart.md) | Q25 | Cold is 30–60× warm, so minimize the *number* of cold queries. NVMe cache must survive process restarts, or a rolling deploy flushes 10,000 caches. |

## 08 — Query engine
| Doc | Answers | Status |
|---|---|---|
| [query-path.md](08-query-engine/query-path.md) | Q26 | Three round trips, with RT-A speculatively fetching everything knowable before touching data. Plan selection is free and **cache-aware**. |
| [hybrid-and-ranking.md](08-query-engine/hybrid-and-ranking.md) | Q27 | RRF by default (robust to score drift). **Cross-shard BM25 needs two-pass IDF** — easy to ship wrong, hard to notice. No inference in v1. |

## 09 — Rust stack
| Doc | Answers | Status |
|---|---|---|
| [crate-survey.md](09-rust-stack/crate-survey.md) | Q28 | `object_store`, `arrow-rs`, `simsimd`, `roaring`, `tantivy`, `foyer`. **The deterministic simulator is built before the distributed features, not after.** One binary, all roles. |
| **[memory-management.md](09-rust-stack/memory-management.md)** | Q35 | **Rust allocation failure aborts and is not catchable**, so limits live above the allocator. In-flight fetch bytes (fan-out × block × concurrency) are the OOM source: score-and-drop makes query memory O(k + resident), not O(scanned). Byte reservations, an emergency reserve for the flush path, `memory.high` + PSI, and a degradation ladder that ends in 429 rather than abort. |
| **[cpu-management.md](09-rust-stack/cpu-management.md)** | Q36 | **QPS is not a unit of capacity** — warm scan is memory-bandwidth-bound, so QPS/node swings 75× (16→1,221) with scan size. Capacity is bytes-scanned/s. Guard against metastable collapse: hedging off under load, retry budgets, CoDel. **Set CPU requests, never CPU limits.** |
| **[hot-loop-performance.md](09-rust-stack/hot-loop-performance.md)** | Q43 | **The scan is memory-bound by ~7× per core**, so the win is fewer bytes, not faster instructions — budget SIMD effort accordingly. Huge pages cut a 96 MB scan from 23,438 TLB entries to 46. Rust 1.87 made most `std::arch` intrinsics safe. Adopt Polars' morsel+permit backpressure and German strings. |
| [runtime-and-io.md](09-rust-stack/runtime-and-io.md) | Q29 | Tokio (work-stealing suits our wildly skewed work) + a separate rayon pool for SIMD. io_uring's win doesn't apply to 30 ms HTTPS. Round-trip depth is a **tested invariant**. |

## 10 — Benchmarks & cost
| Doc | Answers | Status |
|---|---|---|
| [evaluation-methodology.md](10-benchmarks-cost/evaluation-methodology.md) | Q30 | Never report latency without cache state. QPS and recall are one number, not two. Recall and round-trip depth are CI gates. |
| **[tenancy-scale-model.md](10-benchmarks-cost/tenancy-scale-model.md)** | Q33 | **1M tenants × 50 indexes, 90% idle.** Naive per-index flushing costs $1.3M–$65M/month. Three fixes: fan writes *in* to `W = bytes/s × T / B` nodes; make the **tenant** the CAS unit (50×); co-locate small tenants' reads. Write path lands at ~$14k/month. |
| [cost-model.md](10-benchmarks-cost/cost-model.md) | Q31 | At 100M docs: storage $8/mo, writes $1/mo, queries $780/mo, **compute $8,760/mo (92%)**. This is a compute-efficiency business. Idle tenants are free. |

## 11 — Design synthesis
| Doc | Answers | Status |
|---|---|---|
| **[architecture.md](11-design/architecture.md)** | D32 | **Start here for the design.** The five constraints, the diagram, the write/read paths, and how we beat turbopuffer. |
| [key-layout.md](11-design/key-layout.md) | D33 | Every key derivable; **seven kinds of mutable object in the entire system**. Storage-class routing by prefix. |
| **[session-and-affinity-protocol.md](11-design/session-and-affinity-protocol.md)** | Q38 | One opaque token solves read-your-writes *and* cold-cache routing, because the nodes holding fresh data are the nodes we'd route to for warmth. **`session` becomes the default consistency mode** (as in Cosmos DB): read-your-writes at ~1 ms and 0 blob requests. |
| [api-design.md](11-design/api-design.md) | D34 | Every tradeoff (consistency, recall, completeness) is a client parameter. Responses report freshness and cost. |
| [roadmap.md](11-design/roadmap.md) | D35 | Built in descending order of "if this is wrong, the architecture is wrong." M0 measures CAS before anything else. |

---

## Glossary of pstore terms

| Term | Meaning |
|---|---|
| **Tenant** | Unit of **physical grouping, commit, and CAS**. ~1M of them, ~50 indexes each. |
| **Index** | Unit of **API, schema, query, and isolation**. ~50M of them. (turbopuffer calls this a namespace.) Deliberately *not* the same as the tenant — see [tenancy-scale-model](10-benchmarks-cost/tenancy-scale-model.md) §4. |
| **Write cohort** | The `W`-node derivable subset that buffers and flushes writes. `W = write_bytes/s × T / B`. |
| **Shard** | A horizontal partition of one index, by hash of document id. |
| **Lane** | A per-writer append-only WAL stream. Lanes remove CAS from the write path. |
| **Epoch** | Monotonic counter incremented on structural change; part of every key. |
| **Manifest** | The CAS'd blob naming the committed state of an index at an epoch. |
| **RA** | Request amplification: blob requests per logical op, split W / Rseq / Rpar / List. |
| **Segment** | An immutable data object (data blocks + index blocks + footer). |
| **Footer** | Self-describing trailer at a known suffix offset; one `Range: -N` GET bootstraps a segment. |
