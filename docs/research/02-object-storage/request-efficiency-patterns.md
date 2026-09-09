# Request-Efficiency Patterns — the design language of pstore

**Answers:** Q6
**Status:** Complete (v1)

This document turns the economics of `cost-and-latency.md` into reusable patterns. Every
subsystem design must state which of these it uses and what its request amplification is.

## The currency: request amplification

> **RA(op) = blob requests issued per logical operation, split by class (write / read /
> list) and by whether they are sequential (latency-additive) or parallel (latency-free).**

We write it as `RA = 1W + 3Rseq + 40Rpar`. Only `Rseq` costs latency; only `W` and `List`
cost real money.

---

## Pattern 1: Group commit (kills write amplification)

**Problem:** one PUT per user write costs $5/million and rate-limits at 3,500/s/prefix.

**Pattern:** a node accumulates incoming writes in memory for a bounded window
(`max_delay`, e.g. 50–200 ms) or until a size threshold (e.g. 8–64 MB), then issues **one**
PUT containing all of them. Durability is acknowledged when that PUT returns.

**Result:** `RA(write) = 1W / batch`. At 10,000 writes/batch that is 0.0001 PUT per write.

**Tradeoff:** write latency floor = `max_delay + PUT latency`. This is the single knob that
trades dollars for p99 write latency. Offer it as a per-request durability mode:
- `durable` — wait for the PUT (default, ~50–250 ms)
- `batched` — ack on in-memory accept + replication to k peers, PUT follows (~ms, weaker)

**Prior art:** WarpStream and SlateDB both do exactly this; SlateDB explicitly "batches
writes to mitigate high write API costs (PUTs)". It is the defining move of zero-disk
architectures.

## Pattern 2: Range coalescing (converts round trips into bytes)

**Problem:** reading 40 scattered postings inside one 200 MB object = 40 sequential-ish
GETs.

**Pattern:** given a set of desired byte ranges, merge any two whose gap `< G*`, then issue
the merged ranges **in parallel**.

`G*` is where the cost of transferring the gap equals the cost of an extra request:
```
G* ≈ min( bandwidth_per_conn × latency_saved ,  bytes_worth_one_GET )
```
With ~50 MB/s per connection and ~30 ms saved, `G* ≈ 1.5 MB`. In practice we tune per
backend and clamp to a policy range (e.g. 64 KB – 4 MB) to avoid pathological read
amplification on tiny objects.

**Result:** `RA(read)` collapses from `40Rseq` to `~4Rpar`, and wall clock from 1.2 s to
~40 ms.

## Pattern 3: Deterministic keys (abolishes LIST)

**Problem:** discovery requires enumeration; enumeration requires LIST; LIST is expensive,
serial, and semantically wrong.

**Pattern:** every object key is a **pure function** of facts the reader already holds:
```
{prefix_hash}/idx/{index_id}/{epoch}/{kind}/{shard}/{seq}.{ext}
```
The manifest supplies `epoch` and the set of live `(kind, shard, seq)` triples. Nothing is
discovered; everything is derived.

**Corollary — the root pointer problem.** Exactly one key in the system cannot be derived:
the index's manifest head. That key is derived from the *index id*, which the client
supplies. So even the root is computed, not listed. See `03-metadata-consistency/`.

**Result:** `RA(open index) = 1–2 Rseq, 0 List`.

## Pattern 4: Immutable content + one CAS pointer

**Problem:** mutating data in place on a blob store is impossible to do atomically across
multiple objects.

**Pattern:** all data objects are immutable and written once, named by `(epoch, seq)` or by
content hash. State transitions are published by a single conditional PUT that swaps the
manifest pointer. Readers see either the old world or the new world, never a mixture.

**Result:** snapshot isolation for free, time travel for free, branching for free (two
pointers to the same immutable set), and GC becomes a refcount problem instead of a
correctness problem.

## Pattern 5: Superset fetch (spend bytes to save trips)

Given free intra-region bandwidth and 12.5:1 write:read pricing, when in doubt **fetch
more**. Examples:
- Read the whole footer + index block of a data object, not just the needed entry.
- Read an entire posting block, not the exact byte span.
- On cold open, fetch the manifest *and* the index metadata block in one ranged GET by
  co-locating them in one object.

The failure mode of this pattern is bandwidth saturation on the node, not cost. Cap it by
NIC budget, not by dollars.

> **⚠️ Corrected (M-1) — see [`../09-rust-stack/memory-management.md`](../09-rust-stack/memory-management.md)
> §3.** The real cap is **memory**, not NIC. Fan-out × block size × query concurrency is the
> dominant OOM source: 32 lists × 3 MB × 4 shards = 384 MB for a single query if fetched bytes
> are accumulated. Fetch width is bounded by an **in-flight byte reservation**, and blocks are
> decoded, scored, and dropped so peak is O(resident), not O(fetched). Bandwidth is free;
> buffers are not.

## Pattern 6: Co-location / stapling (turn 2 objects into 1)

If A is always read with B, they must be in the same object, adjacent, so one ranged GET
gets both. Applied recursively this is why our data objects carry a self-describing footer:
`[data blocks][index blocks][bloom/zone maps][footer]`, with the footer at a known offset
from the end so a single `Range: -N` suffix GET bootstraps everything.

**Result:** cold open of a data object = **1 GET** (suffix) + **1 GET** (the actual block),
= 2 `Rseq`, never more.

> **C-14 — the suffix GET does not reach Azure, and Azurite answers it with a 500. M7a.**
> `Range: bytes=-N` is the primitive this pattern rests on, and
> [`api-semantics.md`](api-semantics.md) calls it "supported by S3, GCS and Azure alike".
> Three separate observations, kept apart because they say different things:
>
> 1. **Measured, client.** `object_store` 0.14.1 refuses a suffix range **before building a
>    request** (`src/azure/client.rs:1176`, comment dated 2024-01-02), so the conformance
>    suite's `suffix_read` row for Azurite is the *client's* refusal and would read the same
>    with the emulator stopped. ⚠️ **The first version of this banner presented that row as a
>    fact about the service** — in the milestone whose thesis is measured-not-declared. Caught
>    in code review.
> 2. **Measured, emulator.** `scripts/conformance.sh --azure-suffix` asks Azurite 3.34.0
>    directly, past the client: `bytes=0-99 -> 206`, `bytes=-1 -> **500**`. So the primitive is
>    absent there too, and absent *badly* — a 500 is not a refusal a client can act on. It is a
>    named mode of the script rather than a shell history, because this banner rests on it and
>    the conformance suite structurally cannot make this measurement.
> 3. **Documented, service.** The Blob REST API specifies `bytes=startByte-endByte`; there is
>    no suffix form. Not measured here — no Azure account (M0b).
>
> ⚠️ **The consequence holds on all three readings**, because our adapter goes through
> `object_store`: on Azure a cold open becomes `head` → range → block, so **3 `Rseq`, not 2**,
> and `head` is billed as a read — the cost
> [Pattern 7](#pattern-7-negative-caching-and-existence-proofs) forbids on a hot path. The ways
> out are a footer at a *known absolute* offset (a format change), carrying object length in
> the manifest (which already names every segment), or accepting 3 on Azure alone.
>
> **Not fixed here.** M7a measured it; choosing among those three is a design decision with its
> own spec. Evidence: [`docs/profiles/capability-matrix.md`](../../profiles/capability-matrix.md),
> [`M7a/VERIFIED.md`](../../milestones/M7a/VERIFIED.md).

## Pattern 7: Negative caching and existence proofs

Never HEAD to check existence on the hot path — the manifest already proves what exists.
HEAD is reserved for GC and repair. Where existence must be probed (e.g. optional sidecar
objects), record its presence in the manifest instead.

## Pattern 8: Capability-adaptive policy

Read `Capabilities` (see `api-semantics.md` §6) and change *policy*, not code paths:
- `delete_is_free` → aggressive immediate GC (S3) vs. batched weekly GC (GCS/Azure).
- `low_latency_tier_available` → put the WAL on S3 Express, data on Standard.
- `max_batch_delete` → sizing of the reaper.

## Pattern 9: Tiered WAL (hot small writes ≠ cold big data)

Small, latency-sensitive, short-lived objects (the WAL) and large, long-lived objects (the
compacted index) have opposite cost profiles. Split them across storage classes:
- WAL → S3 Express One Zone (cheap GETs, single-digit ms, storage cost irrelevant because
  TTL is minutes).
- Data → S3 Standard (cheap storage, cross-AZ durability).

The WAL's 37.7:1 W:R ratio is fine because group commit already made W rare, and its GETs
(tail-following by other nodes) are 13× cheaper than Standard.

> **⚠️ Corrected — see [`../05-storage-engine/batching-and-visibility.md`](../05-storage-engine/batching-and-visibility.md)
> §8 (C-1).** Express One Zone is single-AZ, so multi-AZ durability costs ~3 copies, and it
> bills per-GB on all uploaded bytes. At bundle sizes it is ~16× *more* expensive than
> Standard. Treat this pattern as a **latency** tool, priced accordingly — not a cost
> reduction.

## Pattern 10: Amortized metadata (one CAS per many commits)

A CAS PUT per write batch would double our write cost. Instead:
- WAL objects are written at deterministic monotonic sequence numbers, so a reader can
  discover the tail by **probing forward** with cheap GETs rather than reading a pointer.
- The manifest is only CAS'd on *structural* changes (compaction, schema, index build), not
  on every batch.

Tail probing: reader knows the last seen seq `n`; issues parallel GETs for `n+1..n+k`; the
first 404 bounds the tail. Cost: `k` cheap reads, 0 writes, 0 lists. This is the trick that
lets us have a durable log with no coordinator and no listing.

## Pattern 11: Bundle across tenants (breaks the per-stream PUT floor)

Group commit (Pattern 1) removes write amplification *within* one stream. It does nothing
about the floor you pay for **having** a stream: a per-index timer at interval `T` costs
`2,592,000/T` PUTs per index per month whether the index writes one document or a billion.
At 1M indexes and `T = 60 s` that is **$216,000/month of pure liveness**.

The fix is to make the flush unit the **node**, not the tenant: one object per window carrying
records for every index the node buffered, sorted by `(index_id, shard)` with a footer index.
PUT cost then scales with **node count and write volume**, not tenant count.

Discovery is preserved by routing writes through the same placement function that routes
reads, so a reader derives the small set of node lanes that could hold its data — still zero
LIST. Full design, cost tables, and the visibility argument:
[`../05-storage-engine/batching-and-visibility.md`](../05-storage-engine/batching-and-visibility.md).

## Pattern 12: Serve freshness from memory, not from storage

Bundling only stays latency-free if visibility does not wait for the PUT. Route writes to an
index's read placements, hold them in an R-way in-memory-replicated memtable, and scan that
memtable exactly at query time. Visibility then costs ~1 ms **regardless of the flush
interval**, and the batching dial moves onto the durable-ack axis where clients can choose it.

> **Corollary.** Any design in which the flush interval appears in the time-to-searchable
> budget has conflated durability with visibility, and will be forced to choose between cost
> and freshness. Separate them and the choice disappears.

---

## Anti-patterns (explicitly banned)

| Anti-pattern | Why banned |
|---|---|
| LIST on read/write/startup path | Price, serialization, scale ceiling |
| One object per row/document | 10⁶× cost blowup |
| Graph index traversal against cold storage | Unbounded sequential round trips |
| HEAD-before-GET | Doubles read count for zero information the manifest lacks |
| Read-modify-write of a large object | Bandwidth + lost-update hazard |
| Per-node discovery by enumeration | O(fleet × dataset) |
| **Per-index time-driven flush** | **$216k–$13M/month floor at 1M indexes, independent of data volume** |
| Tuning the flush interval for search freshness | Conflates durability with visibility; use a memtable (Pattern 12) |
| Multipart upload for ordinary batches | Every `UploadPart` is a PUT-class request; a single PUT covers up to 5 GiB |
| Depending on Azure leases or append blobs | Not portable |
| Parsing ETags as content hashes | Wrong under MPU/KMS |

## Checklist for every subsequent design doc

1. State `RA` for each operation, split W/Rseq/Rpar/List.
2. State the sequential round-trip depth (must be ≤3 on any user-facing cold path).
3. State which of the 10 patterns are used.
4. State behaviour under `503 SlowDown` and 412/409.
5. State GC/refcount story for anything newly written.

## Sources

- [SlateDB: An Object-Native LSM for Online Systems](https://slatedb.io/blog/introducing-slatedb/)
- [SlateDB: An Embedded Storage Engine Built on Object Storage — Materialized View](https://materializedview.io/p/slatedb-an-embedded-storage-engine)
- [Zero-Disk Architecture: The Future of Cloud Storage Systems — pracdata](https://www.pracdata.io/p/zero-disk-architecture-the-future)
- [turbopuffer: fast search on object storage](https://turbopuffer.com/blog/turbopuffer)
- [Best practices design patterns: optimizing Amazon S3 performance — AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html)
