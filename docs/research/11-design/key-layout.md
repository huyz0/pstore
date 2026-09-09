# Blob Key Layout

**Synthesizes:** D33
**Status:** v1 proposal

The key layout is where four separate design rules physically meet: no LIST (rule 4), entropy
at the front (rule 5), one mutable pointer (rule 1), and everything else immutable (rule 2).

## Naming rules

1. **Entropy first.** `{hash4}` = first 4 base32 chars of `xxh3_64(index_id)` ≈ 1M distinct
   prefixes. Gives every index its own S3 partition lineage from its first write, spreads a
   10K-node fleet's request load, and makes caller naming conventions irrelevant.
2. **Every key is derivable** from `(index_id, epoch, shard, kind, seq)` — all of which come
   from the manifest or the request. Nothing is discovered.
3. **Fixed-width, zero-padded, lexicographically sortable** numerics (`{epoch:020}`,
   `{seq:016}`) so ordering is byte-order and range-derivation is arithmetic.
4. **Exactly one mutable key per index** (`HEAD`), plus a small number of CAS'd registers
   (`branches`, `lanemap`, catalog roots).
5. **Kind is in the path**, so GC, lifecycle policies, storage-class routing, and metrics can
   all be expressed as prefix rules.

## The tree

> **Revision.** The tree below is keyed by index. Per
> [`../10-benchmarks-cost/tenancy-scale-model.md`](../10-benchmarks-cost/tenancy-scale-model.md)
> §4 it is now keyed by **tenant**: `{hash4}/tnt/{tenant_id}/HEAD` is the CAS register and
> lists all ~50 of that tenant's indexes (inlining the small ones); per-index subtrees hang
> beneath it, and a hot index may be promoted to its own HEAD. `{hash4}` is computed from
> `tenant_id`. The WAL moves to a **cohort lane** — `{hash4}/wal/{cohort_node}/{seq}.bundle`,
> derived via `LRH(tenant_id)` over a ring of `W` writer nodes (§3 there). Structure,
> derivability, and the mutable-object census are otherwise unchanged.

```
{root}/                                              ← deployment prefix (BYOC-friendly)
│
├── {hash4}/idx/{index_id}/
│   ├── HEAD                                         MUTABLE · CAS · ~1–4 KB
│   │      { epoch, manifest_ref{base,deltas[]}, schema_ref,
│   │        shard_map, lane_watermarks[], commit_nonce, stats_digest }
│   │
│   ├── branches                                     MUTABLE · CAS · fork refcounts
│   ├── m/{epoch:020}.manifest                       immutable · segment list
│   ├── m/{epoch:020}.delta                          immutable · incremental manifest
│   ├── schema/{schema_epoch:020}.json               immutable
│   │
│   └── s{shard:05}/
│       ├── HEAD                                     MUTABLE · CAS · (large indexes only)
│       ├── wal/{lane_id:016x}/{seq:016}.wal         immutable · write-once · NO CAS
│       ├── lanemap/{gen:020}                        MUTABLE · CAS · 8 KiB bitmap
│       ├── seg/L{level}/{epoch:020}-{ulid}.seg      immutable · 256 MiB–4 GiB
│       ├── dv/{segment_ulid}/{epoch:020}.dv         immutable · roaring delete vector
│       └── claims/{work_hash:016x}                  advisory only · TTL
│
├── {hash4}/cat/
│   ├── root                                         MUTABLE · CAS · {epoch, num_buckets, digests[]}
│   ├── b/{bucket:04}/{epoch:020}                    immutable · sorted run of index descriptors
│   └── b/{bucket:04}/log/{lane}/{seq:016}           immutable · pending changes
│
└── {hash4}/clu/
    ├── ROSTER                                       MUTABLE · CAS · gossip seed
    └── roster/{gen:020}                             immutable snapshot
```

> **C-12 — the catalog subtree, corrected. M6a.** The `cat/` lines above and the census row
> below are superseded by
> [`../03-metadata-consistency/catalog-without-master.md`](../03-metadata-consistency/catalog-without-master.md)'s
> C-12. There is **no** `log/{lane}/{seq}` subtree: pending changes live inside the bucket's
> own CAS'd pointer, so the shape is `{bucket:04x}/cat/b/HEAD` (MUTABLE · CAS ·
> `{run_epoch, digest, pending[]}`) and `{bucket:04x}/cat/b/{run_epoch:020}-{digest:016x}`
> (immutable run, **named by content digest as well as epoch** so two folders racing on one
> bucket cannot publish different runs at the same key). `cat/root` carries `{epoch, width}`
> only and changes on a width change, as the census says; the census's *other* claim — that the
> per-bucket pointer changes at fold rate, ~1/min — is what C-12 relaxes: it now takes every
> append as well, at tenant-lifecycle rate. `{bucket:04x}` is fixed-width to 65,536 buckets.
> Evidence: [`../../milestones/M6a/SPEC.md`](../../milestones/M6a/SPEC.md).

## Mutable-object census

The whole system's mutable state, enumerated:

| Key | CAS rate | Purpose |
|---|---|---|
| `idx/{id}/HEAD` | structural commits only (~0.1–5/s) | The index's committed epoch |
| `idx/{id}/s{shard}/HEAD` | per-shard, large indexes | Partitioned register — beats the 5/s ceiling |
| `idx/{id}/branches` | branch create/delete (rare) | GC refcount roots |
| `idx/{id}/s{shard}/lanemap/{gen}` | once per lane lifetime | Lane discovery |
| `cat/root` | bucket-width change (very rare) | Catalog shape |
| `cat/b/{bucket}/…` epoch pointer | catalog folds (~1/min) | Enumeration |
| `clu/ROSTER` | ~1/min cluster-wide | Gossip seed |

**Seven kinds of mutable object in the entire system.** Everything else is written once and
never touched again. That is the property that makes the correctness argument tractable.

## Storage-class routing

Prefix-based, so it is expressible as bucket policy *and* as client-side routing:

| Prefix | Class | Rationale |
|---|---|---|
| `**/wal/**` | S3 Express One Zone (+ Standard dual-write for `durable`) | Small, short-lived, read-often, latency-critical; GETs 13× cheaper |
| `**/seg/**` | S3 Standard | Bulk, long-lived, cheap storage |
| `**/m/**`, `HEAD`, `lanemap` | S3 Standard (small, hot) | CAS support required |
| `**/dv/**` | S3 Standard | Small, frequently re-read |

Lifecycle rules can expire `wal/` after N hours — but only as a backstop; GC is manifest-driven
(rule: never let a lifecycle policy be load-bearing for correctness).

## Sharding the request load

At 10,000 nodes issuing ~10 blob requests/second each = 100,000 req/s aggregate:
- ~1M `{hash4}` prefixes ⇒ average ~0.1 req/s per prefix. Far under the 3,500 W / 5,500 R
  per-prefix limits.
- A single hot index still concentrates on one `{hash4}`, but its keys spread across
  `s{shard}/` and `seg/L{level}/` sub-prefixes, which S3 partitions independently once traffic
  justifies it.
- **Sub-prefix depth is deliberate:** it gives S3's adaptive partitioner something to split on
  before the index gets hot enough to need it.

## Multi-tenancy and BYOC

`{root}` is a deployment-level prefix, so a bucket can host one deployment or many, and a
customer's own bucket can be the `{root}` — the whole system works unchanged. There is no
control-plane state to move, because there is no control plane.

## Versioning

Every object begins with a magic number and a format version. Readers must handle N and N−1;
writers emit N. Format upgrades roll out as: deploy readers → flip writers → compact forward.
No coordinated migration, because segments are immutable and a mixed-version corpus is normal.

## Open questions raised

- OQ-78: 4 hex/base32 chars of prefix entropy — enough at millions of indexes, or should it
  scale with index count?
- OQ-79: Should `{hash4}` be a separate top-level component or embedded in the index id? A
  self-describing id (`{hash4}_{ulid}`) would make keys shorter and ids carry their own
  routing. Attractive; check it doesn't leak internals.
- OQ-80: Per-shard HEAD threshold — at what index size does partitioning the register pay for
  the extra read?
