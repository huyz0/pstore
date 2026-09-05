# Open Questions and Risks

**Answers:** D36
**Status:** Living document — every research doc appends here.

Each entry: what we don't know, why it matters, and how to find out. Sorted by risk.

## Tier 1 — could invalidate the architecture

| ID | Question | Impact | Experiment |
|---|---|---|---|
| **OQ-51** | Does LIRE (SPFresh) preserve partition quality when its split/merge/reassign decisions are **batched into immutable-segment rewrites** instead of applied in place? | If not, every compaction needs full re-clustering — a large cost-model change. **The biggest unvalidated assumption in the design.** | M3.5 spike: implement both, compare recall and partition-length distribution over a simulated 90-day update stream. |
| **OQ-5** | Real CAS throughput and loss-rate curves per backend under 2 / 8 / 64 / 512 concurrent contenders. | The "~5 writes/s" figure is borrowed, not measured. Everything about commit rate and register partitioning depends on it. | **Blocked on cloud accounts (M0b).** Meanwhile invert it (D-101): sweep the fault-injecting store 0.5–50 CAS/s and find the breaking point, so one later measurement says which regime we are in. |
| **OQ-22** | Is forward-probing the lane tail with `k` parallel GETs cheaper than a per-lane tail-pointer object updated every M writes? | Determines read-path cost of the whole lane design. | Model both, then measure with realistic write-rate distributions. |
| **OQ-75** | Real warm queries-per-second per node. Currently a guess (200/s). | The dominant term in the cost model (compute is 92% of cost). | Benchmark the scan path on target instance types. |
| ~~OQ-84~~ | ~~What fraction of indexes are trickle writers?~~ | **ANSWERED:** 1M tenants × up to 50 indexes (~50M), **10% of tenants active per second**. Confirms bundling is essential and forces two further structural changes. | → [`../10-benchmarks-cost/tenancy-scale-model.md`](../10-benchmarks-cost/tenancy-scale-model.md) |
| **OQ-92** | How many of a tenant's ~50 indexes does a typical write burst touch? | Sets active-indexes/s `A` between 100k and 5M — a 50× swing in what the write-cohort design is worth. **The most valuable remaining number.** | Instrument a pilot tenant, or ask design partners. |
| **OQ-93** | Fleet write throughput in bytes/s. | Sets `W* = bytes/s × T / B` directly; without it the cohort ring cannot be sized. | Estimate from expected docs/s × doc size; confirm in M1. |
| **OQ-111** | Measured **bytes-scanned per second** per node, from RAM and from NVMe separately (refines OQ-75). | QPS/node swings 75× with scan size, so the cost model's dominant input is currently meaningless without this. | Benchmark the SIMD scan loop on target instance types at several scan sizes. |
| **OQ-98** | Actual endurance (DWPD/TBW) of AWS/GCP/Azure instance-store NVMe — **none of them publish it**. | The whole cache-fill rate budget (~67 MB/s) rests on a 2-DWPD assumption. If it is 1 DWPD the budget halves and warming takes 20 h; drives failing at month 8 is the failure mode. | Track SMART `percentage_used` drift on a real fleet for 2–4 weeks; or ask the vendor. |
| **OQ-85** | The three-way optimum between memtable memory budget, fold rate, and recovery-scan window. | Fold rate is the dominant per-index PUT cost after bundling; memtable size bounds both query cost and how lazy folding can be. | Model analytically, then measure in M1/M2. |
| **OQ-91** | Prove that `HEAD.lane_watermarks` + forward probing of placement nodes' bundle lanes finds **every** un-folded record under placement change, fallback writes, and node death. | If recovery can miss records, cross-index bundling is unsafe and the whole cost argument collapses. | Deterministic simulation target for M2. |

## Tier 2 — significant design impact

| ID | Question | Where |
|---|---|---|
| OQ-2 | Break-even gap `G*` for range coalescing, per backend | `02/request-efficiency-patterns` |
| OQ-3 | Real p50/p99/p999 TTFB per backend per object size | `02/cost-and-latency` |
| OQ-6 | Does the ABA nonce fully close the hole under S3 multipart ETags? **Unblocked** — `pstore-fake-s3` can synthesize the `-N` and non-MD5 ETag forms, so this is testable without cloud access | `03/manifest-and-cas`, `09/blob-store-fakes` |
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
| ~~OQ-54~~ | **CLOSED favourably** — foyer has admission/reinsertion filters, S3-FIFO + LRU-with-priority-pool, restart recovery, reserved space, and device IOPS/throughput throttling (the endurance enforcement point). Ours to build: class→region mapping, per-tenant accounting, watermarks. | `07/disk-space-management` §7 |
| OQ-64 | Ranking error from per-shard IDF vs. global DF maintenance | `08/hybrid-and-ranking` |
| OQ-66 | Rust SWIM implementation with Lifeguard — `chitchat` (Quickwit) vs `foca` | `09/crate-survey` |
| OQ-72 | Realistic multi-tenant index-size/query-rate distribution for synthetic benchmarks | `10/evaluation-methodology` |
| OQ-86 | Optimal bundle size `B` — cuts PUTs but raises single-index read amplification and `durable` ack latency | `05/batching-and-visibility` |
| OQ-87 | Is per-byte-range encryption sufficient isolation for cross-tenant bundles, or does compliance force per-tenant objects? **Ask customers before building.** | `05/batching-and-visibility` |
| OQ-90 | Does R-way in-memory memtable replication raise intra-AZ network cost or tail latency at 10K nodes? | `05/batching-and-visibility` |
| OQ-94 | Size threshold for tenant-grouped vs index-grouped placement, and for inlining — probably one number; verify | `10/tenancy-scale-model` |
| OQ-95 | Does `W`-node write concentration collide with S3 per-prefix limits, especially on cohort-lane reads during recovery? | `10/tenancy-scale-model` |
| OQ-97 | Adaptive promotion of hot indexes out of the tenant HEAD — reversible? demotion hysteresis? | `10/tenancy-scale-model` |
| OQ-133 | Are three independent per-AZ caches better than one shared cache with cross-AZ reads? Arithmetic says yes overwhelmingly; verify the hot set is small enough at our largest tenant | `04/az-topology` |
| ~~OQ-136~~ | **CLOSED** — probe mesh + blob health bulletin + peer-relative outlier detection; drain via node self-eviction (data-plane only). Surfaced that per-AZ cells removed the very traffic that reveals gray failure. | `04/gray-failure` |
| OQ-150 | Which backend has the highest CAS fidelity for day-to-day dev? MinIO's missing `*` wildcard is a real obstacle; run the conformance suite and pick on evidence | `09/dev-and-test-environment` |
| OQ-153 | Is `fake-gcs-server` faithful to `ifGenerationMatch`? GCS generations are our cleanest CAS story on paper — if so, GCS may be the better primary dev target than S3 | `09/dev-and-test-environment` |
| OQ-144 | Measure the real scan roofline on target instance types. H-1's "7× memory-bound" assumes ~4 cycles/vector; if the real kernel is 12, kernel work becomes worthwhile again | `09/hot-loop-performance` |
| OQ-145 | Huge pages: real TLB win vs allocation-latency and fragmentation cost; explicit `madvise` or THP? | `09/hot-loop-performance` |
| OQ-148 | `simsimd` dispatch overhead at our batch sizes, and whether it handles the 2-vector batching 768 dims needs (refines OQ-71) | `09/hot-loop-performance` |
| OQ-138 | Threshold calibration for gray detection: stdevs, confirmations, dwell. Start suspect-only with no auto-drain; tighten with real data | `04/gray-failure` |
| OQ-142 | Interlock between our draining and AWS zonal autoshift — can both fire and effectively remove two AZs? | `04/gray-failure` |
| OQ-143 | Gray failure of the **blob store** from one AZ's network path — same detection, different response, needs its own signal | `04/gray-failure` |
| OQ-137 | Do GCP/Azure have equivalent cross-zone pricing and a free regional-storage path? BYOC parity depends on it | `04/az-topology` |
| OQ-132 | Do we need epoch notification at all once `session` is the default (the client carries the epoch)? Possibly only for background warming | `04/epoch-propagation` |
| OQ-117 | Measured working-set fraction `f` on real query traces — everything about R rests on f ≈ 0.1, which is folklore not data | `07/storage-to-cache-ratio` |
| OQ-116 | Adaptive concurrency + the blob-store congestion controller are two interacting limiters in one request path; can they oscillate? | `09/cpu-management` |
| OQ-112 | Foreground/background core split: what floor does compaction need to keep segment counts bounded? | `09/cpu-management` |
| OQ-127 | Should sparse postings share a term space with BM25, or use a parallel index? | `06/modalities-and-sequencing` |
| OQ-104 | Real per-query memory profile by plan type — 24 MB is derived, not measured, and it sets the pool size and admission model | `09/memory-management` |
| OQ-107 | jemalloc tuning (`dirty_decay_ms`, `muzzy_decay_ms`, `retain`) against measured RSS, given the documented gap between settings and behaviour | `09/memory-management` |
| OQ-99 | Optimal cache-max fraction of the device (64% is derived from CacheLib/DLWA data, not from our access-size distribution) | `07/disk-space-management` |
| OQ-100 | Does class→region segregation reach the ~1.03 DLWA the FDP paper reports, without FDP hardware? | `07/disk-space-management` |
| OQ-103 | Behaviour when the cache device fails outright mid-flight — must be identical to bypass mode, and the node must not die. Verify. | `07/disk-space-management` |

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
`OQ-56` class-aware admission vs S3-FIFO/W-TinyLFU (see also OQ-101) ·
~~`OQ-57`~~ **closed** — warm classes 1–4 only; bulk warming exceeds the endurance budget ·
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
`OQ-89` shared L0 segments across indexes — worth the GC/branching complexity, or does inlining small indexes already capture it? ·
`OQ-96` prefix entropy width at 1M tenants, 4 vs 5 chars (refines OQ-78) ·
`OQ-101` S3-FIFO vs W-TinyLFU vs our class-aware policy on real traces (refines OQ-56) ·
`OQ-102` dedicated cache partition for very large tenants, vs a quota ·
`OQ-105` plan-time memory estimates vs mid-flight re-reservation ·
`OQ-106` query-pool vs RAM-cache split (both buy latency, at different rates) ·
`OQ-108` should query intermediates ever spill to disk, or is narrowing fetch width strictly better? ·
`OQ-109` per-open-index resident state size, which drives the LRU bound ·
`OQ-110` does `memory.high` throttling ever hurt more than shedding would? ·
`OQ-113` does SMT help a bandwidth-bound scan, or should the pool be sized in physical cores? ·
`OQ-114` scan chunk size: cancellation granularity vs per-chunk overhead ·
`OQ-115` NUMA-aware buffers, or just prefer single-socket instances? ·
`OQ-118` does caching the int8 rerank tier for hot tenants pay for itself? ·
`OQ-119` fixed per-index cache overhead at 50M indexes ·
`OQ-120` is there an R below which we should decline or reprice a workload? ·
`OQ-121` tenant-level vs per-index epochs in the session token ·
`OQ-122` `max_wait_ms` when a node is behind the token: wait, forward, or serve stale? ·
`OQ-123` should routing hints be signed separately so a proxy can use them? ·
`OQ-124` cross-region sessions — reserve token space now ·
`OQ-125` does `session`-as-default surprise users expecting `strong`? ·
`OQ-126` impact payload encoding (u8/f16/varint) and its effect on R ·
`OQ-128` multi-vector storage layout: interleaved or separate section (refines OQ-65) ·
`OQ-129` does accepting sparse vectors as input match how customers actually work? ·
`OQ-130` coalescing interval for epoch hint → revalidation ·
`OQ-131` should a notification inline the manifest for small tenants (prefetch vs trusting peer state)? ·
`OQ-134` per-AZ vs global write cohorts — confirm the PUT floor doesn't rise ·
`OQ-135` does per-AZ cell structure generalize to per-region cells? ·
`OQ-139` dedicated vs rotating probers (rotating avoids a correlated blind spot) ·
`OQ-140` concrete load-normalization model for distinguishing gray failure from overload ·
`OQ-141` should the blob health bulletin be signed? ·
`OQ-146` German-string buffer GC interacting with arena-per-query allocation ·
`OQ-147` non-temporal loads: less cache pollution vs losing L2 reuse within a morsel ·
`OQ-149` `std::simd` (nightly) vs `multiversion` + `std::arch` on stable ·
`OQ-151` contribute a wildcard `If-None-Match` fix upstream to MinIO? ·
`OQ-152` a shared remote dev container later, to escape WSL2's benchmarking limits ·
`OQ-154` does `s3s` model conditional-request headers on PutObject, or do we handle them above the generated types? ·
`OQ-155` run `ceph/s3-tests` against our fake in CI, recording unimplemented ops as expected failures? ·
`OQ-156` can one fake serve GCS/Azure behind a translation layer, or are generations vs ETags too different? ·
`OQ-157` is 100% region coverage on `pstore-engine` realistic, or does the CAS-retry surface make the last few percent brittle? ·
`OQ-158` `cargo-mutants` runtime on a workspace this size; per-crate scheduling needed? ·
`OQ-159` where exactly is the generics/`dyn` boundary — monomorphizing `BlobStore` through the engine could hurt compile times on a capped box ·
`OQ-160` should `pstore-types` exist, or do newtypes belong with their owning layer? (junk-drawer risk) ·
`OQ-161` is a ~4,800-token layer 0+1 the right standing charge? `AGENTS.md` duplicates `INDEX.md`'s conclusions ·
`OQ-162` the review packet runs the gates itself; cache verdicts by tree-sha if a gate gets slow ·
`OQ-163` a long autonomous run needs a token budget, not just a per-task round budget ·
`OQ-164` `build-index.py` counts meta-files as documents — honest, but is it the number a reader wants?

## Strategic risks (not answerable by experiment)

| Risk | Assessment |
|---|---|
| **S3 Vectors commoditizes the category** | Real. AWS positions it as "complementary," and it sets a $0.06/GB price floor with a ~100 ms warm ceiling. Our answer is hybrid search + rich filtering + 10× better warm latency + BYOC + unbounded index count — *not* competing on $/GB. |
| **turbopuffer is well ahead** | They have production scale (1T docs, 25k QPS, 99.99% since launch) and marquee customers. Our differentiation must be architectural (multi-writer lanes, true masterlessness, 10K nodes), not incremental. |
| **The masterless premise is unproven at scale** | Nobody has shipped a fully blob-resident metadata plane at fleet scale. That is simultaneously the risk and the reason to build it. M0 and M2 exist to answer this early and cheaply. |
| **"No master" may be solving a problem customers don't have** | Worth stating plainly: a small hosted metadata service (WarpStream's model) works fine for most people. Our justification is operational simplicity, BYOC data sovereignty, and one-stateful-dependency uptime — which are *product* arguments, and should be validated with customers, not just engineers. |
