# Blob Store API Semantics — the portable primitive set

**Answers:** Q4
**Status:** Complete (v1)
**Retrieved:** 2026-09-05

## Why this is the first document

`pstore` puts *everything* durable — rows, indexes, and all metadata — in a blob store, with
no external coordinator. That means the blob store's API is our concurrency primitive set,
our transaction log, and our lock manager. Anything not portable across S3/GCS/Azure either
becomes a per-backend capability flag or must not be depended on.

## 1. The operation set we can rely on

| Capability | S3 | GCS | Azure Blob | Notes |
|---|---|---|---|---|
| PUT whole object (atomic) | ✅ | ✅ | ✅ (Put Blob) | Atomic replace; no partial visibility. |
| GET whole object | ✅ | ✅ | ✅ | |
| **Ranged GET** | ✅ `Range:` | ✅ `Range:` | ✅ `x-ms-range` | **Load-bearing.** Random access inside big objects. |
| Multi-range GET in one request | ❌ (S3 ignores multipart ranges) | ❌ | ❌ | Must issue N requests, or fetch a superset. See §4. |
| HEAD (metadata only) | ✅ | ✅ | ✅ (Get Blob Properties) | Priced as a read/GET-class op everywhere. |
| **Conditional PUT — create-if-absent** | ✅ `If-None-Match: *` (GA Aug 2024) | ✅ `ifGenerationMatch=0` | ✅ `If-None-Match: *` | The masterless-CAS primitive. |
| **Conditional PUT — compare-and-swap** | ✅ `If-Match: <etag>` (GA Nov 2024) | ✅ `ifGenerationMatch=<gen>` | ✅ `If-Match: <etag>` | The manifest-update primitive. |
| Conditional GET/HEAD | ✅ | ✅ | ✅ | Cheap cache revalidation. |
| Conditional DELETE | ✅ `If-Match` | ✅ `ifGenerationMatch` | ✅ `If-Match` | Safe GC of a version you actually saw. |
| Multipart / chunked upload | ✅ MPU | ✅ resumable / XML MPU | ✅ Put Block + Put Block List | Different shapes; abstract it. |
| **Append to existing blob** | ❌ | ❌ | ✅ Append Blob | **Not portable — do not design around it.** |
| Server-side copy | ✅ | ✅ (rewrite) | ✅ (Copy Blob) | Useful for cheap re-parenting; still a write-class op. |
| Batch delete | ✅ DeleteObjects (1000/req) | ❌ (per-object; SDK batches HTTP) | ✅ Blob Batch (256/req) | Delete is free on S3, billed on GCS/Azure. |
| Object versioning | ✅ | ✅ (generations, always present) | ✅ (opt-in) | GCS generations are the nicest model. |
| **Lease / lock** | ❌ | ❌ | ✅ (15–60 s or infinite) | **Not portable — emulate with CAS + fencing.** |
| LIST | ✅ | ✅ | ✅ | Expensive + eventually-fresh-but-slow. **We avoid it.** |
| Strong read-after-write | ✅ (since Dec 2020) | ✅ | ✅ | List-after-write is also strongly consistent on S3 now, but still costly. |

### The crucial finding

Every major cloud now has **compare-and-swap on a blob**. This is what makes a masterless,
dependency-free design possible at all: a single blob can act as a linearizable register,
and a linearizable register is enough to build a manifest chain, a lease, and a fencing
token.

- S3 added `If-None-Match` on PutObject in **August 2024** and `If-Match` (true CAS) in
  **November 2024**. Before that, S3 was the *only* major store without preconditions and
  everyone building on it needed DynamoDB. That constraint is gone.
- GCS has had `ifGenerationMatch` since forever, and its **generation numbers** are
  monotonic per object — strictly more useful than an opaque ETag because the generation
  itself is a usable fencing token.
- Azure uses ETags plus a native lease, and additionally accepts comma-separated ETag lists
  in `If-Match`/`If-None-Match`.

Failure mode is uniform: **HTTP 412 Precondition Failed**. S3 can also return **409
Conflict** when it cannot evaluate the condition because a concurrent write to the same key
is in flight — this must be retried, and is *not* the same as "I lost the race".

> **Design rule 1.** The only mutable pointer in the system is a CAS'd blob. Everything else
> is immutable, content-addressed, and written exactly once.

> **Design rule 2.** We never use Azure leases or Azure append blobs, even though they are
> convenient, because they don't exist on S3/GCS. Leases are emulated (see
> `04-cluster/ownership-and-leases.md`).

## 2. ETag portability hazards

ETags are *not* uniformly content hashes:

- S3 single-part PUT with SSE-S3 → ETag is the MD5 of the object.
- S3 **multipart** upload → ETag is `md5(concat(part_md5s))-<numparts>`, not a content hash.
- S3 with SSE-KMS or SSE-C → ETag is **not** MD5 at all.
- GCS ETags are opaque and change representation; the **generation** number is the stable
  identity.
- Azure ETags are opaque quoted strings.

> **Design rule 3.** Treat ETags as **opaque comparison tokens only**. Never parse them,
> never assume they are a digest, never use them for integrity. We store our own content
> hash (xxh3 / blake3) in object metadata and in the manifest.

## 3. Where LIST hurts, and what replaces it

LIST is the operation we are designing to avoid, for four separate reasons:

1. **Price.** It is billed at the expensive write-class rate (S3: `$0.005/1000`, same bucket
   as PUT; GCS: Class A; Azure: "List and Create Container" ops), *and* it returns at most
   1000 keys per request. Enumerating 10M objects = 10,000 LIST requests = the price of
   10,000 PUTs, and it is **12.5× the cost of a GET on S3 for less information**.
2. **Latency.** A paginated LIST is inherently serial — each page's continuation token
   depends on the previous response. You cannot parallelize a scan of an unknown keyspace.
3. **Scale ceiling.** With millions of indexes and 10K nodes, any startup path involving
   "list the bucket to discover what exists" is O(dataset) per node and quadratic in the
   fleet.
4. **Semantics.** LIST tells you what objects exist, not which ones are *committed*. A
   listing can show partially-written or garbage state. Reconstructing a consistent view
   from a listing requires convention and prayer.

**The replacement is determinism.** If every object's key is computable from data we already
have (index id, epoch, shard id, sequence number), we never need to ask what exists — we
compute the key and GET it. The manifest tells us what is committed; the key derivation
tells us where it is.

> **Design rule 4.** LIST is permitted in exactly three places: (a) offline GC / orphan
> reaping, (b) disaster recovery when the manifest chain is lost, (c) admin tooling. It is
> forbidden on the read path, the write path, node startup, and index open.

## 4. Ranged reads: the actual workhorse

No cloud supports multi-range GET in a single request in a way we can rely on (S3 returns
the whole object or the first range; the multipart/byteranges response type isn't offered).
This creates the central tension of the design:

- N small ranged GETs = N round trips = N × ~30–60 ms of latency, N × GET price.
- 1 large GET of a superset = 1 round trip, 1 GET price, but wasted bytes and bandwidth.

Since GETs are cheap ($0.0004/1000 on S3) and latency is expensive, and since retrieval
bandwidth within the same region is free on S3, **the correct bias is: fetch the superset**.
This is the "round-trip sensitive" principle turbopuffer describes — maximize data per
access rather than minimize bytes.

We formalize this in `request-efficiency-patterns.md` as the *coalescing rule*: merge two
ranged reads into one if the gap between them is smaller than the break-even gap
`G* ≈ latency_saved × bandwidth`, which for S3-class storage lands in the **hundreds of KB
to low MB** range.

## 5. Throughput and rate limits

- S3: **≥3,500 PUT/COPY/POST/DELETE and ≥5,500 GET/HEAD per second per partitioned prefix**,
  with no limit on prefix count. Partitions are created adaptively; during scale-up you get
  `503 SlowDown`.
- The adaptive split is *reactive*, taking minutes to tens of minutes. A cold index whose
  keys all share a prefix will be rate-limited on its first burst.

> **Design rule 5.** Key layout must spread entropy **high in the key**, not low. Prefixing
> with a hash of the index id gives every index its own partition lineage from the start, and
> gives a 10K-node fleet ~unbounded aggregate request budget. See `11-design/key-layout.md`.

> **Design rule 6.** `503 SlowDown` is a *normal* signal, not an error. Every blob call goes
> through a shared adaptive concurrency limiter with jittered exponential backoff, per
> prefix-group.

## 6. Backends we should support, and their quirks

| Backend | CAS | Notes |
|---|---|---|
| AWS S3 (general purpose) | ✅ | Baseline. |
| AWS S3 Express One Zone (directory buckets) | ✅ | Single-AZ, ~10× lower latency, much cheaper GET, pricier storage. Ideal for the **WAL tier**, not the data tier. |
| GCS | ✅ generations | Best precondition model. |
| Azure Blob (Block Blob) | ✅ | Also has leases we deliberately ignore. |
| Cloudflare R2 | ✅ | Zero egress; had preconditions before S3 did. |
| Tigris | ✅ | S3-compatible, preconditions supported. |
| MinIO / Ceph RGW | ✅ | For on-prem and tests. |

> **Design rule 7.** The storage trait is defined in terms of the *portable* subset plus an
> explicit `Capabilities` struct (`supports_cas`, `supports_batch_delete`,
> `delete_is_free`, `max_batch_delete`, `typical_ttfb_ms`). The engine reads capabilities and
> adapts policy (e.g. compaction aggressiveness) rather than branching on backend name.

> **Capabilities are measured, not declared.** A conformance suite probes each backend and
> records what it *actually* does; the table above is the specification, not the observed
> reality. Self-hosted implementations diverge on precisely the CAS semantics we depend on —
> MinIO does not accept the `*` wildcard at all. A backend whose recorded profile marks CAS
> `Divergent` must **refuse to serve `durable` writes**, failing loudly at startup rather than
> corrupting silently. See
> [`../09-rust-stack/dev-and-test-environment.md`](../09-rust-stack/dev-and-test-environment.md).

## Open questions raised

- OQ-1: Does S3's 409-on-concurrent-conditional-write have a bounded retry cost under a
  10K-node thundering herd on one manifest key? Needs measurement.
- OQ-2: Exact break-even gap `G*` per backend for range coalescing. Needs measurement.

## Sources

- [Add preconditions to S3 operations with conditional requests — AWS docs](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-requests.html)
- [Amazon S3 adds new functionality for conditional writes — AWS What's New](https://aws.amazon.com/about-aws/whats-new/2024/11/amazon-s3-functionality-conditional-writes)
- [Building multi-writer applications on Amazon S3 using native controls — AWS Storage Blog](https://aws.amazon.com/blogs/storage/building-multi-writer-applications-on-amazon-s3-using-native-controls/)
- [What's the Big Deal with Conditional Writes Support in S3? — Tigris](https://www.tigrisdata.com/blog/s3-conditional-writes/)
- [Manage concurrency in Blob Storage — Microsoft Learn](https://learn.microsoft.com/en-us/azure/storage/blobs/concurrency-manage)
- [Specifying conditional headers for Blob service operations — Microsoft Learn](https://learn.microsoft.com/en-us/rest/api/storageservices/specifying-conditional-headers-for-blob-service-operations)
- [Conditional Requests in Cloud Object Storage — Udaya Chathuranga](https://medium.com/@udayaw/conditional-requests-in-cloud-object-storages-267d21914d37)
- [Best practices design patterns: optimizing Amazon S3 performance — AWS docs](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html)
- [Leader Election With S3 Conditional Writes — Gunnar Morling](https://www.morling.dev/blog/leader-election-with-s3-conditional-writes/)
