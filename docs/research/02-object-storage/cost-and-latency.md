# Blob Store Cost and Latency — the physics of the design

**Answers:** Q5
**Status:** Complete (v1)
**Retrieved:** 2026-09-05 — *prices rot; re-verify before quoting externally.*

## 1. Request pricing (us-east-1 / equivalent)

| Store / class | Write-class op | Read-class op | Ratio W:R | Storage $/GB-mo |
|---|---|---|---|---|
| **S3 Standard** | $0.005 / 1,000 (PUT, COPY, POST, **LIST**) | $0.0004 / 1,000 (GET, HEAD, SELECT) | **12.5×** | ~$0.023 |
| **S3 Express One Zone** | $0.00113 / 1,000 PUT | $0.00003 / 1,000 GET | **37.7×** | ~$0.11 |
| S3 Standard-IA | $0.01 / 1,000 | $0.001 / 1,000 | 10× | ~$0.0125 (+retrieval $/GB) |
| **GCS Standard** | Class A ~$0.005 / 1,000 (regional; multi-region higher, and increased to ~$0.10/10k) | Class B ~$0.0004 / 1,000 | ~12.5× | ~$0.020 |
| **Azure Blob Hot** | Write ops per 10,000 (higher than read) | Read ops per 10,000 | ~10× | ~$0.018 |
| S3 DELETE | **free** | — | — | — |
| GCS / Azure DELETE | billed (Class A / write op) | — | — | — |

**The three numbers that drive every design decision:**

1. **A PUT costs 12.5 GETs.** (37.7 on Express.) Writes must be batched; reads may be
   profligate.
2. **LIST is priced as a PUT and returns ≤1000 keys.** Listing is the single worst
   price/information ratio in the API. This is the arithmetic behind Design rule 4.
3. **DELETE is free on S3, billed on GCS/Azure.** Our GC policy must be capability-aware:
   aggressive tombstone reaping on S3, batched-and-lazy on GCS/Azure.

### Worked example: why write batching is existential

Ingesting 1 billion small documents.

| Strategy | PUTs | Cost |
|---|---|---|
| One object per document | 1×10⁹ | **$5,000,000** |
| Batch 10,000 docs/object | 1×10⁵ | **$500** |
| Batch 1,000,000 docs/object | 1×10³ | **$5** |

The same ingest spans six orders of magnitude in cost depending purely on batch size. There
is no clever indexing that recovers a bad batching decision.

### Worked example: why reads can be lavish

A query that issues 10 ranged GETs, 1,000 QPS, all month:

`10 × 1000 × 2.6e6 s/mo = 2.6e10 GETs → 2.6e10 / 1000 × $0.0004 = $10,400/mo`

That is real money, but it is the *uncached* case. With a 95% cache hit rate it drops to
~$520/mo. Contrast: the same query pattern implemented as PUT-class operations would be
$130,000/mo. **Reads are affordable; writes are not.** Also note egress within the same
region to EC2 is free on S3, so the byte volume is nearly free — only the request count
matters.

## 2. Latency

| Path | Typical first-byte | Notes |
|---|---|---|
| S3 Standard GET (small object) | **~15–60 ms p50, 100–200 ms p99** | Cited ranges of "50–150 ms+" are common for cold/large. |
| S3 Standard PUT | ~20–80 ms p50, higher tail | Durability across AZs. |
| S3 Express One Zone GET/PUT | **single-digit ms** first-byte, ~10× faster than Standard | Single-AZ, no cross-AZ durability. |
| GCS / Azure | broadly comparable to S3 Standard | Same order of magnitude. |
| Local NVMe read | ~50–150 µs | ~300–1000× faster than S3. |
| RAM | ~100 ns | ~10⁵× faster than S3. |

**Consequences:**

- **The round trip is the unit of cost, not the byte.** At ~30 ms per hop and a 100 ms
  cold-query budget, we get **~3 sequential round trips**. Turbopuffer independently landed
  on exactly this: vector search capped at three round trips. Any algorithm requiring a
  data-dependent chain of more than ~3 fetches (i.e. graph traversal) is disqualified from
  the cold path. This single fact selects the whole indexing strategy — see
  `06-indexing/vector-index-survey.md`.
- **Throughput is elastic, latency is not.** S3 gives essentially unbounded parallel
  bandwidth. Wide-and-shallow beats narrow-and-deep, always. A fan-out of 200 concurrent
  ranged GETs costs the same wall-clock as 1.
- **The cache tiers exist to convert round trips into microseconds**, not to save money on
  requests (though they do that too).

## 3. The storage cost stack

Turbopuffer's published comparison, per TB per month — this is the economic case for the
whole architecture:

| Architecture | $/TB/mo |
|---|---|
| RAM + 3× replicated SSD | $3,600 |
| RAM cache + 3× SSD (typical incumbent vector DB) | $1,600 |
| 3× SSD only | $600 |
| **Object storage + SSD cache** | **$70** |
| Object storage only | $20 |

A **~20–50× reduction** versus memory-resident vector databases. The entire value
proposition is: *pay object-storage prices for capacity, pay cache prices only for the
working set.* This is why the design tolerates 30 ms round trips at all.

## 4. Rate limits as a design input

- S3: ≥3,500 write-class/s and ≥5,500 read-class/s **per partitioned prefix**, unlimited
  prefixes, adaptive repartitioning over minutes.
- 10,000 nodes each issuing 10 requests/s = 100,000 req/s aggregate. That needs ~20+
  distinct hot prefixes minimum, and far more for headroom.

> **Design rule 8.** Every request budget is stated per prefix-group, and key layout must
> guarantee that no single prefix-group carries more than a few thousand ops/sec. Entropy
> goes at the front of the key.

## 5. Derived budgets for `pstore`

These become hard targets carried into the design docs:

| Operation | Blob-request budget |
|---|---|
| Write batch commit (any size) | **1 PUT** (+1 CAS PUT amortized over many batches) |
| Warm point query | **0** (cache hit) |
| Cold vector query | **≤3 GETs sequential**, fan-out allowed within each round |
| Cold BM25 query | **≤3 GETs sequential** |
| Open an index (cold) | **≤2 GETs**, zero LISTs |
| Node join / start | **O(1) GETs**, zero LISTs |
| Compaction of N inputs → 1 output | N ranged GETs + 1 MPU + 1 CAS PUT |

## Open questions raised

- OQ-3: Measure real p50/p99/p999 TTFB per backend per object size from within each cloud;
  published numbers are marketing-grade.
- OQ-4: Is S3 Express One Zone the right WAL tier? Its 37.7× W:R ratio and $0.11/GB storage
  make it great for a short-lived, read-heavy, low-latency log. Model the crossover.

## Sources

- [S3 Pricing — AWS](https://aws.amazon.com/s3/pricing/)
- [Amazon S3 pricing: the complete 2026 guide — CloudZero](https://www.cloudzero.com/blog/s3-pricing/)
- [Announcing up to 85% price reductions for Amazon S3 Express One Zone — AWS](https://aws.amazon.com/blogs/aws/up-to-85-price-reductions-for-amazon-s3-express-one-zone/)
- [Unpacking Amazon S3 Express One Zone — Vantage](https://www.vantage.sh/blog/amazon-s3-express-one-zone)
- [Storage pricing — Google Cloud](https://cloud.google.com/storage/pricing)
- [Announcement of pricing changes for Cloud Storage — Google Cloud](https://cloud.google.com/storage/pricing-announce)
- [Azure Blob Storage Pricing: A 2026 Cost Breakdown — CloudZero](https://www.cloudzero.com/blog/azure-blob-storage-pricing/)
- [Best practices design patterns: optimizing Amazon S3 performance — AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html)
- [turbopuffer: fast search on object storage](https://turbopuffer.com/blog/turbopuffer)
- [The Cloud Storage Triad: Latency, Cost, Durability — Materialized View](https://materializedview.io/p/cloud-storage-triad-latency-cost-durability)
