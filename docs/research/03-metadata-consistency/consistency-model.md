# Consistency Model

**Answers:** Q10
**Status:** Complete (v1)

## 1. What we promise

| Guarantee | Level |
|---|---|
| **Durability** | A `durable` write is on the blob store (11 nines, multi-AZ) before ack. |
| **Isolation** | **Snapshot isolation** for reads. A query executes entirely against one immutable epoch + a bounded WAL suffix. |
| **Atomicity** | Per write-batch. A batch is visible entirely or not at all. **No cross-batch or cross-index transactions.** |
| **Ordering** | Total order per `(index, shard)`, established by `(lane_seq, lane_id)`. **No global order across shards.** |
| **Read-your-writes** | Guaranteed in `strong` mode; opt-out per query. |
| **Monotonic reads** | Guaranteed per session via a client-carried `read_token`. |

We deliberately do **not** promise: multi-document transactions, cross-index consistency, or
a global total order. Search workloads don't need them, and they are exactly what would force
a coordinator back into the design.

## 2. Three read modes

The 10 ms floor turbopuffer describes for strong consistency is the cost of *checking the
blob store*. We expose that cost as a choice.

### `strong` (default)
Read HEAD (conditional GET, usually a 304), then probe the lane tail. Sees every
acknowledged write.
- **Cost:** 1–2 Rseq. **Latency floor:** ~1 blob RTT (~10–30 ms).

### `bounded(max_staleness_ms)`
Serve from the cached epoch if it was validated within `max_staleness_ms`; otherwise
revalidate. The client names its own tolerance.
- **Cost:** 0 requests on the fast path. **Latency floor:** cache speed (~1 ms).
- This is strictly better than turbopuffer's eventual mode, whose worst-case staleness is
  *"up to about one hour"* and is not client-controllable.

### `at_token(read_token)`
The client passes the opaque token returned by its last write. The node serves at ≥ that
epoch/watermark, blocking or refreshing if behind. Gives **read-your-writes and monotonic
reads at bounded-mode cost**, because the common case is that the cached view is already
past the token.

> **Design rule 10.** Freshness is a *client-specified* parameter with an explicit bound,
> never a system-chosen mystery. Every response carries the epoch and staleness it was served
> at.

## 3. How readers stay fresh without a stampede

10,000 nodes polling HEAD naively would be a self-inflicted DDoS. Three mechanisms:

1. **Conditional GET.** `If-None-Match: <cached_etag>` → **304 Not Modified**: full read
   price, zero bytes, minimal latency. Freshness checks are cheap.
2. **Jittered, demand-driven revalidation.** Only nodes actually serving an index poll it,
   at a rate derived from that index's observed query rate, with per-node jitter. A cold
   index is polled by nobody.
3. **Piggybacked epoch hints.** A node that observes a new epoch gossips it (see
   `04-cluster/membership.md`). Most nodes learn about a change without polling. Gossip is a
   *hint* only — never trusted for correctness, only used to trigger a revalidation early.

**Worst case cost:** an index served by *k* nodes at *q* QPS revalidates at most
`min(k × poll_rate, ...)` reads/s — bounded, and each is a 304.

## 4. Write visibility

```
t0  client sends batch
t1  node buffers it, assigns (lane, seq)
t2  node PUTs the lane object            <- durable; ack for `durable` mode
t3  readers probing the lane tail see it <- visible for `strong` reads
t4  indexer folds it into a segment      <- visible for all reads, faster
t5  structural CAS commit publishes it   <- epoch advances
```

- **Visibility actually happens at t1, not t3** — see
  [`../05-storage-engine/batching-and-visibility.md`](../05-storage-engine/batching-and-visibility.md)
  §5. Writes are routed to the index's read placements and held in an R-way in-memory
  memtable, so every node that can answer a query for the index has the record ~1 ms after
  arrival, independent of when the PUT lands. The t2–t5 sequence below describes durability
  and query *efficiency*, not freshness.
- Between t2 and t5, data is **visible but unindexed**: strong-mode queries must scan the
  memtable (and any un-folded WAL) as well as the indexed segments. We cap the unindexed volume (turbopuffer caps at
  128 MiB; we do the same, adaptively) and apply back-pressure beyond it.
- `bounded` reads may serve from t4 or even t3 state.

## 5. What the blob store gives and does not give

| Property | Status |
|---|---|
| Read-after-write for new objects | ✅ strongly consistent on S3, GCS, Azure |
| Read-after-overwrite | ✅ strongly consistent on S3 since Dec 2020 |
| **List-after-write** | ✅ consistent on S3, but we don't use LIST |
| **Linearizable CAS** | ✅ on all three |
| Cross-object atomicity | ❌ — hence the single-pointer manifest design |
| Cross-region consistency | ❌ — a `pstore` deployment is single-region; multi-region is async replication of immutable objects + HEAD, **eventually consistent, explicitly** |

## 6. Failure behaviour

| Failure | Effect |
|---|---|
| Node dies mid-batch (pre-PUT) | Client sees an error; retries are safe because batches carry a client-supplied idempotency key deduped at fold time. |
| Node dies post-PUT, pre-ack | Data is durable and will be visible. Client retry deduplicates. **At-least-once, deduplicated to effectively-once.** |
| Node pauses for 5 minutes then wakes | Harmless. Its CAS fails (fencing, `manifest-and-cas.md` §2); its lane writes are at already-consumed sequence numbers and are ignored or deduped. |
| Blob store partial outage / 503 storm | Reads degrade to cache (`bounded` mode still serves); writes fail closed. We never accept a write we cannot make durable in `durable` mode. |
| Two nodes think they own the same shard | Safe by construction — both may do the work, one wins the CAS. |

## 7. The one thing we must get right

Every correctness argument above reduces to a single invariant:

> **Invariant I1.** No node ever mutates an object another node might read, and every state
> transition is a CAS on a single key conditioned on the exact version the actor observed.

If a design ever needs to break I1, it is the wrong design.

## Open questions raised

- OQ-10: Right cap for unindexed WAL volume before back-pressure — trades write availability
  against strong-read latency.
- OQ-11: Should `read_token` be a vector (per-shard watermarks) or a scalar epoch? Vector is
  more precise, larger, and leaks internal structure.

## Sources

- [turbopuffer — Concepts: strong vs eventual consistency](https://turbopuffer.com/docs/concepts)
- [turbopuffer — Architecture](https://turbopuffer.com/docs/architecture)
- [Add preconditions to S3 operations with conditional requests — AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-requests.html)
- [How to do distributed locking — Martin Kleppmann](https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html)
