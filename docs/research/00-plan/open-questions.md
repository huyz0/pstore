# Open Questions and Risks

**Answers:** D36
**Status:** Living document — every research doc appends here.

Each entry: what we don't know, why it matters, and how to find out. Sorted by risk.

## Tier 1 — could invalidate the architecture

| ID | Question | Impact | Experiment |
|---|---|---|---|
| **OQ-51** | Does LIRE (SPFresh) preserve partition quality when its split/merge/reassign decisions are **batched into immutable-segment rewrites** instead of applied in place? | If not, every compaction needs full re-clustering — a large cost-model change. **The biggest unvalidated assumption in the design.** | M3.5 spike: implement both, compare recall and partition-length distribution over a simulated 90-day update stream. |
| **OQ-5** | Real CAS throughput and loss-rate curves per backend under 2 / 8 / 64 / 512 concurrent contenders. | The "~5 writes/s" figure is borrowed, not measured. Everything about commit rate and register partitioning depends on it. | M0 microbenchmark inside each cloud. |
| **OQ-22** | Is forward-probing the lane tail with `k` parallel GETs cheaper than a per-lane tail-pointer object updated every M writes? | Determines read-path cost of the whole lane design. | Model both, then measure with realistic write-rate distributions. |
| **OQ-75** | Real warm queries-per-second per node. Currently a guess (200/s). | The dominant term in the cost model (compute is 92% of cost). | Benchmark the scan path on target instance types. |
| **OQ-84** | What fraction of indexes are trickle writers, and what is the real index-size distribution? | Sets the value of cross-index bundling and the inline-small-index threshold — i.e. whether we are optimizing the common case or a hypothetical one. | Customer discovery + any public SaaS tenant-distribution data; build the synthetic generator from it (OQ-72). |
| **OQ-85** | The three-way optimum between memtable memory budget, fold rate, and recovery-scan window. | Fold rate is the dominant per-index PUT cost after bundling; memtable size bounds both query cost and how lazy folding can be. | Model analytically, then measure in M1/M2. |
| **OQ-91** | Prove that `HEAD.lane_watermarks` + forward probing of placement nodes' bundle lanes finds **every** un-folded record under placement change, fallback writes, and node death. | If recovery can miss records, cross-index bundling is unsafe and the whole cost argument collapses. | Deterministic simulation target for M2. |

## Tier 2 — significant design impact

| ID | Question | Where |
|---|---|---|
| OQ-2 | Break-even gap `G*` for range coalescing, per backend | `02/request-efficiency-patterns` |
| OQ-3 | Real p50/p99/p999 TTFB per backend per object size | `02/cost-and-latency` |
| OQ-6 | Does the ABA nonce fully close the hole under S3 multipart ETags? | `03/manifest-and-cas` |
| OQ-10 | Cap on unindexed WAL volume before backpressure | `03/consistency-model` |
| OQ-12 | Does Lifeguard's false-positive suppression hold when nodes are CPU-pinned by SIMD work? Our workload is SWIM's pathological case. | `04/membership` |
| OQ-16 | Quantify the cache dip after a 2× scale-out; is shadow warming worth its cost? | `04/routing-and-placement` |
| OQ-24 | S3 Express One Zone durability semantics; dual-write cost at real batch sizes | `05/write-path-and-wal` |
| OQ-26 | Our segment format vs. Lance for random access and scan — should we adopt rather than build? | `05/file-format-and-layout` |
| OQ-35 | Two-index design (SPANN cold / DiskANN warm) — measure before rejecting permanently | `06/vector-index-survey` |
| OQ-39 | RaBitQ vs BBQ vs int8 on our target embedding families and dimensions | `06/quantization` |
| OQ-43 | Tantivy-over-`Directory` request amplification for a realistic multi-term query | `06/full-text-search` |
| OQ-46 | Selectivity thresholds for filter plan switching (YFCC + synthetic correlated) | `06/filtering` |
| OQ-50 | Recall lost to segment fragmentation (probing `p` lists across 10 segments vs 1) | `06/incremental-maintenance` |
| OQ-54 | Does `foyer` support per-class quotas, pinning, zero-copy handoff? | `07/cache-hierarchy` |
| OQ-64 | Ranking error from per-shard IDF vs. global DF maintenance | `08/hybrid-and-ranking` |
| OQ-66 | Rust SWIM implementation with Lifeguard — `chitchat` (Quickwit) vs `foca` | `09/crate-survey` |
| OQ-72 | Realistic multi-tenant index-size/query-rate distribution for synthetic benchmarks | `10/evaluation-methodology` |
| OQ-86 | Optimal bundle size `B` — cuts PUTs but raises single-index read amplification and `durable` ack latency | `05/batching-and-visibility` |
| OQ-87 | Is per-byte-range encryption sufficient isolation for cross-tenant bundles, or does compliance force per-tenant objects? **Ask customers before building.** | `05/batching-and-visibility` |
| OQ-90 | Does R-way in-memory memtable replication raise intra-AZ network cost or tail latency at 10K nodes? | `05/batching-and-visibility` |

## Tier 3 — tuning and refinement

`OQ-1` S3 409 retry cost under a 10K-node herd ·
`OQ-4` Express One Zone as WAL tier crossover ·
`OQ-7` manifest delta-chain cap ·
`OQ-8` catalog `num_buckets` and split threshold ·
`OQ-9` catalog scope (per-bucket vs global) ·
`OQ-11` `read_token` vector vs scalar ·
`OQ-13` flat vs zone-sharded gossip crossover ·
`OQ-14` LRH `C` and CHBL `ε` under heavy-tailed index sizes ·
`OQ-15` placement key granularity ·
`OQ-17` backup-timer `d` per work type ·
`OQ-18` duplicate-work waste during large scale-out ·
`OQ-19` max shard fan-out before two-level aggregation ·
`OQ-20` dynamic replication factor oscillation/damping ·
`OQ-21` cache quota policy ·
`OQ-23` lane bitmap sizing under writer churn ·
`OQ-25` cross-lane tie-break vs client-supplied versions ·
`OQ-27` footer suffix size ·
`OQ-28` index section as a separate object for huge segments ·
`OQ-29` LSM level count and fan-out ·
`OQ-30` decoupling re-clustering from data compaction ·
`OQ-31` compaction fairness under heavy-tailed index sizes ·
`OQ-32` roaring vs ribbon for delete vectors ·
`OQ-33` branch refcounting in deep branch trees ·
`OQ-34` epochs public or internal ·
`OQ-36` exact-scan threshold ·
`OQ-37` centroid hierarchy above 1M centroids ·
`OQ-38` boundary augmentation factor by dimension ·
`OQ-40` oversample factor per rerank rung ·
`OQ-41` 1-bit quantization at 3072d / Matryoshka ·
`OQ-42` opt-in PQ for customers who benchmark it better ·
`OQ-44` hoisting block-max metadata out of Tantivy's format ·
`OQ-45` impact-ordered vs doc-ordered postings ·
`OQ-47` when to materialize filter bitmaps ·
`OQ-48` partitioned indexes vs many indexes ·
`OQ-49` `p_effective` explosion on anti-correlated filters ·
`OQ-52` drift thresholds for re-clustering ·
`OQ-53` centroid hierarchy crossover ·
`OQ-55` RAM:NVMe ratio and instance selection ·
`OQ-56` class-aware admission vs S3-FIFO/W-TinyLFU ·
`OQ-57` shadow-warming cost/benefit ·
`OQ-58` cache retention after placement loss ·
`OQ-59` balance cost of AZ-aware LRH ·
`OQ-60` speculative RT-A fetch waste ·
`OQ-61` cache-aware cost model design ·
`OQ-62` two-level fan-out shape ·
`OQ-63` RRF `k` default (60 vs 10) ·
`OQ-65` multi-vector storage layout ·
`OQ-67` manifest serialization format ·
`OQ-68` DataFusion for aggregations ·
`OQ-69` io_uring for the NVMe cache tier ·
`OQ-70` core split between Tokio and the scan pool ·
`OQ-71` `simsimd` dispatch overhead ·
`OQ-73` affordable ground truth for billion-scale filtered recall ·
`OQ-74` publishable reproducible cost-per-query benchmark ·
`OQ-76` cache-size vs blob-cost crossover ·
`OQ-77` cross-AZ transfer cost under fan-out ·
`OQ-78` prefix entropy width ·
`OQ-79` self-describing index ids ·
`OQ-80` per-shard HEAD threshold ·
`OQ-81` Arrow Flight for bulk ingest ·
`OQ-82` public epochs / time travel ·
`OQ-83` streaming query responses ·
`OQ-88` bundle record ordering: sorted by `(index_id, shard)` vs clustered by read affinity ·
`OQ-89` shared L0 segments across indexes — worth the GC/branching complexity, or does inlining small indexes already capture it?

## Strategic risks (not answerable by experiment)

| Risk | Assessment |
|---|---|
| **S3 Vectors commoditizes the category** | Real. AWS positions it as "complementary," and it sets a $0.06/GB price floor with a ~100 ms warm ceiling. Our answer is hybrid search + rich filtering + 10× better warm latency + BYOC + unbounded index count — *not* competing on $/GB. |
| **turbopuffer is well ahead** | They have production scale (1T docs, 25k QPS, 99.99% since launch) and marquee customers. Our differentiation must be architectural (multi-writer lanes, true masterlessness, 10K nodes), not incremental. |
| **The masterless premise is unproven at scale** | Nobody has shipped a fully blob-resident metadata plane at fleet scale. That is simultaneously the risk and the reason to build it. M0 and M2 exist to answer this early and cheaply. |
| **"No master" may be solving a problem customers don't have** | Worth stating plainly: a small hosted metadata service (WarpStream's model) works fine for most people. Our justification is operational simplicity, BYOC data sovereignty, and one-stateful-dependency uptime — which are *product* arguments, and should be validated with customers, not just engineers. |
