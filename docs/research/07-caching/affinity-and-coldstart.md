# Cache Affinity and Cold Start at 10,000 Nodes

**Answers:** Q25
**Status:** Complete (v1)

## The tension

- **Affinity** wants requests for an index to land on the same few nodes, so their caches
  stay warm.
- **Elasticity** wants any node to serve anything, so scaling needs no data movement.
- **Balance** wants load spread evenly.

Resolved by: **placement is a hint (LRH + CHBL), never a requirement.** See
`04-cluster/routing-and-placement.md`.

## Cold-start cost, measured

turbopuffer's published figures for 1M documents:

| | Cold | Warm |
|---|---|---|
| Vector (768d, 3 GB) | p50 874 ms, p90 444 ms | p50 14 ms, p90 10 ms |
| BM25 (300 MB) | p90 285 ms | p90 18 ms |

**A cold query is ~30–60× a warm query.** The whole operational game is minimizing the
*number* of cold queries, not the cost of each one.

## Cold-start taxonomy and mitigations

| Cause | Frequency | Mitigation |
|---|---|---|
| **First-ever query on an index** | Once per index | Unavoidable. Optimize the path: prefetch centroids + index section in parallel with planning. |
| **Genuinely cold tenant** (queried hourly) | Common in the long tail | Unavoidable and *correct* — this tenant costs almost nothing. Set expectations in the SLA: cold ≈ 400 ms. |
| **Placement change (scale-out, deploy, node death)** | Frequent | **Shadow warming**: on a placement change, the new owner pre-fetches classes 2–3 (index section, centroids) — a few MB — while the old owner still serves. Cheap and high-value. |
| **Eviction under memory pressure** | Tunable | Class-aware quotas (`cache-hierarchy.md`) prevent bulk traffic evicting metadata. |
| **Deploy / restart** | Every release | **Cache survives restart** — NVMe cache is persistent, keyed by immutable segment id, so a process restart is not a cache flush. `foyer`'s disk tier gives this. **This is a large, easily-missed win.** |
| **Stampede** | Bursty | Singleflight + admission control (`04-cluster/load-and-hotspots.md`). |

> **D-23.** The NVMe cache must be **durable across process restarts** and validated by
> segment id (immutable ⇒ always valid). **This is mandatory, not an optimization**: the
> endurance budget puts a full cache refill at ~10 hours per node, so a deploy that flushes
> caches costs 10 hours of degraded hit rate fleet-wide
> ([`disk-space-management.md`](disk-space-management.md) §2). A rolling deploy of 10,000 nodes must not produce
> 10,000 cold caches. Design the on-disk cache format for fast reopen (index the cache
> directory in a small persistent file; do not scan it).

## The two-hop question

Routing forwards a request from the receiving node to a placement node (~0.2 ms intra-AZ).
Worth it when:
```
P(warm at placement) × (cold_latency − warm_latency) > hop_cost
0.9 × (400 ms − 14 ms) = 347 ms  ≫  0.2 ms
```
Overwhelmingly worth it. The hop is essentially always correct; skip it only when the
receiving node is itself a placement.

**Cross-AZ caution:** an inter-AZ hop costs both latency (~1 ms) and **data transfer charges**
($0.01/GB each way on AWS). Since we forward the *request*, not the data, and return results
(small), this is acceptable — but a fan-out query returning large results cross-AZ is not.

> **D-24.** Prefer same-AZ placements when available; make AZ a first-class input to LRH
> scoring. Cross-AZ is a fallback, not a default. This also improves tail latency.

## Warm-cache API

Let clients declare intent:
```
POST /v1/indexes/{id}/warm   { shards: [...], classes: ["metadata","centroids","vectors"] }
```
turbopuffer offers this and it costs us nothing to provide. Use cases: a nightly batch job, a
demo, a known traffic ramp, a blue/green deploy.

Charge for it honestly (it consumes blob requests and cache space) and it becomes a feature
rather than an abuse vector.

## Measuring what matters

Metrics that must exist from day one:
- `cold_query_ratio` per index and fleet-wide — **the single most important product metric**.
- Cache hit rate **per class** (a 99% overall hit rate hides a 40% centroid miss rate).
- Bytes fetched per query, blob requests per query, per index.
- Placement churn rate and post-churn cold ratio.
- Time-to-warm after a placement change.

## Open questions raised

- ~~OQ-57~~ **ANSWERED** — see [`disk-space-management.md`](disk-space-management.md) §2.
  Yes for metadata, never for bulk. Cache fill is capped by device endurance at ~67 MB/s, so
  filling a 2.4 TB cache takes ~10 hours; but classes 1–4 are 0.1–1% of bytes (2.4–24 GB =
  36 s to 6 min) and unblock every query. **D-44:** warm classes 1–4 only.
- OQ-58: How long does the NVMe cache remain useful after a placement change (i.e. should a
  node retain an index's cache after losing placement, in case it comes back)? Leaning yes,
  with decay.
- OQ-59: AZ-aware LRH — how much balance do we lose by constraining placement to an AZ?

## Sources

- [turbopuffer: fast search on object storage (cold/warm latency figures)](https://turbopuffer.com/blog/turbopuffer)
- [turbopuffer — Architecture (cache locality routing, warm cache API)](https://turbopuffer.com/docs/architecture)
- [foyer — Hybrid cache for Rust](https://foyer.rs/docs/overview)
- [Local Rendezvous Hashing — arXiv](https://arxiv.org/pdf/2512.23434)
