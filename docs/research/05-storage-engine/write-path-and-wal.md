# The Write Path: Lanes, Group Commit, and Zero CAS

**Answers:** Q15
**Status:** Complete (v1) — **partially superseded by
[`batching-and-visibility.md`](batching-and-visibility.md)**, which shows that per-index
batching has a cost floor of $216k–$13M/month at 1M indexes regardless of batch size, and
moves the WAL unit from per-`(index, shard)` to per-**node** cross-index bundles. Read that
document with this one; §3's lane bitmap survives as the fallback-discovery mechanism, and
§5's Express One Zone recommendation is corrected there (C-1).

This is the design's biggest departure from turbopuffer and the thing that makes 10K nodes
useful rather than decorative.

## 1. The constraint stack

1. A PUT costs 12.5 GETs ⇒ **batch**.
2. CAS on one key tops out at ~5 writes/s ⇒ **keep data writes off CAS**.
3. turbopuffer's published ceiling is **1 WAL entry/second per namespace** and
   ~10,000 vectors/s — a direct consequence of one serialized commit point per namespace.
4. 10,000 stateless nodes must be able to write to the *same* index concurrently, or the
   fleet size is pointless.

## 2. The core idea: lanes

> **A lane is a single-writer, append-only sequence of immutable WAL objects, owned by one
> node.** Multiple lanes exist concurrently. Ordering is established at *read* time by merging
> lanes, not at write time by serializing writers.

> **Revision (see [`batching-and-visibility.md`](batching-and-visibility.md) §4).** A lane
> object is a **cross-index bundle** — one object carrying records for every index and shard
> the node buffered in that window, sorted by `(index_id, shard)` with a footer index. The
> per-`(index, shard)` keying shown below is retained only for the fallback path, where a
> writer that cannot reach an index's placements writes a dedicated object and registers
> itself in the lane bitmap. Bundling is what removes the per-index PUT floor; the lane
> concept is otherwise unchanged.

```
{h}/idx/{id}/s{shard}/wal/{lane_id}/{seq:016}.wal
```

- `lane_id` is a random 64-bit value chosen by the writing node when it starts writing to
  that shard. Collisions are negligible; a collision is also harmless because the object
  key includes `seq` and writes are `If-None-Match` conditional (a colliding writer gets a
  412 and picks a new lane).
- `seq` is a per-lane monotonic counter, starting at 0.
- Each object is written **once**, with `If-None-Match: *`. Never overwritten.

**Consequences:**
- N concurrent writers ⇒ N lanes ⇒ **zero contention, zero CAS, zero coordination on the
  write path**.
- Per-index write throughput is bounded only by the blob store's per-prefix rate limit, and
  lanes naturally spread across prefixes.
- `RA(write batch) = 1 W`. With group commit at 10k docs/batch, that is 10⁻⁴ PUT per
  document.

### Why this is safe
Nothing about ordering is lost. Search has no cross-document transactions, so the only thing
we need is:
- **per-document ordering** — guaranteed, because all writes for a doc id go to the same
  *shard*, and the merge rule resolves duplicates by `(commit_time, lane_id, seq)` with a
  deterministic tie-break;
- **atomicity of a batch** — guaranteed, because a batch is one object;
- **durability** — guaranteed by the PUT.

We give up a global total order across lanes. That is the trade, and it is the right one.

## 3. Discovering the tail without LIST or CAS

The reader problem: given lanes it has never seen, how does it know what exists?

**Two-part answer:**

### (a) Known lanes: forward probing
For a known lane at `seq = n`, issue **parallel GETs** for `n+1 … n+k`. The first 404 bounds
the tail. Cost: `k` cheap reads (Rpar, one round trip), 0 writes, 0 LISTs. `k` adapts to the
observed write rate.

### (b) New lanes: the lane directory
A writer that opens a *new* lane advertises it once:

```
PUT {h}/idx/{id}/s{shard}/lanes/{lane_id}   If-None-Match: *   (~100 B, written once per lane)
```

But that still needs enumeration to read. So instead the lane set is **folded into HEAD** by
the indexer (which is running anyway) and, in between folds, discovered via:
- **gossip** — the writing node advertises `(index, shard, lane_id)` in its gossip payload;
  readers pick it up in ~1 s. This is a *hint*, so gossip is allowed to be lossy;
- **the strong-read fallback** — a `strong` read that must not miss data reads the small,
  deterministic **lane bitmap**: a fixed-width object per shard where lane registration flips
  a bit at a derived offset.

### The lane bitmap (the neat trick)
```
{h}/idx/{id}/s{shard}/lanemap/{gen}      // fixed 8 KiB = 65,536 bits
```
`lane_slot = hash(lane_id) % 65536`. A writer claims a slot by CAS'ing the bitmap **once per
lane lifetime** (not per write). A reader gets the complete set of possibly-live lanes in
**one 8 KiB GET**, no enumeration.

- Cost: 1 CAS per lane creation — bounded by writer churn, not write volume. With 10K nodes
  each opening a lane per shard per hour, that is ~3 CAS/s cluster-wide across all shards,
  spread over millions of independent bitmap keys.
- Slot collisions are harmless (they cause a reader to probe a lane id it derives from the
  live set in HEAD; unknown-but-set slots trigger a fallback probe).
- The bitmap is reset at each structural commit, since folded lanes are no longer needed.

> This replaces "one serialized WAL per namespace" with "an unbounded set of parallel lanes,
> summarized in 8 KiB." It is the central mechanism of the design.

## 4. Group commit

Within a node, writes to the same `(index, shard)` accumulate until:
- `max_bytes` (default 8 MiB, tunable to 64 MiB), or
- `max_delay` (default 50 ms in `durable` mode), or
- an explicit flush.

Then: one PUT. Batching is per-shard, so a node writing to 1,000 indexes issues 1,000 PUTs
per window — which is why `max_delay` scales with observed per-index write rate (a trickle
tenant waits longer; a firehose tenant flushes on bytes).

**Durability modes:**

| Mode | Ack when | Latency | Durability |
|---|---|---|---|
| `durable` (default) | WAL PUT returns | 50–250 ms | Blob-store durability |
| `batched` | Accepted into buffer + replicated to `f` peers in-memory | ~1–5 ms | Survives `f` node failures, not a fleet-wide outage |
| `async` | Accepted into buffer | <1 ms | None until flush |

`batched` is honest about what it is: a bounded-loss mode. Turbopuffer publishes p50 165 ms
for a 500 kB write; `batched` gets us to single-digit ms for clients who accept the trade.

## 5. Storage class routing

WAL objects are small, short-lived (minutes to hours), and read repeatedly by tail probes.
That profile matches **S3 Express One Zone** exactly: GETs are 13× cheaper than Standard
($0.00003 vs $0.0004 per 1,000), latency is single-digit ms, and the higher $0.11/GB-month
storage cost is irrelevant at a 1-hour TTL.

- WAL → Express/low-latency tier (where available).
- Segments → Standard.
- **Caveat:** Express is single-AZ. `durable` mode requires multi-AZ durability, so either
  (a) dual-write WAL to Express (for latency) and Standard (for durability), or (b) offer
  Express-only WAL as a documented single-AZ durability tier.

> **⚠️ Corrected — see [`batching-and-visibility.md`](batching-and-visibility.md) §8 (C-1).**
> The original recommendation of (a)-by-default understated the cost. Multi-AZ durability on
> Express needs ~3 copies (WarpStream writes to three Express buckets for quorum), and since
> April 2025 Express bills **$0.0032/GB on all uploaded bytes**. For an 8 MiB bundle that is
> **≈16× more expensive than a single Standard PUT**, dominated by per-GB transfer rather
> than requests. **Express is a latency purchase, not a cost saving**, and it gets worse as
> bundles grow. Default to Standard; offer Express as a priced opt-in for single-digit-ms
> *durable* acks. Most workloads will not need it, because the freshness layer already
> delivers ~1 ms *visibility*.

## 6. Backpressure

Unindexed WAL volume per shard is capped (default 128 MiB, matching turbopuffer's published
number). Beyond the cap:
1. Raise indexing priority for that shard (deterministic assignment already picked a node).
2. Slow `durable` acks (increase `max_delay`).
3. Reject with `429` + `retry_after`.

Never accept unbounded unindexed data: it directly degrades strong-read latency, because
strong reads must scan it.

## 7. Cost summary

| Operation | RA |
|---|---|
| Write batch (any size) | **1 W** (2 W with dual-tier durability) |
| Lane creation | 1 CAS, once per lane lifetime |
| Tail probe (reader) | `k` Rpar, 1 round trip |
| Lane discovery (reader) | 1 Rseq (8 KiB bitmap) or 0 (gossip/HEAD) |
| **CAS operations per write** | **0** |

## 8. Comparison

| | turbopuffer | pstore |
|---|---|---|
| WAL structure | one shared WAL per namespace | N parallel lanes per shard |
| Commit rate ceiling | ~1/s per namespace | blob rate limit per prefix |
| Coordination on write | group commit at a single point (broker) | none |
| Write throughput/index | ~10k vectors/s | target ≥1M vectors/s |
| Tail discovery | (internal) | forward probe + 8 KiB lane bitmap |

## Open questions raised

- OQ-22: Does forward probing with `k` parallel GETs cost more than it saves vs. a small
  per-lane "tail pointer" object updated every M writes? Model both.
- OQ-23: Lane bitmap size and reset policy under adversarial writer churn.
- OQ-24: Verify Express One Zone durability semantics and the dual-write cost at real batch
  sizes.
- OQ-25: Deterministic tie-break for concurrent updates to the same doc id across lanes —
  is `(commit_time, lane_id, seq)` acceptable, or do we need client-supplied versions? Leaning
  **client-supplied optional version** for last-write-wins correctness.

## Sources

- [turbopuffer — Architecture (WAL, group commit, 1 entry/s/namespace, 128 MiB unindexed)](https://turbopuffer.com/docs/architecture)
- [turbopuffer — Concepts](https://turbopuffer.com/docs/concepts)
- [How to build a distributed queue in a single JSON file on object storage — turbopuffer](https://turbopuffer.com/blog/object-storage-queue)
- [SlateDB: An Object-Native LSM for Online Systems](https://slatedb.io/blog/introducing-slatedb/)
- [Zero Disks is Better (for Kafka) — WarpStream](https://www.warpstream.com/blog/zero-disks-is-better-for-kafka)
- [Announcing up to 85% price reductions for Amazon S3 Express One Zone — AWS](https://aws.amazon.com/blogs/aws/up-to-85-price-reductions-for-amazon-s3-express-one-zone/)
- [Best practices design patterns: optimizing Amazon S3 performance — AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html)
