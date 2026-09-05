# Routing and Placement: Cache Affinity Without Ownership

**Answers:** Q12
**Status:** Complete (v1)

## The problem restated

Nodes own nothing. Any node can serve any index by reading the blob store. So placement is
purely: **maximize the chance that the node handling a request already has the bytes in its
NVMe/RAM cache**, while keeping load even and churn low.

Requirements:
1. **O(1) or O(C) lookup** — this must be computed per request at 10K-node scale.
2. **Minimal churn** — when a node joins or leaves, few keys should move, or cache value is
   destroyed fleet-wide.
3. **Bounded load** — no node should carry ≫ average.
4. **Replication factor R** — a hot index should map to R nodes so a single node isn't a
   bottleneck.
5. **No coordinator.** Placement must be computable from the membership view alone.

## The candidates, and the disqualifying finding

| Scheme | Lookup | Load balance | Churn | Verdict at 10K nodes |
|---|---|---|---|---|
| Ring consistent hashing (1 token/node) | O(log N) | **peak/avg = Θ(ln N)** — bad | ~1/N moves | Needs vnodes |
| Ring + virtual nodes | O(log N) | 1+ε needs **Θ(ln N / ε²) vnodes/node** | ~1/N | 10K nodes × ~200 vnodes = 2M ring entries. Heavy but workable. |
| **Rendezvous / HRW** | **O(N)** | Excellent, no vnodes needed | Minimal | ❌ **Disqualified.** Published guidance: *don't use HRW above ~100 nodes* due to O(N) lookup. At 10K that is 10,000 hashes **per request**. |
| Maglev | O(1) via lookup table | Very good | Small but **non-minimal** | Good for L4 LB; table rebuild on churn. |
| Consistent Hashing with Bounded Loads (CHBL) | O(log N) + probing | **Provably bounded**: cap `C = ⌈(1+ε)·n/k⌉`, skip overloaded nodes | ~1/N | Strong candidate; used in HAProxy (Vimeo). |
| **Local Rendezvous Hashing (LRH)** | **O(C)**, C≈20–100 | max load within **10–15% of average** (vs 50%+ for plain RH) | **5–10× lower churn than consistent hashing** | **Best fit.** |

**Local Rendezvous Hashing** restricts HRW scoring to a contiguous window of `C` distinct
physical neighbours on a ring. It recovers HRW's excellent balance and minimal churn while
dropping lookup from O(N) to O(C), with reported **sub-millisecond lookup at >10,000 nodes**
and O(log n) load-imbalance bounds.

> **D-5.** Placement uses **Local Rendezvous Hashing with C ≈ 32**, layered with
> **CHBL-style overload skipping** for real-time load feedback.

LRH gives us structural balance from the hash; CHBL-style skipping handles the balance the
hash cannot know about — a single 50 TB index, or an index at 100× the query rate of its
neighbours. Real load beats hashed load whenever they disagree.

## The placement function

```
placements(index_id, shard, R) -> [node_id; R]

1. ring_pos   = hash(index_id, shard)
2. window     = C physical nodes clockwise from ring_pos      // O(log N) lookup + O(C) walk
3. scored     = window sorted by hrw_weight(node, key) desc   // O(C log C)
4. take the first R nodes whose observed_load < (1+ε)·avg_load  // CHBL skip
5. if fewer than R qualify, relax ε (never fail)
```

`observed_load` comes from gossip-piggybacked load hints. Because it is only an optimization,
stale load data is harmless.

## Routing tiers

```
client → LB (any node)  → coordinator node (LRH-selected)  → [fan-out to shard nodes]
```

- **Any node can accept any request.** The external LB needs no affinity awareness at all —
  a plain round-robin L4/L7 LB is sufficient. This is important: it means no custom LB,
  no consistent-hashing LB config, no sticky sessions.
- The receiving node computes placement locally and **forwards** (cheap intra-cluster RPC,
  ~0.2 ms) rather than serving cold. One extra hop is ~1% of a cold blob fetch and buys a
  cache hit.
- If the receiving node is already a placement for that key, it serves directly (no hop).
- **Fallback:** if all R placements are unhealthy or slow, *any* node serves it from the blob
  store. Correctness never depends on reaching the "right" node. This is the property that
  makes the system trivially available.

## Churn behaviour

| Event | Effect |
|---|---|
| Node joins | ~1/N of keys move to it. It serves them cold for a few queries. No data movement — it just reads the blob store. Recovery = one cold query (~400 ms), not a rebuild. |
| Node leaves | Its keys redistribute to LRH neighbours; those serve cold briefly. |
| **Scale from 1,000 → 2,000 nodes** | ~50% of keys remap; cache hit rate dips for minutes. **Nothing is copied.** This is the WarpStream property: no rebalancing, scale in seconds. |
| Rolling deploy of 10K nodes | Churn is continuous; LRH's low churn keeps the aggregate cache warm. Use **graceful drain**: a departing node keeps serving for a grace period while gossip propagates. |

Mitigations for the scale-out cache dip:
- **Shadow warming.** After a placement change, the new owner asynchronously pre-fetches the
  hot files (centroids first — see `07-caching/`) while the old owner still serves.
- **Cache handoff hint.** The old owner tells the new owner which files were hot; the new
  owner fetches from the blob store (not from the peer — peer-to-peer transfer would create
  the coupling we are trying to avoid).

## Why not "one node owns an index"?

Because it reintroduces everything we deleted: ownership needs leases, leases need liveness,
liveness needs consensus, and failover needs a window. **Any node can serve anything** is not
a compromise; it is the whole architecture.

## Cost summary

| Operation | Cost |
|---|---|
| Placement computation | O(log N + C) ≈ sub-millisecond, no I/O |
| Extra hop for affinity | ~0.2 ms intra-AZ |
| Node join/leave | **0 bytes copied** |

## Open questions raised

- OQ-14: Optimal `C` and `ε` under our real index-size distribution (heavy-tailed).
- OQ-15: Should placement key be `(index, shard)` or `(index, shard, file)`? File-level gives
  better balance for one huge index but multiplies routing decisions per query. Leaning
  `(index, shard)` with per-file placement only above a size threshold.
- OQ-16: Quantify the scale-out cache dip; is shadow warming worth its blob-request cost?

## Sources

- [Local Rendezvous Hashing: Bounded Loads and Minimal Churn via Cache-Local Candidates — arXiv](https://arxiv.org/pdf/2512.23434)
- [Consistent Hashing with Bounded Loads — arXiv](https://arxiv.org/pdf/1608.01350)
- [Revisiting Consistent Hashing with Bounded Loads — arXiv](https://arxiv.org/pdf/1908.08762)
- [Consistent Hashing: Algorithmic Tradeoffs — Damian Gryski](https://dgryski.medium.com/consistent-hashing-algorithmic-tradeoffs-ef6b8e2fcae8)
- [Consistent Hashing vs. Rendezvous Hashing — DZone](https://dzone.com/articles/consistent-hashing-vs-rendezvous-hashing-a-compara)
- [Rendezvous hashing — Wikipedia](https://en.wikipedia.org/wiki/Rendezvous_hashing)
- [Maglev: A Fast and Reliable Software Network Load Balancer — Google Research](https://research.google/pubs/maglev-a-fast-and-reliable-software-network-load-balancer/)
- [Architecture — WarpStream docs](https://docs.warpstream.com/warpstream/overview/architecture)
