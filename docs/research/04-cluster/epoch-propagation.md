# Propagating "Something Changed" Without Paying for It

**Answers:** Q40 — *How do we spread news of a new generation / metadata update as
best-effort, so queries don't pay a round trip to discover it?*
**Status:** Complete (v1)

## 1. What the news is for

When a tenant commits (a fold, a compaction, a schema change), its HEAD advances to a new
epoch. Nodes serving that tenant need to know so they can revalidate before a query arrives
rather than during one.

The correctness floor already exists and is cheap: a conditional GET of HEAD with
`If-None-Match`, which returns **304 Not Modified** — full read price, zero bytes. The problem
is not cost, it is **when** it happens.

> **Finding E-1.** Propagation does not remove the blob GET. It moves it **off the query
> critical path**. That is the entire value: ~30 ms of revalidation happening 200 ms *before*
> the query instead of *during* it.

Stating it this way also settles the correctness question. The notification is a **hint to
revalidate**, never the new state itself.

> **D-75.** A node never serves state it learned from a peer. It serves state it read from the
> blob store, possibly *because* a peer told it to look. Trusting gossiped state would break
> the CAS-as-fencing model ([`../03-metadata-consistency/manifest-and-cas.md`](../03-metadata-consistency/manifest-and-cas.md) §2)
> — a node that lost a CAS race, or is simply buggy, could otherwise publish state that was
> never committed.

## 2. Don't gossip it — the committer knows exactly who cares

The instinct is to gossip epoch changes through the membership mesh. The arithmetic kills it.

Event rate: 1M tenants folding hourly (278/s) plus ~10,000 hot tenants committing every second
≈ **10,278 commits/s**.

| Dissemination | Fleet bandwidth | Cross-AZ cost (if 2/3 crosses) |
|---|---|---|
| **Targeted → the R=3 placements** | **3.1 MB/s** | **~$107/month** |
| Full flood → all 10,000 nodes | 10,278 MB/s | **~$355,000/month** |

**Three orders of magnitude.** A flooded epoch feed would cost more in cross-AZ transfer than
the entire rest of the system.

And it is unnecessary, because **placement is a pure function**:

> The node that just won the CAS can compute `placements(tenant)` locally — the same LRH
> function used to route reads — and send `R` unicast messages. No subscription registry, no
> topic routing, no flooding. The set of nodes that care is *derivable*, exactly as blob keys
> are.

> **D-76.** Epoch changes are propagated by **direct unicast to the computed placements**, not
> by gossip. Gossip carries membership and load; it does not carry per-tenant state.

This is the same principle that removed LIST from the storage layer, applied to the network:
*derive who needs to know, don't broadcast and let them filter.*

## 3. The three-tier propagation stack

| Tier | Mechanism | Latency | Guarantees |
|---|---|---|---|
| **1. Direct notify** | Committer → R placements, unicast, ~100 B | ~0.2 ms | Best effort |
| **2. Piggyback** | Epoch hints ride on query forwards, heartbeats, write acks | ~seconds | Best effort, free |
| **3. Conditional GET** | `If-None-Match` on HEAD, jittered, demand-driven | poll interval | **The correctness floor** |

Tier 3 is the only one that must work. Tiers 1 and 2 are latency optimizations that can be
lost, duplicated, delayed, or reordered with no consequence beyond a slower query.

> **D-77.** Every tier above the conditional GET is deletable. If a propagation mechanism ever
> becomes load-bearing for correctness, that is a design regression.

### Message shape
```
EpochHint { tenant_id, index_id?, epoch, manifest_ref, sender }
```
Carrying `manifest_ref` lets the receiver fetch the *immutable* manifest object directly
(cacheable, no CAS semantics) once it has confirmed the epoch via HEAD — or skip work entirely
if it is already at ≥ `epoch`. Including the epoch means most hints are resolved with **zero
blob requests**, because the node is already current.

### Coalescing
A tenant committing rapidly generates a hint per commit. Receivers **coalesce by
`(tenant, index)` keeping the max epoch**, and revalidate at most once per interval. A burst of
100 commits produces one revalidation.

### Interaction with the session protocol
A client's session token already carries `(epoch, watermark)`
([`../11-design/session-and-affinity-protocol.md`](../11-design/session-and-affinity-protocol.md)),
so a request itself is a propagation channel: a node receiving a token ahead of its own view
learns it must refresh. **The client is tier 2.5** — and it is the tier that matters most,
because it fires exactly when someone actually cares.

## 4. Who gets notified when placement disagrees

The committer computes placements from *its* membership view. Views differ during churn, so
some notifications go to the wrong nodes and some correct nodes are missed. Both are harmless:
a wrong recipient ignores an unknown tenant; a missed node falls back to tier 3.

New placements (after a scale-out) have never been notified about anything. They poll on first
use, which is the cold path they were going to take regardless.

## 5. Cost summary

| Operation | Cost |
|---|---|
| Notify on commit | R unicast messages, ~100 B, AZ-local |
| Receiver already current | **0 blob requests** |
| Receiver stale | 1 conditional GET (usually 304) |
| Steady state, no commits | **0** |

At 10,278 commits/s fleet-wide: ~3 MB/s of notification traffic and a bounded revalidation
rate, versus a flood that would cost ~$355k/month in cross-AZ alone.

## 6. Open questions raised

- OQ-130 — Coalescing interval for hint→revalidation. Too short wastes GETs on a hot tenant;
  too long defeats the purpose.
- OQ-131 — Should the notification include the manifest bytes for small tenants (inline HEAD),
  turning a hint into a prefetch? Tempting, but it edges toward trusting peer state (D-75);
  a safe form would be "prefetch this immutable object" while still confirming via HEAD.
- OQ-132 — Do we need notification at all once `session` is the default consistency mode
  (the client carries the epoch)? Possibly only for *background* freshness — cache warming and
  compaction scheduling — rather than for query correctness. Measure before building tier 1.

## Sources

- [Gossip Protocol Explained — High Scalability](https://highscalability.com/gossip-protocol-explained/)
- [The Promise and Limitations of Gossip Protocols — Cornell](https://research.cs.cornell.edu/Quicksilver/public_pdfs/2007PromiseAndLimitations.pdf)
- [Consistency level choices — Azure Cosmos DB (session tokens)](https://learn.microsoft.com/en-us/azure/cosmos-db/consistency-levels)
- [Add preconditions to S3 operations with conditional requests — AWS](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-requests.html)
- [AWS Inter-AZ Data Transfer Costs: 2026 Architect's Guide](https://itmagic.pro/blog/aws-inter-az-data-transfer-costs-2026-architects-guide)
- Arithmetic reproducible in `04-cluster/az-budget.py`.
