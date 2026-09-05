# Background Work Without a Master: Optimistic Work + CAS-on-Publish

**Answers:** Q13
**Status:** Complete (v1)

## The problem

Some work must happen exactly-once-ish and is expensive: compaction, index building,
GC, catalog folding, roster folding. Traditionally this needs a scheduler and a lock service.
We have neither.

## The insight

From `manifest-and-cas.md` §2: **the CAS already provides fencing**, so duplicated work is
never *incorrect* — only *wasteful*. That reframes the entire problem:

> We do not need mutual exclusion. We need **duplicate-work suppression**, which is an
> economics problem, not a correctness problem.

Economics problems can be solved with hints, heuristics, and probability. Correctness
problems cannot.

## The pattern: optimistic work + CAS-on-publish

```
1. Any node independently decides work item W is needed
   (deterministic function of the manifest it already holds — no coordination).
2. It computes a deterministic claim key and does a soft claim (see below).
3. It performs the work, writing immutable output objects.
4. It CASes HEAD to publish.
5. Winner: work is committed. Loser: 412 -> discard output; GC reaps the orphans.
```

Properties:
- **No lock, no lease, no liveness detection required for correctness.**
- **No failover window.** If the working node dies at step 3, another node notices W is still
  needed (step 1 is deterministic and re-evaluated continuously) and redoes it. There is no
  timeout to tune and no stuck state.
- A paused-then-resumed node cannot corrupt anything (fencing).

The entire cost of masterlessness is *some wasted CPU and some orphan objects*. Both are
cheap; a control plane is not.

## Suppressing duplicate work (the economics half)

Three layers, cheapest first:

### 1. Deterministic assignment (free, no I/O)
Work item W for `(index, shard)` is assigned by the **same LRH function used for routing**.
The node that is placement #0 for that key is "the one that should do it". Every node can
compute this locally with zero communication. This alone removes ~all duplication in the
steady state.

### 2. Jittered deferral (free)
Non-primary placements do not start immediately — placement #1 waits `d`, placement #2 waits
`2d`, etc. If the primary is alive and working, it publishes before the others begin. If it
is dead, the backup starts after `d` with no detection logic. This is a **backup-timer
pattern**, not a failover: nobody decides anything.

### 3. Soft claims (one cheap write, only for expensive work)
For work above a cost threshold (e.g. compacting 10 GB), the node writes an advisory claim:

```
PUT {h}/idx/{id}/claims/{work_hash}  If-None-Match: *
    { node_id, started_at, expected_duration, heartbeat_epoch }
```

- 412 ⇒ someone else is on it ⇒ defer and re-check later.
- The claim has a TTL; an expired claim can be overwritten with `If-Match`.
- **A stale or wrong claim is harmless** — it can only cause duplicate or delayed work, never
  incorrect work. Clock skew is therefore acceptable, which is precisely what
  `manifest-and-cas.md` §5 says we require of leases.
- Cost: 1 PUT per expensive work item. Negligible relative to the work.

> **Design rule 12.** Claims are advisory. Any code path that would behave *incorrectly* if a
> claim were violated is a bug. Reviewers should treat "we hold the claim, so we can skip the
> CAS check" as a blocking defect.

## Work scheduling with no scheduler

Every node, on a timer, evaluates for each index it is a placement for:

```
needed_work(manifest) -> [WorkItem]   // pure function
```

Examples: "L0 has 12 segments, threshold is 8 ⇒ compact"; "shard 7 has 30% tombstones ⇒
rewrite"; "unindexed WAL is 200 MB ⇒ build segments"; "epoch 400 is unreferenced and older
than the retention window ⇒ GC".

Because `needed_work` is a **pure function of state the node already holds**, there is no
queue, no dispatcher, no scheduler state to lose, and nothing to recover after a crash. The
work list regenerates itself from the manifest, always. This is the single most important
simplification in the design.

Priority ordering is local (a node picks the highest-value item it is assigned), with global
fairness emerging from the fact that assignment is hash-uniform.

## Comparison with turbopuffer's broker

Their queue post funnels writes through a **stateless broker whose address is stored in
`queue.json`**, with heartbeats and CAS-driven failover. It works, and it is a sensible
answer to CAS contention. But it is a master: there is a failover window, an address to
publish, a liveness protocol, and a bottleneck.

We avoid needing a broker because lanes (see `05-storage-engine/write-path-and-wal.md`) remove
CAS from the write path entirely, so there is no contention left to amortize.

## Failure matrix

| Scenario | Outcome |
|---|---|
| Worker dies mid-compaction | Orphan objects; another node redoes it after its backup timer. No stuck state. |
| Two nodes compact the same shard | Both finish; one wins the CAS; loser discards. Wasted CPU only. |
| Node paused 10 min, resumes, publishes | CAS fails (fencing). Discards. Safe. |
| Claim written, node dies, clock skew | Claim expires late; work delayed, not lost. |
| All nodes for an index are down | Work resumes when any node picks the index up. Nothing is lost — state is in the blob store. |
| GC deletes an object a slow reader still wants | Prevented by **epoch retention**: objects are only reaped after `max_query_duration + safety` past dereference. See `05-storage-engine/mutations-and-mvcc.md`. |

## Cost summary

| Operation | RA |
|---|---|
| Deciding what work is needed | **0** (pure function of held state) |
| Soft claim | 1 W, only for expensive items |
| Publishing work | 1 R + 2 W (the manifest commit) |
| Duplicated work (steady state) | ~0 (deterministic assignment) |
| Duplicated work (during churn) | bounded by R (replication factor) |

## Open questions raised

- OQ-17: Backup-timer `d` should be ~p99 of the work duration. Needs measurement per work
  type.
- OQ-18: Is duplicate-work waste during a large scale-out event actually bounded? Model it.

## Sources

- [How to do distributed locking — Martin Kleppmann](https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html)
- [Locks, leases, fencing tokens, FizzBee! — Surfing Complexity](https://surfingcomplexity.blog/2025/03/03/locks-leases-fencing-tokens-fizzbee/)
- [Leader Election With S3 Conditional Writes — Gunnar Morling](https://www.morling.dev/blog/leader-election-with-s3-conditional-writes/)
- [How to build a distributed queue in a single JSON file on object storage — turbopuffer](https://turbopuffer.com/blog/object-storage-queue)
