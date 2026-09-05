# Unit Economics

**Answers:** Q31
**Status:** Complete (v1)
**Prices retrieved:** 2026-09-05 (us-east-1). Re-verify before external use.

## Inputs

| Item | Price |
|---|---|
| S3 Standard storage | $0.023 / GB-month |
| S3 PUT / COPY / POST / **LIST** | $0.005 / 1,000 |
| S3 GET / HEAD | $0.0004 / 1,000 |
| S3 DELETE | free |
| S3 Express One Zone PUT | $0.00113 / 1,000 |
| S3 Express One Zone GET | $0.00003 / 1,000 |
| S3 Express One Zone storage | $0.11 / GB-month |
| Intra-region transfer to EC2 | free |
| Cross-AZ transfer | ~$0.01 / GB each way |
| Compute (i-family w/ NVMe), rough | ~$1–3 / hour per node |

## Reference workload

**100M documents, 768-dim embeddings + ~1 KB of attributes each, 1,000 QPS, 10M writes/day.**

### Storage

| Tier | Bytes/doc | Total | $/month |
|---|---|---|---|
| RaBitQ 1-bit codes (scan tier) | 96 B | 9.6 GB | $0.22 |
| int8 codes (rerank tier) | 768 B | 76.8 GB | $1.77 |
| float16 vectors (exact rerank) | 1,536 B | 153.6 GB | $3.53 |
| Attributes (columnar, ~3× compressed) | ~340 B | 34 GB | $0.78 |
| FTS index (~30% of text) | ~300 B | 30 GB | $0.69 |
| Centroids, index sections, overhead | — | ~15 GB | $0.35 |
| Boundary duplication (SPANN, ~2×on scan tier) | — | ~10 GB | $0.23 |
| **Total** | | **~330 GB** | **~$7.6 / month** |

**Storage is essentially free.** This is worth internalizing: at $0.023/GB, 100M documents
cost less per month than a single developer-hour. Every design tradeoff that spends storage
to save requests or latency is correct.

### Writes

10M docs/day, batched at 10,000 docs per WAL object:
- 1,000 WAL PUTs/day (×2 for dual-tier durability) = 2,000 PUTs/day
- Compaction: ~10 GB/day rewritten ⇒ ~160 multipart PUTs + ~1,300 GETs
- Structural commits: ~500/day = 1,500 ops

```
PUTs:  ~3,700/day  → 111k/month  → $0.55/month
GETs:  ~40k/day    → 1.2M/month  → $0.48/month
```

**~$1 / month for 300M document writes.** Compare: unbatched, one PUT per document would be
**$1,500/month**. The batching decision is worth 1,500×.

### Queries

1,000 QPS = 2.6B queries/month.

| Cache hit rate | Blob GETs/query | GETs/month | $/month |
|---|---|---|---|
| 99% | 0.15 | 390M | **$156** |
| 95% | 0.75 | 1.95B | **$780** |
| 80% | 3.0 | 7.8B | **$3,120** |
| 0% (pathological) | 15 | 39B | **$15,600** |

**Cache hit rate is the dominant cost driver on the read path**, and it is a compute/RAM
tradeoff: buying more cache is buying down blob requests. The crossover is worth computing per
deployment — this is a real knob, not a hand-wave.

### Compute

1,000 QPS at (say) 200 warm queries/sec/node ⇒ ~5–8 nodes with headroom.

> **⚠️ "200 QPS/node" is not a property of the node.** Warm scan is memory-bandwidth-bound, so
> capacity is **vectors scanned per second**, and QPS/node ranges from ~16 to ~1,221 depending
> purely on scan size — a 75× swing. See
> [`../09-rust-stack/cpu-management.md`](../09-rust-stack/cpu-management.md) §1. Quote node
> counts against a stated scan size or not at all.
`~6 × $2/hr × 730 = ~$8,760/month`.

### Totals

| Component | $/month | Share |
|---|---|---|
| Storage | $8 | 0.1% |
| Writes | $1 | 0.0% |
| Queries (95% hit) | $780 | 8% |
| Compute | $8,760 | **92%** |
| **Total** | **~$9,550** | |

## The tenancy term the single-index model hides

The table above is for **one** index. It omits the cost of *having* many indexes, which is
where the naive design dies: a per-index flush timer at interval `T` costs `2,592,000/T` PUTs
per index per month **whether the index writes one document or a billion**.

| Design | 1M indexes, 1 doc/min each (~16.7 MB/s total) |
|---|---|
| Per-index flush, `T` = 1 s | **$13.0M / month** |
| Per-index flush, `T` = 60 s | **$216,000 / month** |
| **Cross-index node bundles (8 MiB / 5 s)** | **$26 – $260 / month** |
| \+ adaptive per-index folds (hourly) | + ~$3,600 / month |

~1,000× on the WAL, with *better* visibility latency. After bundling, the **fold rate**
becomes the dominant per-index PUT cost, which is why it must be size-driven rather than
timed. Idle indexes cost zero.
→ [`../05-storage-engine/batching-and-visibility.md`](../05-storage-engine/batching-and-visibility.md)

## The three conclusions

1. **Compute dominates, not storage or requests.** Once batching and caching are right, the
   blob store is a rounding error and this is a *compute efficiency* business. Query CPU per
   query is the number to optimize, which retroactively justifies the SIMD/quantization
   emphasis and the `runtime-and-io.md` care about scan performance.
2. **Idle tenants are free.** 100M documents at rest cost $7.60/month with **zero** compute.
   A million small idle indexes cost storage only. This is the property no Tier-1 system can
   match and it should drive pricing: charge for storage + queries, not for provisioned
   capacity.
3. **Unbatched writes, per-*index* flush timers, or an uncached read path are 1,000×–2,000×
   cost regressions.** These are not optimizations; they are the difference between a business
   and a bankruptcy. All three must be enforced by tests, not by discipline.

## Comparison at 100M docs / 330 GB

| Architecture | $/TB/mo | Est. $/month for storage-equivalent |
|---|---|---|
| RAM + 3× SSD | $3,600 | ~$1,190 |
| RAM cache + 3× SSD | $1,600 | ~$530 |
| 3× SSD | $600 | ~$200 |
| **Object storage + SSD cache** | **$70** | **~$23** |
| Object storage only | $20 | ~$7 |

**S3 Vectors** (the price floor to beat): $0.06/GB-mo storage, $0.20/GB PUT, tiered query
pricing. For 330 GB that is ~$20/month storage — ~2.6× our raw S3 cost, but bundled with
their compute. Our differentiation is not price-per-GB; it is **hybrid search, filtering,
warm latency ~10× better, BYOC, and unbounded index count**.

## Pricing implications (informational)

A defensible model, given the cost structure:
- **Storage**: $/GB-month, marked up modestly over blob cost.
- **Queries**: $/1M, tiered by index size (the driver is CPU and cache pressure, both of which
  scale with index size — the same reason S3 Vectors tiers query price by index size).
- **Writes**: $/1M, cheap.
- **No provisioned capacity charge** — this is the whole point, and it is what a Tier-1
  competitor structurally cannot offer.

## Open questions raised

- OQ-75: Real warm-QPS-per-node number — the single largest input to the cost model, and
  currently a guess (200/s/node). Must be measured.
- OQ-76: Cache-size vs. blob-request-cost crossover as a function of working-set size.
- OQ-77: Cross-AZ transfer cost under fan-out — could become significant at high shard counts
  with large intermediate results. Model it.

## Sources

- [S3 Pricing — AWS](https://aws.amazon.com/s3/pricing/)
- [Amazon S3 pricing: the complete 2026 guide — CloudZero](https://www.cloudzero.com/blog/s3-pricing/)
- [Announcing up to 85% price reductions for Amazon S3 Express One Zone — AWS](https://aws.amazon.com/blogs/aws/up-to-85-price-reductions-for-amazon-s3-express-one-zone/)
- [turbopuffer: fast search on object storage ($/TB/month table)](https://turbopuffer.com/blog/turbopuffer)
- [Amazon S3 Vectors GA — AWS](https://aws.amazon.com/about-aws/whats-new/2025/12/amazon-s3-vectors-generally-available/)
- [AWS S3 Vectors Pricing Deep Dive — Murray Cole](https://murraycole.com/posts/aws-s3-vectors-pricing-deep-dive)
