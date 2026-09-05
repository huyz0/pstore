# Vector Index Selection: Round Trips Decide Everything

**Answers:** Q19
**Status:** Complete (v1)

## The one-line argument

> Object storage costs ~30 ms per round trip. Our cold-query budget is ~3 sequential round
> trips. **Graph indexes require a data-dependent chain of hops; clustered indexes require a
> fixed 2.** Therefore `pstore` uses a clustered index. Everything else is detail.

## The candidates

### HNSW — disqualified for cold storage
Hierarchical navigable small world graphs. Excellent in RAM (Weaviate: 2.8 ms mean, 4.4 ms
p99), the industry default.

- Search is a **greedy traversal**: each hop's next reads depend on the current hop's results.
  There is no way to batch them. Typical searches touch hundreds of nodes over tens of
  sequential steps.
- On object storage: tens of steps × 30 ms = **seconds**. Fatal.
- Update cost is high (graph mutation), which is also bad for an LSM where segments are
  built constantly.
- **Filtered search degrades badly**: filtering creates "islands and dead ends", terminating
  traversal early and collapsing recall exactly when the filter is most selective.

Verdict: **RAM-tier only.** We may use HNSW *over centroids* (a tiny in-memory structure),
which is precisely what SPANN does — but never over the full dataset on cold storage.

### DiskANN / Vamana — disqualified, but instructively
Designed for SSD. A single flat graph with a long-range-edge construction, PQ codes in RAM to
guide traversal, full vectors on SSD.

- **Beam search**: at each step, use in-memory PQ to pick the best `W` candidates and issue
  all `W` SSD reads **in parallel**. So each *step* is one round trip of width `W`, and a
  query takes several steps.
- On NVMe (~100 µs), 8 steps × 100 µs = 0.8 ms. Fine.
- On S3 (~30 ms), 8 steps = **240 ms of pure sequential latency**, before any compute — and
  that is the *good* case. Still 3–8× our budget.
- Beam search is genuinely clever and *is* the right structure for our **NVMe cache tier**.

Verdict: **great on the warm tier, unusable on the cold tier.** Note this is a real option
for a hybrid design — see "Should we run two indexes?" below.

### IVF / SPANN — selected
Inverted-file / clustered. Centroids in memory; posting lists (clusters) on disk.

**SPANN** (NeurIPS 2021, Microsoft) is the mature form:
- **Hierarchical balanced clustering** at build time to equalize posting-list lengths — no
  pathological long lists.
- **Boundary/closure augmentation**: vectors near a cluster boundary are added to *multiple*
  posting lists, so a query that picks the "wrong" cluster still finds them. This is what
  recovers the recall a naive IVF loses.
- **Query-aware dynamic pruning**: decide at query time which posting lists are worth
  fetching, based on centroid distances — fewer I/Os for easy queries.
- Results: **90% recall@1 and recall@10 in ~1 ms with 32 GB memory at billion scale**,
  **2× faster than DiskANN at equal recall and memory** on three billion-scale datasets.

**The round-trip structure is the whole point:**
```
RT 1:  fetch centroid table          (small, cached ~always → often 0 RTs)
       [pure compute: score centroids, pick top-p posting lists]
RT 2:  fetch p posting lists          (parallel — one round trip regardless of p)
       [pure compute: scan, score]
RT 3:  fetch full vectors / documents for the top-k   (parallel)
```
**Two to three round trips, fixed, independent of dataset size.** Compare DiskANN's data-
dependent chain. This is exactly what turbopuffer means by choosing an index that "minimizes
roundtrips and write-amplification compared to graph-based indexes like HNSW or DiskANN."

**Known SPANN weaknesses, and our answers:**

| Weakness | Evidence | Our mitigation |
|---|---|---|
| Index size 1.5×–3.4× larger than DiskANN (boundary vectors duplicated across lists) | SPANN paper comparisons | Storage is $0.023/GB. **This is the cheapest currency we spend.** Accept it. |
| Degrades faster than DiskANN on **low-dimensional** data at high recall — needs coarser posting lists ⇒ more I/O | SPANN vs DiskANN analysis | Our workload is embeddings (768–3072 dims), where SPANN is strongest. Document low-dim as a weak spot; fall back to exact scan for small/low-dim indexes. |
| Higher SSD bandwidth, lower IOPS (coarse granularity) | Same | **This is a feature on object storage**, where bandwidth is free and requests are the cost. The property that hurts SPANN on SSD helps it on S3. |

### SPFresh / LIRE — selected for maintenance
SOSP 2023. Adds **incremental in-place updates** to SPANN via the **LIRE** protocol:
- Insert/delete append to the nearest posting list; deletes are marked, GC deferred.
- **Split** a posting list over `max_posting_len`; **merge** under `min_posting_len`.
- After a structural change, **only boundary vectors** that violate the nearest-partition
  assignment rule are re-checked and reassigned — provably a small set in a good index, so no
  full scan.
- A version map tracks stale entries for query-time filtering.
- Result: **better query latency and accuracy than global rebuild, using 1% of the DRAM and
  <10% of the cores at peak**, at billion scale with a 1%/day update rate.

This is what makes a *live* clustered index viable, and it is why turbopuffer names SPFresh
specifically. Details in `06-indexing/incremental-maintenance.md`.

## Decision

> **D-8.** The cold-tier index is **SPANN-family: hierarchical balanced clustering, boundary
> augmentation, query-aware pruning, maintained incrementally by a LIRE-style protocol.**
>
> **D-9.** The centroid table is the **hottest object in the system** and gets its own cache
> class with the highest admission priority (turbopuffer prioritizes centroid cache fills for
> the same reason).
>
> **D-10.** Small indexes (below ~50k–200k vectors, or ~100 MB) use **exact brute-force scan**
> — no ANN index at all. It is faster, exactly accurate, needs no maintenance, and covers the
> long tail of a millions-of-indexes product. **Most indexes will be in this regime.** This
> deserves to be a headline behaviour, not a fallback.

## Should we run two indexes (SPANN cold + DiskANN warm)?

Tempting: DiskANN is genuinely better on NVMe, which is where hot queries are served.
**Recommendation: no, not in v1.** Reasons: two indexes double build cost, double maintenance
complexity, and double the surface for recall bugs; and the warm path's bottleneck is compute,
not I/O, once the data is local. Revisit only if warm-tier benchmarks show SPANN losing to
DiskANN by a wide margin on the same cached bytes. Recorded as OQ-35.

## Sizing the parameters

| Parameter | Starting point | Rationale |
|---|---|---|
| Posting list size | ~1,000–10,000 vectors | Must be ≥ one useful ranged GET (`G*` ≈ 64 KiB–4 MiB); 4,000 × 768d × 1 byte (RaBitQ) ≈ 3 MB. Good fit. |
| Centroids | ~√N to N/1000 | 1B vectors ⇒ ~1M centroids ⇒ centroid table ~1M × 768 × 1B ≈ 768 MB quantized. Needs its own HNSW/tree for selection. |
| Posting lists probed (`p`) | 8–64, query-adaptive | Directly trades recall against bytes; one round trip regardless of `p`. |
| Boundary replication factor | 2–8 | Recall vs. index size. |

Note the pleasing consequence: **`p` is free in round trips.** We can probe 64 lists instead
of 8 for one extra round trip's worth of *bytes* and zero extra *latency hops*. This is a
lever that RAM-based systems don't have and it partially compensates for cold latency.

## Cost summary

| Query | RA |
|---|---|
| Cold, centroids not cached | 1 Rseq (centroids) + 1 Rpar (p lists) + 1 Rpar (docs) = **3 Rseq depth** |
| Cold, centroids cached | **2 Rseq depth** |
| Warm | 0 |
| Small index (exact) | 1 Rpar (the whole thing) = **1 Rseq depth** |

## Open questions raised

- OQ-35: Two-index (SPANN cold / DiskANN warm) — measure before rejecting permanently.
- OQ-36: Exact-scan threshold as a function of dimension, cache state, and QPS.
- OQ-37: Multi-level centroid hierarchy for >1M centroids — SPANN uses a memory index over
  centroids; at our scale that index may itself need to be on disk.
- OQ-38: Recall impact of boundary augmentation factor at 768d vs 3072d.

## Sources

- [SPANN: Highly-efficient Billion-scale Approximate Nearest Neighbor Search — arXiv](https://arxiv.org/pdf/2111.08566)
- [SPANN — Microsoft Research](https://www.microsoft.com/en-us/research/publication/spann-highly-efficient-billion-scale-approximate-nearest-neighbor-search/)
- [SPFresh: Incremental In-Place Update for Billion-Scale Vector Search — SOSP 2023 (ACM DL)](https://dl.acm.org/doi/10.1145/3600006.3613166)
- [SPFresh notes — Hrushikesh Dokala](https://hrushikesh.dev/notes/spfresh/)
- [What Is DiskANN? Billion-Scale Vector Search Explained — Couchbase](https://www.couchbase.com/blog/diskann/)
- [I/O Optimizations for Graph-Based Disk-Resident ANN Search: A Design Space Exploration — arXiv](https://arxiv.org/pdf/2602.21514)
- [Vector Search for the Future: From Memory-Resident, Static Heterogeneous Storage, to Cloud-Native Architectures — arXiv](https://arxiv.org/pdf/2601.01937)
- [turbopuffer — Architecture (SPFresh rationale)](https://turbopuffer.com/docs/architecture)
- [Filtered ANN Search in Vector Databases — arXiv](https://arxiv.org/html/2602.11443)
