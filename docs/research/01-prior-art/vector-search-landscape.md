# Prior Art: The Vector / Hybrid Search Landscape

**Answers:** Q3
**Status:** Complete (v1)
**Retrieved:** 2026-09-05

## The architectural split that actually matters

Forget feature grids. There are exactly **three** architectures in this market, and they
determine cost, elasticity, and multi-tenancy far more than any feature:

### Tier 1 — Memory-resident (HNSW in RAM)
*Qdrant, Weaviate, Milvus (default), pgvector, Chroma, Vespa*

- Index lives in RAM; disk is durability only. Fastest possible warm latency
  (Weaviate: 2.8 ms mean, 4.4 ms p99).
- **Cost is the binding constraint: ~$1,600–3,600 per TB/month.** You pay for capacity you
  aren't querying.
- Idle tenants cost the same as active ones ⇒ **multi-tenancy at millions of tenants is
  economically impossible.**
- Scaling = resharding = data movement = rebalancing pain.

### Tier 2 — Object-storage-backed with stateful compute
*Pinecone Serverless, LanceDB, Milvus/Zilliz Cloud tiered*

- Pinecone Serverless: **"vector clustering on top of blob storage"**, *immutable vector
  slabs organized in an LSM-tree structure in object storage, with stateless on-demand
  executors*, "separation of reads, writes, and storage", multi-tenant compute layer caching
  warm namespaces, live index updates. Cold-start queries have higher latency.
- This is convergent evolution with turbopuffer: **LSM + immutable slabs + clustering +
  blob storage + stateless executors + cache** is now the consensus architecture.

### Tier 3 — Object-storage-native, no stateful tier at all
*turbopuffer, `pstore`, S3 Vectors*

- ~$70/TB/month. Idle tenants ≈ free. Elastic in seconds.
- Cold latency 200–500 ms is the price paid.

**`pstore` is Tier 3 and should never apologize for cold latency** — it is the direct
purchase price of a 20–50× cost reduction and unbounded tenancy. The engineering goal is to
make the *warm* path indistinguishable from Tier 1 and the *cold* path bounded and rare.

## Competitor notes

| System | Architecture | Strengths | Weakness we exploit |
|---|---|---|---|
| **Pinecone Serverless** | Tier 2, LSM slabs on blob storage, stateless executors | Zero-ops, 74k QPS @ 90% recall (BigANN 2023 filtered track), built-in inference + reranking, exact metadata filtering | Closed, single vendor cloud, p99 ~96 ms @ 600 QPS/135M vectors; no BYO object store |
| **Qdrant** | Tier 1 | Best free tier, native sparse (SPLADE, miniCOIL), ColBERT multi-vector | RAM-bound cost; per-tenant isolation expensive |
| **Weaviate** | Tier 1 | Best-in-class hybrid (vector+BM25+filters), 2.8 ms mean latency | RAM cost; scaling to millions of tenants |
| **Milvus / Zilliz** | Tier 1/2 | Billions of vectors, **Dual-Pool graph traversal** for filtered search + adaptive brute-force fallback | Operational complexity |
| **Vespa** | Tier 1 | Billion-scale hybrid, native tensors, learned ranking | Heavy, JVM, ops burden |
| **S3 Vectors** | Tier 3 (AWS-native) | $0.06/GB-mo floor, 2B vectors/index, 10k indexes/bucket | ~100 ms even when warm, no BM25/hybrid, AWS-only, limited filtering |
| **turbopuffer** | Tier 3 | The reference implementation | See `turbopuffer.md` §4 |

## Filtered search — the hardest unsolved problem, and our opening

This is where the market is genuinely weak, and where a clustered index has an advantage
that is rarely stated out loud.

**The graph-index failure mode.** With a strict filter, HNSW "hides" non-matching nodes,
which creates **islands and dead ends**: the traversal terminates early because there is no
valid path to the true nearest neighbour. Recall collapses precisely when the filter is most
selective. High-cardinality filters (`user_id`, timestamps) force a flat scan.

**The published taxonomy** (from the 2026 filtered-ANN survey):
- **Pre-filter**: build a bitset, then search within it. Works well for **partition-based
  indexes like IVF** because it prunes distance computations early; does *not* help graph
  indexes, which must still traverse.
- **Post-filter**: search then filter. Degrades severely as candidates become scarce; may
  return fewer than *k*.
- **Runtime filter**: evaluate predicates lazily during traversal; fewer evaluations but
  per-access I/O — used in disk-based systems.

**Findings by selectivity:**
- **Low selectivity (very restrictive filters): IVF-family beats HNSW**, decisively, because
  cluster pruning composes with the filter.
- **High selectivity (permissive filters):** HNSW holds throughput; post-filtering becomes
  competitive.
- System-level behaviour dominates algorithm choice: Milvus's dual-pool traversal plus
  brute-force fallback beats cleverer algorithms; pgvector's cost model frequently picks a
  worse plan than a sequential scan.
- Filter-index fusion approaches (ACORN, Filtered-DiskANN) **tightly couple metadata to the
  vector index**, which breaks down for ad-hoc queries over arbitrary schemas — a fatal
  limitation for a general-purpose multi-tenant product.
- The survey's **GLS (global-local selectivity) correlation** metric — local selectivity
  (fraction of true k-NN passing the filter) vs. global selectivity (fraction of the dataset
  passing) — is a better index-selection signal than distance-based heuristics, spanning
  nearly the full [−1, 1] range.

> **Strategic conclusion.** `pstore` chooses a clustered (IVF/SPANN-family) index for
> round-trip reasons (see `cost-and-latency.md` §2). That choice **independently** gives us
> the better filtered-search architecture, because pre-filtering composes with cluster
> pruning. Filtered search is therefore not a compromise we tolerate — it is a headline
> feature. See `06-indexing/filtering.md`.

> **Corollary.** We must implement adaptive plan selection driven by an estimated GLS/
> selectivity, with an **exact brute-force fallback** when the filtered candidate set is
> small. Milvus's result shows the fallback matters more than the clever path.

## Where the gaps are (our differentiation)

1. **Filtered + hybrid at Tier 3 economics.** S3 Vectors has the economics but not the
   features; Weaviate/Qdrant have the features but not the economics.
2. **Multi-tenancy at millions of indexes.** S3 Vectors caps at 10,000 indexes per bucket.
   Tier 1 systems are priced out. We target **millions**, with idle cost ≈ storage only.
3. **Bring-your-own-bucket / BYOC.** Data sovereignty is a real enterprise requirement and
   is structurally easy for us and hard for Tier 1.
4. **Warm latency at Tier 3 cost.** S3 Vectors' ~100 ms warm is the number to beat by 10×.
5. **10K-node elasticity.** Nobody in Tier 1/2 can add 1,000 nodes in a minute without
   rebalancing.

## Sources

- [Architecture — Pinecone serverless docs](https://docs.pinecone.io/reference/architecture/serverless-architecture)
- [Introducing Pinecone Serverless](https://www.pinecone.io/blog/serverless/)
- [Accurate and Efficient Metadata Filtering in Pinecone's Serverless Vector Database](https://www.pinecone.io/research/accurate-and-efficient-metadata-filtering-in-pinecones-serverless-vector-database/)
- [Filtered Approximate Nearest Neighbor Search in Vector Databases: System Design and Performance Analysis — arXiv](https://arxiv.org/html/2602.11443)
- [JAG: Joint Attribute Graphs for Filtered Nearest Neighbor Search — arXiv](https://arxiv.org/pdf/2602.10258)
- [Best Vector Databases in 2026 — Firecrawl](https://www.firecrawl.dev/blog/best-vector-databases)
- [Amazon S3 Vectors GA — AWS](https://aws.amazon.com/about-aws/whats-new/2025/12/amazon-s3-vectors-generally-available/)
