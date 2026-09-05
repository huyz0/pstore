# Bundling Writes Without Paying Latency for It

**Answers:** Q32 — *How do we bundle more per PUT without delaying search-after-write?*
**Status:** Complete (v1)
**Supersedes parts of:** [`write-path-and-wal.md`](write-path-and-wal.md) §4–5 (see §12)

## 1. The apparent tension

| Want | Pushes toward |
|---|---|
| Fewer PUTs (cost) | **Bigger batches** ⇒ longer buffering |
| Fast search-after-write | **Smaller batches** ⇒ more PUTs |

Every engine in this space has this dial. Elasticsearch calls it `refresh_interval` (default
1 s: "a document indexed at time T becomes searchable around T+1s"), Kafka calls it
`linger.ms` + `batch.size`, WarpStream calls it the flush interval (250 ms or 4–8 MiB).

**The tension is real but it is not fundamental.** It only exists if visibility is defined as
"present in an object on the blob store." Once it isn't, the dial has two independent halves.

## 2. The finding that reframes the problem

The dominant PUT cost is **not** proportional to how much data you write. It is proportional
to **how many independent streams you flush, times how often you flush them** — a floor you
pay for merely being alive.

WarpStream published the arithmetic for Kafka partitions, and it transfers directly:

> A file per partition every **100 ms** costs **~$130/month per partition** in S3 PUTs alone.
> Every **250 ms**, ~**$50/month per partition**. Multiply by 1,024 partitions.

Substitute "index" for "partition" and the problem is far worse for us, because we target
**millions** of indexes rather than thousands of partitions.

### The per-index flush floor

`PUTs/month = 2,592,000 / T`, at $5 × 10⁻⁶ per PUT:

| Flush interval `T` | $/month **per index** | × 1M indexes |
|---|---|---|
| 250 ms | $51.84 | $51.8M |
| 1 s | $12.96 | **$13.0M** |
| 10 s | $1.30 | $1.3M |
| 60 s | $0.216 | **$216,000** |
| 10 min | $0.0216 | $21,600 |
| 1 hour | $0.0036 | $3,600 |

**An index that writes one document per minute costs the same as one writing a million per
second**, if both flush on a timer. That is the whole problem, and it is invisible in any
benchmark that uses a single index.

> **Finding B-1.** Per-index batching is structurally broken at our tenancy scale. No choice
> of `T` is acceptable: 1 s bankrupts us, and 60 min destroys durability latency. The dial has
> no good setting, which means the dial is the wrong control.

## 3. The decomposition that dissolves it

A naive design conflates three things that are actually independent:

| Quantity | What it controls | Must it involve the blob store? |
|---|---|---|
| **Batch size / flush interval** | PUT cost | Yes |
| **Durability latency** | when a `durable` write can be acked | Yes — it *is* the PUT |
| **Visibility latency** | when a query can see the write | **No** |

Visibility is a *query routing* property, not a storage property. A document is searchable as
soon as **every node that could answer a query for that index has it in memory** — which can
be milliseconds after arrival, entirely independent of when the bytes reach S3.

Pinecone states exactly this design: writes are acked in under 100 ms to a WAL on S3 "without
waiting for indexing to complete"; an in-memory **memtable** on the index builder holds recent
writes; a **freshness layer** serves them; "because all reads can be routed through the
memtable on the index builder, newly written vectors are immediately queryable." They call it
a lambda-style architecture for vector search.

> **Finding B-2.** Once visibility is served from replicated memory rather than from object
> storage, **the flush interval stops being a latency parameter**. We can batch for seconds
> and still be searchable in ~1 ms.

Everything below follows from B-1 and B-2.

---

## 4. Lever 1 — Bundle across indexes (100×–1000×)

**The change:** a node writes **one object per flush window containing writes for every index
and shard it buffered**, instead of one object per index.

```
{h}/wal/{node_id}/{seq:016}.bundle
┌──────────────────────────────────────────────────────┐
│ records for idx A/s3   (contiguous)                  │
│ records for idx B/s0   (contiguous)                  │
│ records for idx Q/s7   (contiguous)                  │
│ … sorted by (index_id, shard) …                      │
├──────────────────────────────────────────────────────┤
│ BUNDLE INDEX: [(index_id, shard, offset, len, count)]│
│ FOOTER: fixed size, at a known suffix offset         │
└──────────────────────────────────────────────────────┘
```

This is precisely WarpStream's design — *"WarpStream Agents create individual files that
contain data from many different topic-partitions, which keeps both costs and latency low"* —
and it is the single highest-leverage change available to us.

### The cost model changes shape

```
PUTs/s  =  max(  write_bytes_per_s / bundle_bytes  ,  active_writer_nodes / T  )
                 └── data-driven term ──┘             └── time-driven floor ──┘
```

The time-driven floor is now bounded by **node count**, not **index count**. That is the
structural win: it decouples cost from tenancy.

> **Refinement — size the writer set, don't inherit it.** This document assumed
> `active_writer_nodes = N` (the whole fleet), because write placement followed read
> placement. With most tenants idle at any instant that is wasteful: the floor should be set
> deliberately. Writes fan **in** to a derivable cohort of `W* = write_bytes_per_s × T / B`
> nodes, which balances the two terms above and takes the floor from $129,600/month
> (W=10,000, T=1 s) to ~$1,555 (W=600, T=5 s). See
> [`../10-benchmarks-cost/tenancy-scale-model.md`](../10-benchmarks-cost/tenancy-scale-model.md) §3.

### Worked example: 1M indexes each writing 1 doc/minute (~1 KB docs)

Total: 16,667 docs/s ≈ 16.7 MB/s.

| Design | PUTs/month | $/month |
|---|---|---|
| Per-index, 60 s flush | 43.2 B | **$216,000** |
| Per-index, 1 s flush | 2.59 T | $13.0M |
| **Node bundles**, 8 MiB / 5 s, ~100 active writer nodes | ~5–52 M | **$26 – $260** |

**~1,000× cheaper**, and (per Lever 2) *lower* visibility latency than the 60 s design.

### The hard part: how does a reader find a bundle?

`{h}/wal/{node_id}/{seq}` is not derivable from `index_id`. Four candidate answers:

| Option | Verdict |
|---|---|
| (a) Write a per-index pointer object | ❌ Reintroduces the per-index PUT. Defeats the purpose. |
| (b) Gossip the bundle contents | ⚠️ Fast, but a hint — not durable. Fine as an accelerator, not as the mechanism. |
| (c) A durable reverse index (bundle → indexes) | ❌ It is itself a write; same floor. |
| (d) **Write-routing = read-routing** | ✅ |

**(d) is the answer.** Writes for index A are routed to A's LRH placements — the *same*
function that routes reads. A reader (or a recovering node) therefore knows the small set of
node lanes that could contain A's data, and probes them. It is derived, not discovered:
**zero LIST**, consistent with Design rule 4.

Routing writes does **not** create ownership: if the placements are unreachable, any node may
write to its own bundle lane and register itself in the per-shard **lane bitmap**
(`write-path-and-wal.md` §3), which is the existing durable fallback. Placement stays a hint.

### Recovery scanning is cheap *because it amortizes*

A node taking over from a dead peer scans that peer's bundle lane once and recovers **every
index in it simultaneously**. With hourly folds and a fleet writing 2 bundles/s, that is
~7,200 suffix GETs — issued in parallel (one round trip), costing **~$0.003 total**, not per
index. The expensive-looking path is cheap precisely because bundles are shared.

### Large writers get their own object

If one index's pending bytes exceed the bundle target on their own, it gets a dedicated
object. Bundling is for the long tail; a firehose tenant fills a batch unaided and should not
have its data interleaved with anyone else's.

> **D-37.** Node-level cross-index bundles are the default WAL unit. Per-index objects are the
> exception, used only when a single index fills a bundle by itself.

---

## 5. Lever 2 — The freshness layer (this is the latency answer)

```
write arrives
  └─▶ routed to primary placement of (index, shard)          ~0.2 ms
  └─▶ appended to that index's in-memory memtable
  └─▶ replicated in-memory to the R−1 other placements       ~0.2–0.5 ms  (intra-AZ)
  └─▶ SEARCHABLE  ◀── every node that can answer a query for this index now has it
  ⋯
  └─▶ (up to T later) folded into the node's bundle, ONE PUT  ← durability
  └─▶ (later) folded into a segment + ANN/BM25 index          ← query efficiency
```

A query for index A is already routed to a placement of A for cache affinity
(`04-cluster/routing-and-placement.md`), so **the node holding the memtable is the node
answering the query.** Visibility costs nothing extra; it reuses routing we already do.

The memtable is scanned **exactly** — brute force with SIMD over quantized codes. It is small
by construction (capped, see §6), so this is microseconds to low milliseconds and *more*
accurate than the ANN path. Merging its results with the indexed segments is the same
cross-source merge the query engine already performs across segments.

**Fallback:** a query landing on a non-placement node either forwards (which it already does)
or, for `bounded` reads, serves without the memtable and reports its staleness honestly
(`03-metadata-consistency/consistency-model.md`).

> **D-38.** Visibility is served by an R-way in-memory-replicated memtable at the placement
> nodes. The flush interval is tuned for **cost and durability latency only** — never for
> freshness.

---

## 6. Lever 3 — Dual-trigger, adaptive flush

Flush on `bytes ≥ B` **or** `age ≥ T`, whichever first — the Kafka/WarpStream shape. Because
visibility is decoupled, `T` can be far larger than the 250 ms–1 s that other systems are
forced into.

| Parameter | Value | Bound by |
|---|---|---|
| `B` (bundle target) | 8–32 MiB | Diminishing returns above; NIC and memory below |
| `T` (max buffering) | **250 ms – 5 s**, per durability class | `durable`-mode ack SLO — *not* visibility |
| Memtable cap per index | ~16–64 MiB | Exact-scan cost per query |
| Total buffer per node | fraction of RAM | Spill to local NVMe under pressure |

A useful counter-intuitive datum: **Kafka 4.0 changed `linger.ms` from 0 to 5 ms by default**,
because "the efficiency gains from larger batches typically result in similar or lower
producer latency despite the increased linger." Batching reduces per-request overhead and
queueing; a little linger is often free or better. Our `T` is doing the same work at a much
larger scale.

**Per-tenant adaptivity:** `T` scales down as an index's observed write rate rises (a firehose
hits `B` first anyway) and up for trickle tenants (who gain nothing from a fast durable ack).
The memtable makes the trickle case latency-neutral.

---

## 7. Lever 4 — The second-order floor: fold and commit rate

Bundles solve the WAL. But un-folded data cannot accumulate forever: memtables consume RAM,
bundles cannot be GC'd until folded, and the recovery-scan window grows. Folding means a
structural commit — **and structural commits are per-index, so they have their own floor**:
1 segment PUT + 1 manifest PUT + 1 HEAD CAS.

Three reductions:

1. **Adaptive fold trigger.** `fold when pending_bytes > 1 MiB OR pending_age > 1 h OR memory
   pressure`. A trickle index folds hourly; a firehose index folds on bytes, where the PUT is
   amortized over megabytes.
2. **Inline small indexes.** Below a size threshold (~256 KiB), an index's entire contents
   live **inside HEAD**. No segment, no manifest — **an index with 500 documents is literally
   one object**, and a fold is a single CAS PUT. Given the tenancy distribution we expect,
   this covers a large fraction of all indexes.
3. **Shared L0 segments.** Where several small indexes fold at once on the same node, their L0
   segments may share one object addressed by byte range (same trick as bundles). Their
   manifests still reference it individually. *Use sparingly* — it complicates GC and
   branching, so restrict it to short-lived L0 and never to compacted levels.

Fold cost at 1M continuously-trickling indexes: ~720 commits/index/month ≈ **$3,600/month**
fleet-wide, versus $216,000 for per-index 60 s flushing. Idle indexes cost **zero**.

> **D-39.** After bundling, the *fold rate* becomes the dominant per-index PUT cost. It must be
> adaptive and size-driven, never a fixed timer.

> **And the CAS unit must be the tenant, not the index** — at 1M × 50 that is worth another
> 50× ($540k → $10.8k/month). A tenant's indexes are written by one application, buffered on
> one cohort node, and commit together in one CAS. See
> [`../10-benchmarks-cost/tenancy-scale-model.md`](../10-benchmarks-cost/tenancy-scale-model.md) §4.

---

## 8. Lever 5 — Storage tiering, and a correction

`write-path-and-wal.md` §5 recommended dual-writing the WAL to S3 Express One Zone for latency
plus Standard for durability, calling the second PUT negligible. **That understated the cost.**

Express One Zone is **single-AZ**, so multi-AZ durability requires writing multiple copies —
WarpStream writes to **three Express buckets for quorum**, which they note **neutralizes the
per-request savings**. And since April 2025, Express charges **$0.0032/GB on all bytes
uploaded** (the former 512 KiB-free allowance is gone).

For an 8 MiB bundle:

| Path | Requests | Data transfer | Total |
|---|---|---|---|
| Standard, 1 copy | $0.000005 | free (intra-region) | **$0.000005** |
| Express, 3-copy quorum | $0.0000034 | $0.000075 | **$0.000078** |

**≈16× more expensive**, and the cost is dominated by per-GB transfer, not requests — the
opposite of the Standard cost structure. Express's advantage is real but narrow: WarpStream
reports **4× lower end-to-end latency, p99 write latency from 400–600 ms down to single-digit
ms**.

> **Correction C-1.** Express One Zone is a **latency purchase, not a cost saving**, and it
> gets *worse* as bundles get larger. Default the write path to **Standard + bundling +
> freshness layer**. Offer Express as an opt-in tier for customers who need single-digit-ms
> **durable** acks, priced to reflect the ~16× premium. Most search workloads do not need it,
> because the freshness layer already gives them ~1 ms *visibility*.

---

## 9. The read side of bundling

Bundles mix indexes, so reading one for index A also brings along B and C. Three mitigations:

1. **Sort by `(index_id, shard)` inside the bundle** so each index's records are contiguous —
   one ranged GET, never a scatter.
2. **Suffix-GET the footer first** (Pattern 6): one `Range: -N` read yields the bundle index,
   which yields exact ranges for everything else. Same contract as segments.
3. **Scan sharing.** WarpStream's *distributed mmap*: consistent-hash each **file** to one
   agent, which pages it in fixed **4 MiB** chunks and serves all readers of that file. Three
   fetches for three different partitions inside one chunk become **one object-storage
   request**, and GET count "scales ~linearly with the throughput of the workload" rather than
   with partition count.

> **D-40.** Add **bundle-id → cache-owner** as a second LRH placement dimension, distinct from
> index placement, so all readers of a bundle converge on one node's 4 MiB pages. This matters
> on the recovery and cold-reader paths; the steady state is served from the memtable.

They also flag the hazard we inherit: **one lagging reader can double GET count**, because it
re-reads live-edge files that everyone else already passed. Their fix — background compaction
of small ingestion files into large ones — is exactly our fold.

---

## 10. The latency budget, before and after

Time from `write()` returning to a strong-consistency query seeing the document:

| Component | Per-index batching (60 s) | pstore (bundles + freshness layer) |
|---|---|---|
| Buffer/flush window | 0 – 60,000 ms | **0** — visible on arrival |
| PUT round trip | 50 – 250 ms | **0** — off the visibility path |
| Reader tail discovery | 100 – 1,000 ms | **0** — same node holds it |
| Fold + index build | seconds – minutes | **0** for correctness; memtable is scanned exactly |
| Replication to placements | — | **0.2 – 0.5 ms** |
| **Time-to-searchable** | **0.2 – 60 s** | **≈ 1 ms** |
| **`durable` ack latency** | 50 – 250 ms | 250 ms – 5 s (tunable, `T`) |
| **`batched` ack latency** | — | **≈ 1 ms** |

The tradeoff has not vanished — it has **moved onto the durable-ack axis, where clients can
choose it**, and off the visibility axis, where they could not.

Compare Elasticsearch's ~1 s `refresh_interval` and Pinecone's "data appears in search results
within seconds": a ~1 ms visibility floor with hour-scale batching is a genuine
differentiator, and it falls out of routing we already do.

---

## 11. Hazards

| Hazard | Mitigation |
|---|---|
| **Loss window in `batched` mode** | Un-flushed writes survive R−1 node failures but not simultaneous loss of all R (e.g. an AZ). Document it precisely; `durable` mode remains available per request. |
| **Memory pressure from many memtables** | Global buffer budget with per-tenant caps (Design rule 13); spill to local NVMe; force-fold under pressure. |
| **Cross-tenant data in one object** | Per-index encryption keys applied per byte range; access control at the range, never the object. **Must be designed in, not added later.** |
| **Bundle GC** | A bundle is referenced by many indexes; delete only when all are folded. Enforce with a TTL plus forced fold on approach to expiry. |
| **Query fan-in grows** | Query merges memtable + un-folded bundles + segments. Cap memtable size and bound un-folded bundles per index. |
| **Two stacked batching layers** | Client-side batching plus our `T` can multiply latency (the classic Nagle + delayed-ACK pathology). Document the interaction; report the server-side wait in the response. |
| **Bundle lane prefix hotspot** | One node's lane is one prefix at a few PUT/s — far under the 3,500/s limit. |
| **Placement change mid-buffer** | Old primary force-flushes and hands the watermark to the new one; the bundle is durable regardless. |

## 12. Anti-patterns

| Anti-pattern | Why |
|---|---|
| Per-index time-driven flush | The $216k–$13M/month floor of §2 |
| Multipart upload for batches | **Every `UploadPart` is a PUT-class request** (a 100 GB object in 800 parts = 802 billable requests). A single `PutObject` handles up to **5 GiB in one request**. Use MPU only above 5 GiB or for genuinely streamed writes. |
| Tuning the flush interval for freshness | That is the memtable's job; conflating them re-creates the tension |
| Defaulting to Express One Zone for cost | See Correction C-1 — it is ~16× *more* expensive at bundle sizes |
| Scattering an index's records through a bundle | Forces a scatter-read; sort by `(index_id, shard)` |

## 13. Corrections to earlier documents

- **C-1** — `write-path-and-wal.md` §5 and `request-efficiency-patterns.md` Pattern 9: Express
  One Zone is a latency purchase, not a cost saving. Corrected in §8 above.
- **C-2** — `write-path-and-wal.md` §3: lanes are per-`(index, shard)`. They are now
  per-**node**, carrying many indexes per object; the per-shard lane bitmap survives as the
  fallback-discovery mechanism.
- **C-3** — `compaction.md` §"Compaction economics": a 4 GiB output was costed at 64 × 64 MiB
  multipart parts. It is under the 5 GiB single-PUT limit, so it is **1 request, not 64**.
  Compaction request cost is ~64× lower than stated (it was already negligible; CPU remains
  the real constraint).

## 14. Open questions raised

- **OQ-84 (Tier 1)** — Measure the real per-index tenancy distribution: what fraction of
  indexes are trickle writers? This sets the value of the entire bundling design and the inline-
  small-index threshold.
- **OQ-85 (Tier 1)** — Memtable memory budget vs. fold rate vs. recovery-scan window: the
  three-way optimum. Model it, then measure.
- OQ-86 — Optimal bundle size `B`. Larger cuts PUTs but raises read amplification for a single
  index's slice and increases `durable` ack latency.
- OQ-87 — Is per-byte-range encryption sufficient isolation for cross-tenant bundles, or do
  compliance requirements force per-tenant objects? **Ask customers before building.**
- OQ-88 — Should bundles be sorted by `(index_id, shard)` or clustered by expected read
  affinity? Sorting is simpler and probably enough.
- OQ-89 — Shared L0 segments across indexes: worth the GC and branching complexity, or does
  inlining small indexes already capture the win?
- OQ-90 — Does the R-way in-memory memtable replication meaningfully raise intra-AZ network
  cost or tail latency at 10K nodes?
- OQ-91 — Recovery correctness: prove that `HEAD.lane_watermarks` + forward probing of
  placement nodes' bundle lanes finds **every** un-folded record, under placement change,
  fallback writes, and node death. This is a simulator target for M2.

## Sources

- [Write Path — WarpStream docs (250 ms / 8 MiB flush, multi-partition files)](https://docs.warpstream.com/warpstream/overview/architecture/write-path)
- [Minimizing S3 API Costs with Distributed mmap — WarpStream](https://www.warpstream.com/blog/minimizing-s3-api-costs-with-distributed-mmap)
- [How WarpStream enables cost-effective low-latency streaming with Amazon S3 Express One Zone — AWS Storage Blog](https://aws.amazon.com/blogs/storage/how-warpstream-enables-cost-effective-low-latency-streaming-with-amazon-s3-express-one-zone/)
- [Architecture — WarpStream docs](https://docs.warpstream.com/warpstream/overview/architecture)
- [Reimagining the vector database — Pinecone (memtable, freshness layer, lambda architecture)](https://www.pinecone.io/blog/serverless-architecture/)
- [The vector database to build knowledgeable AI — Pinecone](https://www.pinecone.io/how-pinecone-works/)
- [Elasticsearch Refresh Interval: Defaults, Tuning, and Trade-offs](https://pulse.support/kb/what-is-elasticsearch-refresh-interval)
- [Tune for indexing speed — Elastic Docs](https://www.elastic.co/docs/deploy-manage/production-guidance/optimize-performance/indexing-speed)
- [Kafka Batch Producer: batch.size & linger.ms — Conduktor](https://www.conduktor.io/kafka/kafka-producer-batching)
- [Producer Configs — Apache Kafka (linger.ms default 0 → 5 ms in 4.0)](https://kafka.apache.org/41/configuration/producer-configs/)
- [Uploading and copying objects using multipart upload — AWS docs](https://docs.aws.amazon.com/AmazonS3/latest/userguide/mpuoverview.html)
- [Are we billed a PUT request for each part with S3 multipart upload? — AWS re:Post](https://repost.aws/questions/QUXmwDga0VRvSOOjYWMfor-w/are-we-billed-a-put-request-for-each-part-with-s3-multipart-upload-or-only-once-for-the-final-merged-file)
- [Announcing up to 85% price reductions for Amazon S3 Express One Zone — AWS](https://aws.amazon.com/blogs/aws/up-to-85-price-reductions-for-amazon-s3-express-one-zone/)
- [S3 Pricing — AWS](https://aws.amazon.com/s3/pricing/)
