# pstore — Research Plan

**Project:** `pstore` — a massively scalable, object-storage-native search/vector engine in Rust.
**Status:** Research phase (design precedes code).
**Owner:** Huy Nguyen
**Started:** 2026-09-05

---

## 1. Product thesis (the thing we are building)

A turbopuffer-class search engine, rebuilt with harder scaling constraints:

| Dimension | Requirement |
|---|---|
| Unit of tenancy | **Index** (not "namespace"). Millions of indexes, each 0 → 100B+ rows. |
| Source of truth | **Blob store only.** Data *and* metadata. No Postgres, no etcd, no ZooKeeper, no DynamoDB. |
| Storage backends | S3, GCS, Azure Blob — pluggable, plus S3-compatible (R2, Tigris, MinIO). |
| Topology | **No master.** No leader election for the control plane. Scales to **10,000 nodes**. |
| Local state | Local NVMe + RAM are **pure cache**. Any node can be destroyed at any time with zero data loss and no rebuild coordination. |
| Blob API discipline | **LIST is nearly banned.** Optimize for few, large PUTs. Favor GET/HEAD (cheap, ~10x cheaper than PUT on S3). |
| Workloads | Vector ANN, BM25 full-text, filtered/hybrid search, aggregations. |

The blob-API discipline is the spine of the whole design. Every subsystem is judged by
its **request amplification**: requests per write, requests per query, requests per
compaction, requests per node join.

---

## 2. Research questions (what we must answer before writing code)

Each is tracked to a document. `Q` = question, `D` = the doc that answers it.

### A. Prior art
- **Q1** What exactly does turbopuffer do, and what did they publish about how? → `01-prior-art/turbopuffer.md`
- **Q2** Which systems already treat object storage as the only durable tier, and what did they learn? (WarpStream, SlateDB, Neon, Quickwit, LanceDB, Delta/Iceberg, S3 Vectors) → `01-prior-art/object-storage-native-systems.md`
- **Q3** What is the competitive landscape for vector/hybrid search, and where are the gaps? → `01-prior-art/vector-search-landscape.md`

### B. The substrate: blob stores
- **Q4** What is the exact API surface across S3/GCS/Azure? Which primitives are portable, which are not? Conditional writes, ETags, generations, leases, multipart, ranged GET, tags, versioning, batch delete. → `02-object-storage/api-semantics.md`
- **Q5** What do operations actually cost and what do they actually take (latency distributions, TTFB vs. throughput, first-byte vs. last-byte, concurrency scaling, per-prefix throughput limits)? → `02-object-storage/cost-and-latency.md`
- **Q6** Given Q4/Q5, what are the *design rules* for minimizing request count? How do we avoid LIST entirely? → `02-object-storage/request-efficiency-patterns.md`
- **Q7** What Rust crates exist for blob access, and are they good enough? → `02-object-storage/rust-object-store-crates.md`

### C. Metadata without a master
- **Q8** How do we hold a mutable pointer (the index manifest) with only blob-store primitives? CAS chains, epoch fencing, log-structured manifests, ABA hazards. → `03-metadata-consistency/manifest-and-cas.md`
- **Q9** How do we hold *millions* of indexes' metadata without ever listing a bucket? Deterministic key derivation, catalog sharding, index-of-indexes. → `03-metadata-consistency/catalog-without-master.md`
- **Q10** What consistency/isolation do we promise? Read-your-writes, snapshot isolation, staleness bounds, and what conditional-write support each cloud gives us. → `03-metadata-consistency/consistency-model.md`

### D. Cluster: 10,000 nodes, no master
- **Q11** How do nodes find each other and agree on membership at 10K scale without a coordinator? SWIM/gossip vs. blob-store-published membership. → `04-cluster/membership.md`
- **Q12** How is work assigned to nodes with no assigner? Rendezvous/consistent hashing, virtual nodes, churn cost, cache-affinity-preserving routing. → `04-cluster/routing-and-placement.md`
- **Q13** How do we prevent two nodes doing the same mutation (compaction, indexing) without a lock service? Blob-store leases, fencing tokens, idempotent/optimistic work. → `04-cluster/ownership-and-leases.md`
- **Q14** How does load balance in practice — hot indexes, huge indexes, cold-start stampedes? → `04-cluster/load-and-hotspots.md`

### E. Storage engine
- **Q15** What is the write path? WAL on blob store, group commit, batching economics (how many writers can share one PUT), durability latency. → `05-storage-engine/write-path-and-wal.md`
- **Q16** What is the on-disk (on-blob) layout? LSM levels vs. tiered vs. delta-on-base; file formats; column layout; how a single blob is structured for ranged reads. → `05-storage-engine/file-format-and-layout.md`
- **Q17** Compaction: policy, who runs it, how it is scheduled without a master, and its blob-request cost. → `05-storage-engine/compaction.md`
- **Q18** Deletes, updates, MVCC, tombstones, TTL, and index branching/copy-on-write. → `05-storage-engine/mutations-and-mvcc.md`
- **Q35** How do we manage memory so the process degrades instead of being OOM-killed, given
  that Rust allocation failure aborts uncatchably? → `09-rust-stack/memory-management.md`
- **Q34** How do we manage local disk so a full cache degrades instead of taking a node down,
  and how large / how fast may the cache actually be? → `07-caching/disk-space-management.md`
- **Q33** What does the real tenancy shape (1M tenants × up to 50 indexes, 10% active per
  second) do to the design? → `10-benchmarks-cost/tenancy-scale-model.md` *(closes OQ-84)*
- **Q32** How do we bundle more per PUT without delaying search-after-write? Cross-tenant
  bundling, the per-index flush floor, freshness layers, fold-rate economics. →
  `05-storage-engine/batching-and-visibility.md` *(added after P5; the write-path doc's
  per-index model did not survive contact with the tenancy scale)*

### F. Indexing algorithms
- **Q19** Which ANN structures survive on high-latency storage? Graph (HNSW/Vamana/DiskANN) vs. clustered (IVF/SPANN/SPFresh). Round-trip counts. → `06-indexing/vector-index-survey.md`
- **Q20** Quantization: PQ/OPQ, SQ, binary, RaBitQ — recall/size/CPU tradeoffs and rerank strategy. → `06-indexing/quantization.md`
- **Q21** Full-text: BM25 on blob storage, posting list layout, skip structures, block-max WAND. → `06-indexing/full-text-search.md`
- **Q22** Filtering: pre- vs. post-filter, predicate pushdown, zone maps, bitmap indexes, and filtered-ANN recall collapse. → `06-indexing/filtering.md`
- **Q23** Incremental index maintenance: how do we avoid rebuilding a 1B-vector index on every write? → `06-indexing/incremental-maintenance.md`

### G. Caching
- **Q24** Cache hierarchy design: RAM + NVMe, admission, eviction, hybrid cache crates (foyer, moka), sizing. → `07-caching/cache-hierarchy.md`
- **Q25** How does cache affinity survive node churn at 10K nodes? Cold-start cost and mitigation. → `07-caching/affinity-and-coldstart.md`

### H. Query engine
- **Q26** End-to-end query path with a round-trip budget. Planning, vectorized execution, pagination, aggregation. → `08-query-engine/query-path.md`
- **Q27** Hybrid ranking: fusion strategies, multi-vector, reranking. → `08-query-engine/hybrid-and-ranking.md`

### I. Implementation stack
- **Q28** Rust crate survey: runtime, IO, SIMD, arrow, tantivy, serialization, RPC/API. → `09-rust-stack/crate-survey.md`
- **Q29** Async IO and concurrency architecture for a request-amplification-bound system. → `09-rust-stack/runtime-and-io.md`

### J. Validation
- **Q30** How do we benchmark this honestly? Datasets, recall metrics, cold/warm separation, cost-per-query. → `10-benchmarks-cost/evaluation-methodology.md`
- **Q31** What is the unit-economics model? $/TB/month, $/1M writes, $/1M queries, and where the cliffs are. → `10-benchmarks-cost/cost-model.md`

### K. Synthesis
- **D32** Proposed architecture. → `11-design/architecture.md`
- **D33** Blob-store key layout and object taxonomy. → `11-design/key-layout.md`
- **D34** Public API design. → `11-design/api-design.md`
- **D35** Build roadmap and de-risking order. → `11-design/roadmap.md`
- **D36** Open questions and known risks. → `00-plan/open-questions.md`

---

## 3. Method

1. **Source-first.** Every non-obvious claim in a research doc carries a citation. Vendor
   pricing/latency numbers get a `retrieved:` date because they rot.
2. **Numbers over adjectives.** "Cheap" is not a finding; "$0.0004 per 1000 GETs, ~$0.005
   per 1000 PUTs, 12.5x ratio" is.
3. **Request-amplification budget.** Every design doc states its blob-request cost per
   operation. This is the currency of the project.
4. **Falsifiable.** Where a claim is load-bearing and uncertain, it goes to
   `00-plan/open-questions.md` with a proposed experiment.
5. **Index discipline.** `docs/research/INDEX.md` is updated whenever a doc lands. Each doc
   opens with a `Status / Answers / Key findings` header so the index can be rebuilt from
   the docs themselves.

## 4. Execution order

| Phase | Docs | Why first |
|---|---|---|
| P0 | Plan, index | Scaffolding |
| P1 | B (Q4–Q7) | The substrate constrains everything else |
| P2 | A (Q1–Q3) | Learn from who's done it |
| P3 | C (Q8–Q10) | Masterless metadata is the highest-risk novel part |
| P4 | D (Q11–Q14) | 10K-node topology |
| P5 | E (Q15–Q18) | Storage engine |
| P6 | F (Q19–Q23) | Indexing |
| P7 | G, H (Q24–Q27) | Caching + query |
| P8 | I, J (Q28–Q31) | Stack + validation |
| P9 | K | Synthesis |
