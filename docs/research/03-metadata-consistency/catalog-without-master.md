# The Catalog: Millions of Indexes, Zero LISTs

**Answers:** Q9
**Status:** Complete (v1)

## The problem

`pstore` targets **millions of indexes**. Three questions must be answerable without ever
enumerating a bucket:

1. *Where is index X?* — the read/write path.
2. *Does index X exist, and what is its config?* — create/open semantics.
3. *What indexes exist?* — admin listing, billing, GC, global maintenance.

(1) and (2) are hot and must be O(1). (3) is cold but must not be O(bucket).

## 1. The hot path needs no catalog at all

An index's root key is a **pure function of its id**:

```
key(index_id) = "{shard_prefix(index_id)}/idx/{index_id}/HEAD"
shard_prefix(id) = base32(xxh3_64(id))[0..4]      // 4 chars ≈ 1M distinct prefixes
```

The client supplies `index_id`; the node computes the key; one GET returns HEAD. **No
lookup service, no catalog read, no LIST.** The catalog is not on the critical path — it
exists only for enumeration and lifecycle.

The `shard_prefix` also does the S3-partitioning work (Design rule 5): entropy is the
*first* component of the key, so every index gets an independent partition lineage from its
first write, and a 10K-node fleet spreads across ~10⁶ prefixes.

**RA(open index) = 1 Rseq. Forever, at any scale.**

### Existence and creation
`CREATE` = `PUT(HEAD, initial, If-None-Match: *)`. Success ⇒ created; 412 ⇒ already exists.
Idempotent create is one conditional PUT with no read. Delete is a tombstone in HEAD
followed by async reaping (never an immediate mass delete).

## 2. The cold path: a sharded, self-describing catalog

For enumeration we maintain a catalog that is itself blob-resident, CAS'd, and **fixed-width
in fan-out** so it never needs listing:

```
{h}/cat/root                       <- CAS'd; {catalog_epoch, num_buckets, bucket_digests[]}
{h}/cat/b/{bucket:04}/{epoch:020}  <- immutable; a sorted run of index descriptors
```

- `num_buckets` is a fixed power of two (e.g. 4096), so bucket keys are **derived, not
  discovered**. Enumerating all indexes = `num_buckets` parallel GETs = **1 Rpar round**,
  4096 reads ≈ $0.0016. Compare: LISTing 10M objects = 10,000 LIST ops ≈ $0.05 *and* is
  serial.
- A bucket is an immutable sorted run per epoch; the root names the current epoch per bucket.
- `bucket_digests[]` lets a reader skip buckets that haven't changed since its last read
  (conditional GET / digest compare) — so incremental enumeration is nearly free.
- Buckets split by doubling `num_buckets` when a bucket exceeds a size threshold; the root
  CAS publishes the new width. Old readers see the old width and are simply stale, never
  wrong.

### Catalog write amplification
An index creation must not rewrite a whole catalog bucket. So catalog updates are
**batched and asynchronous**:
- Creation is *immediately* effective via HEAD (hot path), and only *eventually* visible in
  the catalog.
- A background **catalog folder** (any node, elected by nobody — optimistic + CAS) drains a
  per-bucket change log (`{h}/cat/b/{bucket}/log/{lane}/{seq}` — lanes again) into a new
  sorted run every N seconds.
- Enumeration reads `run + unmerged log tail`, so it is never stale beyond the log tail.

This is the same shape as the main storage engine (immutable runs + lanes + CAS'd pointer),
which is deliberate: **one mechanism, used three times** (data, catalog, cluster state).

## 3. What about tenants, quotas, billing?

Per-index config, quotas, and usage counters live **inside HEAD** (config) and in
**per-index counter objects** written by lanes (usage). Aggregation for billing is a
periodic map-reduce over the catalog buckets — a batch job, not an online service. There is
no global counter to contend on.

## 4. Multi-tenancy economics

Because an idle index is *just a few objects at rest*, its marginal cost is:
- storage: bytes × $0.023/GB-mo,
- compute: **zero** (no process holds it open),
- memory: **zero**,
- metadata service: **zero** (there isn't one).

This is what makes millions of indexes viable and is structurally impossible for
RAM-resident systems. S3 Vectors caps at 10,000 indexes per bucket; we have no equivalent
limit because we never enumerate.

## 5. Hazards

| Hazard | Mitigation |
|---|---|
| Hot prefix from a naming convention (e.g. all ids start `tenant-`) | We hash the id for the prefix, so caller naming can't create hotspots. |
| Catalog root CAS contention | Root changes only on bucket-width change (rare). Bucket epochs change via per-bucket CAS — 4096 independent registers. |
| Index id enumeration as a security issue | Ids are opaque; the catalog is internal-only. Public API never exposes cross-tenant enumeration. |
| Catalog loss | Catalog is *derived* state, not source of truth. It can be rebuilt by the one sanctioned full LIST (DR path). Losing it degrades admin, never serving. |
| Bucket skew | Hash-based assignment; monitor and double width. |

## 6. Cost summary

| Operation | RA |
|---|---|
| Open / write / query an index | **0 catalog requests** |
| Create index (idempotent) | 1 W |
| Full enumeration of N indexes | `num_buckets` Rpar (1 round) |
| Incremental enumeration | ≤ changed-bucket count, Rpar |
| Billing rollup | 1 full enumeration per period |

## Open questions raised

- OQ-8: Right value of `num_buckets` and the split threshold; depends on descriptor size and
  the real index-count distribution.
- OQ-9: Should the catalog be per-region/per-bucket or global? Leaning per-bucket
  (a `pstore` deployment == a blob bucket + a prefix), which keeps BYOC clean.

## Sources

- [Quickwit 101 — metastore as a JSON file on object storage](https://quickwit.io/blog/quickwit-101)
- [Amazon S3 Vectors GA — 10,000 indexes per vector bucket](https://aws.amazon.com/about-aws/whats-new/2025/12/amazon-s3-vectors-generally-available/)
- [Best practices design patterns: optimizing Amazon S3 performance — AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html)
- [turbopuffer — Concepts: namespace = prefix on object storage](https://turbopuffer.com/docs/concepts)
