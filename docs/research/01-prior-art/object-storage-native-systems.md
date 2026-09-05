# Prior Art: Systems That Treat Object Storage as the Only Durable Tier

**Answers:** Q2
**Status:** Complete (v1)
**Retrieved:** 2026-09-05

The "zero-disk architecture" (ZDA) pattern. What each system did, and — most importantly —
**what each one still needed a coordinator for**, since eliminating that is `pstore`'s
central claim.

## Scorecard: who is actually masterless?

| System | Data on blob store | **Metadata on blob store** | External coordinator | Verdict |
|---|---|---|---|---|
| **WarpStream** (Kafka) | ✅ | ❌ | **Proprietary hosted metadata store** | Not masterless |
| **SlateDB** | ✅ | ✅ (manifest + CAS) | none | **Masterless** (single-writer by design) |
| **Quickwit** | ✅ (splits) | ⚠️ Postgres, *or* JSON-on-S3 | Postgres (recommended) | Hybrid |
| **Neon** | ✅ (WAL → pageserver → S3) | ❌ | Postgres control plane + Paxos safekeepers | Not masterless |
| **Iceberg / Delta** | ✅ | ✅ *if* REST catalog uses conditional PUT | usually a catalog service | Depends on catalog |
| **turbopuffer** | ✅ | ✅ | none in critical path; **stateless broker** for queues | Nearly — see below |
| **LanceDB** | ✅ | ✅ (manifest) | catalog for enterprise | Hybrid |
| **S3 Vectors** (AWS) | ✅ | ✅ (AWS-internal) | AWS operates it | N/A (it is the substrate) |
| **`pstore` (target)** | ✅ | ✅ | **none, ever** | The goal |

**The finding that matters:** almost everyone who built a ZDA system before August 2024 had
to put metadata somewhere else — DynamoDB, Postgres, or a proprietary service — because S3
had no conditional writes. WarpStream's entire business model has a "metadata scaling is
handled by WarpStream Cloud" clause for exactly this reason. **That constraint expired.**
`pstore` is being designed in the post-CAS era and can be genuinely dependency-free.

---

## WarpStream — the origin of ZDA

- Stateless **Agents**; no local disk, no data ownership, therefore **no rebalancing when
  scaling** — add or remove agents in seconds. Any agent can be leader for any topic, commit
  offsets for any consumer group, or act as cluster coordinator.
- Data goes straight to object storage; agents batch aggressively to make PUT economics work.
- **But:** all cluster metadata (topic config, partition assignments, consumer offsets) lives
  in a separate proprietary, strongly-consistent hosted metadata store.

**Take:** the stateless-agent + no-rebalancing property is exactly what we want, and is what
makes 10K nodes tractable. The lesson from their split is that *metadata is the hard part*,
not data. We must solve metadata on the blob store or we will end up shipping a control
plane too.

## SlateDB — the closest technical relative

An embedded LSM key-value store whose only durability dependency is object storage.

- MemTables flush periodically to object storage as SSTs; **writes are batched to mitigate
  PUT costs**.
- Reads mitigated by "standard LSM caching techniques: in-memory block caches, compression,
  bloom filters, and local SST disk caches."
- Manifest on object storage with CAS for atomic state transitions.

**Take:** validates the whole storage-engine shape (LSM + manifest + CAS + block cache) and
the economics. Its limitation is that it is *embedded and effectively single-writer* — it
does not solve multi-writer or fleet-scale placement, which is where our work is.

## Quickwit — the closest search relative

- Index = a set of immutable **splits** on S3. A split bundles inverted index + columnar +
  row store + a **hotcache**.
- **The hotcache is the key trick:** a metadata "blueprint" that is **<0.1% of split size
  (~10 MB for a 15 GB split)**, whose byte range is recorded in the metastore's
  `split_footer` field. One GET of the footer range gives you everything needed to plan the
  rest of the reads.
- Metastore is Postgres *or a single JSON file on object storage*.
- Merge policy: merge 10 splits until reaching 10M docs — bounding both metastore rows and
  the number of splits a query must open.
- Complete indexer/searcher separation.

**Take — three ideas we adopt directly:**
1. **The hotcache/footer pattern.** Our data objects get a self-describing footer at a known
   suffix offset, fetched with one `Range: -N` GET. This is Pattern 6.
2. **Bounding the number of objects a query touches** is a first-class merge-policy goal, not
   a side effect. Query fan-out is proportional to file count.
3. Their JSON-metastore-on-S3 shows the shape of a blob-native catalog, and its scaling
   limits show why we need to shard it.

## Neon — WAL-first separation of storage and compute

Compute and storage as independent layers communicating via a **stream of WAL records**;
storage tier (safekeepers → pageservers) materializes pages and tiers to S3.

**Take:** the WAL-as-the-interface idea is right and we use it. But Neon needs Paxos
safekeepers for low-latency durability because Postgres commit latency can't absorb a
200 ms S3 PUT. We dodge this by **exposing the tradeoff to the user** (`durable` vs
`batched` modes) rather than building a consensus tier.

## Iceberg / Delta Lake — the commit protocol we are reusing

Iceberg's commit: read the current metadata pointer → stage a new `metadata.json` →
**atomic compare-and-swap the pointer** → on loss, retry against the new snapshot. It "works
correctly on any storage system with an atomic pointer swap," implemented as a DB
transaction (JDBC catalog), **conditional PutObject (REST catalog)**, or atomic rename
(HDFS).

The metadata tree — pointer → metadata file → manifest list → manifests → data files —
delivers atomic commits, snapshot isolation, time travel, schema evolution and partition
evolution **without rewriting data**.

**Take:** this is a proven, boring, correct design for exactly our problem, and we should
copy its skeleton rather than invent one. Two adaptations required:
- Iceberg commits are *table-scoped and infrequent* (minutes). Ours must handle a much higher
  structural-change rate, so we split "data commits" (no CAS, lane-based) from "structural
  commits" (CAS) — Pattern 10.
- Iceberg's known weakness is **write conflicts under many concurrent writers** — OCC retry
  storms. Our lane design avoids putting data writes on the CAS path at all.

## LanceDB / Lance format

Columnar format designed for random access and vector search, with a manifest-based
versioning model; enterprise tier separates query serving, indexing, compaction, and
persistence into independently scalable components.

**Take:** validates the "separate the four roles" decomposition, which we mirror as
node *roles* rather than node *types* (any pstore node can do any role — see `04-cluster/`).

## Amazon S3 Vectors — the competitor that is also the substrate

GA December 2025. A `vector bucket` type with a dedicated API; up to **2 billion vectors per
index**, **10,000 indexes per bucket**. Latency: sub-second for infrequent queries, ~100 ms
or less for frequent ones. Pricing: **$0.20/GB PUT, $0.06/GB-month storage**, query cost
tiered from $0.004/TB. Claimed up to 90% cost reduction. AWS positions it as
"complementary" to vector databases.

**Take — this is the most important competitive datapoint in this document:**
- It sets the **price floor** ($0.06/GB-mo) and the **latency ceiling we must beat**
  (~100 ms warm is *bad*; we target 10 ms warm).
- Its limits are our differentiation: 2B vectors/index, 10K indexes/bucket, no BM25, no
  hybrid search, limited filtering, AWS-only, no bring-your-own-cloud.
- The existence of a first-party product validates the category and simultaneously means
  **"cheap vector storage" alone is not a business**. The defensible surface is: hybrid
  search, rich filtering, low warm latency, multi-cloud, and per-tenant isolation at millions
  of indexes.

## Consolidated lessons for `pstore`

1. **Metadata-on-blob-store is the hard part and the differentiator.** Post-2024 CAS makes it
   possible; nobody in this list has fully done it at fleet scale.
2. **Statelessness buys elasticity.** No data ownership ⇒ no rebalancing ⇒ scale in seconds.
   This is the property that makes 10K nodes plausible.
3. **Batch or die.** Every ZDA system independently arrived at group commit.
4. **Footer/hotcache pattern** — one suffix GET to bootstrap an object.
5. **Bound the file count per query** via merge policy.
6. **Copy Iceberg's commit protocol**, but keep bulk data writes off the CAS path.
7. **Expose the durability/latency tradeoff** rather than building a consensus tier to hide
   it.

## Sources

- [Architecture — WarpStream docs](https://docs.warpstream.com/warpstream/overview/architecture)
- [Zero Disks is Better (for Kafka) — WarpStream](https://www.warpstream.com/blog/zero-disks-is-better-for-kafka)
- [SlateDB: An Object-Native LSM for Online Systems](https://slatedb.io/blog/introducing-slatedb/)
- [slatedb/slatedb — GitHub](https://github.com/slatedb/slatedb)
- [Quickwit 101 — Architecture of a distributed search engine on object storage](https://quickwit.io/blog/quickwit-101)
- [Architecture — Quickwit docs](https://quickwit.io/docs/overview/architecture)
- [The lakebase architecture — Neon docs](https://neon.com/docs/introduction/architecture-overview)
- [Writing to an Apache Iceberg Table: How Commits and ACID Actually Work](https://amdatalakehouse.substack.com/p/writing-to-an-apache-iceberg-table)
- [Exploring the Architecture of Apache Iceberg, Delta Lake, and Apache Hudi — Dremio](https://www.dremio.com/blog/exploring-the-architecture-of-apache-iceberg-delta-lake-and-apache-hudi/)
- [Architecture — LanceDB docs](https://docs.lancedb.com/enterprise/architecture)
- [Amazon S3 Vectors is now generally available with 40 times the scale of preview — AWS](https://aws.amazon.com/about-aws/whats-new/2025/12/amazon-s3-vectors-generally-available/)
- [AWS claims 90% vector cost savings with S3 Vectors GA — VentureBeat](https://venturebeat.com/data-infrastructure/aws-claims-90-vector-cost-savings-with-s3-vectors-ga-calls-it-complementary)
- [Zero-Disk Architecture: The Future of Cloud Storage Systems — pracdata](https://www.pracdata.io/p/zero-disk-architecture-the-future)
