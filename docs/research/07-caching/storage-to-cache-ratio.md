# Maximizing the S3-to-Cache Ratio

**Answers:** Q37 — *How do we keep far more on the blob store than in RAM/NVMe?*
**Status:** Complete (v1)

## 1. The metric

> **R = bytes of data managed on the blob store ÷ bytes resident in RAM + NVMe.**

R is the entire economic thesis expressed as one number. R = 1 is a memory-resident database
at $1,600–3,600/TB-month. R = 300 is a $70/TB-month product. Every design decision in
`pstore` should be checkable against "does this raise or lower R?"

It decomposes cleanly:

```
R  =  (bytes stored per vector)  ÷  (bytes cached per vector)  ×  1/f
      └──── representation ratio ────┘                            └ working-set fraction
```

## 2. The representation ratio: 31.6×

Per 768-dim vector with ~500 B of attributes and a ~500 B stored document:

| Tier | Bytes on blob store | Cached? |
|---|---|---|
| RaBitQ 1-bit codes (+ factors) | 104 | **yes — this is the scan tier** |
| SPANN boundary duplication | 104 | only the probed lists |
| int8 rerank tier | 768 | rarely |
| f16 exact-rerank tier | 1,536 | **never by default** |
| Attributes (~3× compressed) | 170 | filter columns only |
| FTS / sparse postings | 100 | hot terms only |
| Stored document fields | 500 | top-k only |
| **Total on blob store** | **3,282** | |
| **Resident per hot vector** | | **104** |
| | | **= 31.6×** |

Two things produce this, and both are already decisions:

- **Quantization does 32× of it.** RaBitQ 1-bit is the single largest lever in the entire
  system for R. ([`../06-indexing/quantization.md`](../06-indexing/quantization.md))
- **Tier separation does the rest.** Full-precision vectors live in their own segment section
  and are fetched only for exact rerank, never cached
  ([`cache-hierarchy.md`](cache-hierarchy.md) class 9).

### The counter-intuitive corollary

**Adding more tiers to the blob store *raises* R**, because it grows the numerator without
touching the denominator. Storing an f16 exact-rerank tier costs $0.023/GB-month and *improves*
the ratio. So:

> **D-67. Store generously, cache stingily.** Blob storage is the cheapest resource we have;
> precomputing and storing an extra representation is nearly always right, and caching one
> nearly always wrong. This is the ratio-maximizing strategy stated as a rule.

## 3. The working-set fraction, and why the index choice sets it

`f` is the fraction of a dataset's vectors whose posting lists are actually touched. It is
determined mostly by the *index structure*:

| f | R | Meaning |
|---|---|---|
| 1.0 | 32:1 | Every byte is hot (worst case) |
| 0.3 | 105:1 | |
| **0.1** | **316:1** | Typical for skewed search workloads |
| 0.03 | 1,052:1 | |
| 0.01 | 3,156:1 | Highly skewed, or mostly-idle tenants |

A clustered index probes `p` posting lists out of `C` centroids, so a *single* query touches
`p/C` of the data — for 1M centroids and p=32, 0.003%. Across many diverse queries the union
saturates, but it saturates *low* because clusters are semantically coherent: similar queries
hit the same clusters.

> **Finding R-1.** A graph index (HNSW/DiskANN) has **much worse cache-hit density**, because
> traversal touches scattered nodes across the whole graph — there is no locality to exploit.
> Clustered indexes concentrate access into contiguous posting lists that are exactly the unit
> we cache and fetch.

We chose SPANN for round trips ([`../06-indexing/vector-index-survey.md`](../06-indexing/vector-index-survey.md))
and for filtered search ([`../06-indexing/filtering.md`](../06-indexing/filtering.md)).
**This is the third independent argument for the same decision**, and arguably the most
commercially important: it is what makes R large.

## 4. Capacity is never the binding constraint

For a 1 PB dataset:

| | R = 32 | R = 100 | R = 316 |
|---|---|---|---|
| Nodes needed for **capacity** (2.4 TB cache each) | 13 | 4 | **1.3** |

Versus throughput:

| Target | 200 QPS/node | 1,000 QPS/node |
|---|---|---|
| 10,000 QPS | 50 nodes | 10 nodes |
| 100,000 QPS | 500 nodes | 100 nodes |

> **Finding R-2.** At any plausible R, **QPS binds long before capacity does** — by one to two
> orders of magnitude. Nodes exist to serve queries, not to hold data.

This is the quantitative confirmation of the cost model's conclusion that compute is 92% of
cost. It also reframes the goal: **R does not need to be maximized in the abstract; it needs to
be high enough that we never buy a node for capacity.** Once R ≳ 50 that condition holds for
essentially any dataset, and further gains in R show up as *hit rate*, which is a latency and
blob-cost win rather than a node-count win.

## 5. The seven levers, ranked

| # | Lever | Effect on R | Status |
|---|---|---|---|
| 1 | **1-bit quantization for the scan tier** | **32×** | Decided (D-11) |
| 2 | **Never cache full-precision vectors** | ~15× on the denominator | Decided (class 9) |
| 3 | **Clustered index ⇒ low f, high locality** | 3–30× via f | Decided (D-8), and R-1 is a third reason |
| 4 | **Class-priority admission** — bulk can never evict metadata | protects the small hot set | Decided (D-21) |
| 5 | **Scan bypass** — compaction/export don't populate the cache | prevents f→1 | Decided (D-50) |
| 6 | **Answer aggregations from cached stats** | 0 data bytes for many queries | Decided (query-path) |
| 7 | **Store extra tiers on the blob store** | raises the numerator, free | **D-67, new** |

Two additional levers specific to our tenancy shape:

8. **Tenant co-location for small tenants** (D-43): a tenant's ~50 indexes share one node's
   cache, so their working set is one coherent unit rather than 50 fragments each carrying
   their own metadata overhead. At 50M indexes this materially reduces the fixed per-index
   cache cost.
9. **Idle tenants cost zero cache.** With 90% of tenants idle at any instant, the fleet-level R
   is roughly 10× the per-active-index R. This is the multi-tenancy dividend and it is large.

## 6. What lowers R (the anti-patterns)

| Anti-pattern | Damage |
|---|---|
| Caching f16/f32 vectors "for rerank speed" | Denominator ×15 ⇒ R ÷15 |
| Graph index over the full dataset | Destroys locality; f → 1 |
| Skipping quantization "for accuracy" | R ÷32 — and RaBitQ has a theoretical error bound, so the accuracy argument is weak |
| Letting scans populate the cache | f → 1 |
| Caching stored document fields eagerly | Denominator ×5 for data used only on the final top-k |
| Per-index metadata overhead at 50M indexes | A fixed per-index cache cost times 50M is its own working set |

## 7. Measuring it

R must be a **first-class product metric**, reported per node, per tenant, and fleet-wide:

- `managed_bytes / resident_bytes` (fleet R)
- Per-tenant R — a tenant with low R is either genuinely hot or misconfigured (e.g. no
  quantization, or a pathological query pattern), and both are worth surfacing
- Working-set fraction `f` per index, from posting-list access histograms
- **Bytes cached per byte scanned** — the efficiency of the cache as a scan accelerator, which
  is what actually matters given Finding R-2

> **D-68.** Report R in the same dashboard as cost. A regression in R is a regression in unit
> economics and should be treated like a latency regression: alarmed and bisected.

## 8. Open questions raised

- **OQ-117 (Tier 2)** — Measured `f` on real query traces. Everything in §3 rests on
  f ≈ 0.1, which is an estimate from search-workload folklore, not our data.
- OQ-118 — Does caching the int8 rerank tier for the hottest tenants pay for itself? It costs
  8× the denominator but removes a round trip on rung 1.
- OQ-119 — Fixed per-index cache overhead at 50M indexes: what is the floor, and does it
  dominate for tiny tenants?
- OQ-120 — Is there an R below which we should *refuse* a workload (or price it differently)?
  A customer whose access pattern is uniformly random over 100 TB is a memory-resident database
  customer, not ours, and the honest answer may be to say so.

## Sources

- [turbopuffer: fast search on object storage ($/TB/month by architecture)](https://turbopuffer.com/blog/turbopuffer)
- [RaBitQ: Quantizing High-Dimensional Vectors with a Theoretical Error Bound — arXiv](https://arxiv.org/abs/2405.12497)
- [SPANN: Highly-efficient Billion-scale ANN Search — arXiv](https://arxiv.org/pdf/2111.08566)
- [Quickwit 101 — hotcache is <0.1% of split size](https://quickwit.io/blog/quickwit-101)
- [Amazon S3 Vectors GA — AWS](https://aws.amazon.com/about-aws/whats-new/2025/12/amazon-s3-vectors-generally-available/)
- Arithmetic reproducible in `09-rust-stack/cpu-budget.py` (sections A–D).
