# Membership at 10,000 Nodes, With No Coordinator

**Answers:** Q11
**Status:** Complete (v1)

## What membership is actually for

Because nodes are stateless and own no data, membership is **not** needed for correctness.
It is needed only to answer: *"which node should I ask, so that the cache is likely warm?"*

> **Design rule 11.** Membership is an optimization input. A stale, incomplete, or wrong
> membership view degrades cache hit rate and nothing else. No correctness property may ever
> depend on it.

> **Scope note.** Rule 11 holds for *binary* liveness. It does **not** cover degraded-but-alive
> nodes: a gray-failing AZ keeps passing liveness checks while ruining latency for a third of
> traffic (17–67× effective mean). That needs a second, *continuous* signal which never removes
> a node from membership and only adjusts routing weight — deliberately separate from
> Lifeguard, which we chose precisely because it suppresses false positives. See
> [`gray-failure.md`](gray-failure.md).

This is a much weaker requirement than a typical distributed system, and it is what makes
10K nodes easy rather than hard. Compare: in a system with data ownership, a wrong membership
view means unavailability or split-brain.

## Option A — Gossip (SWIM + Lifeguard)

SWIM separates failure *detection* from membership *dissemination*, using randomized probing
plus infection-style gossip. Deployments of **10,000+ participants** exist; HashiCorp's
`memberlist`/Lifeguard has run clusters of **6,000+ nodes**.

**Lifeguard** matters at our scale: it adds local health awareness, cutting false-positive
failure detections by **50×** while detecting true failures faster. Without it, SWIM declares
healthy members faulty whenever a node is CPU-starved or the network is slow — which, in a
cache-heavy search node under load, is *the normal state*.

| | |
|---|---|
| ✅ | Sub-second convergence; proven at our scale; no external dependency; carries piggybacked hints (epoch changes, load) for free. |
| ❌ | O(N) memory per node for the member table (10K × ~200 B = 2 MB — fine); constant background chatter; needs seed discovery; partition behaviour needs care. |

## Option B — Blob-store-published membership

Each node writes a small heartbeat object at a derived key and reads a compacted roster:

```
{h}/clu/live/{node_id}          <- PUT every T seconds (~10 s), ~200 B
{h}/clu/roster/{gen:020}        <- immutable snapshot, folded by any node
{h}/clu/ROSTER                  <- CAS'd pointer to current gen
```

Cost at 10K nodes, T = 10 s: **1,000 PUT/s = ~$13/day = ~$390/mo.** Plus roster reads.
That is real money for a liveness signal, and it puts 1,000 writes/s of pure overhead on the
blob store forever.

| | |
|---|---|
| ✅ | Zero node-to-node networking; works across partitions; trivially debuggable (`cat` the roster); no seed problem — the bucket is the seed. |
| ❌ | ~$400/mo of pure overhead; detection latency = heartbeat interval (10s of seconds); folding the roster is a job someone must do. |

## Decision: hybrid, gossip-primary

> **D-4.** Use **SWIM + Lifeguard gossip as the primary membership protocol**, and the blob
> store as the **seed/bootstrap and partition-healing backstop**.

Concretely:
- A joining node reads `{h}/clu/ROSTER` (**1 GET**) to obtain seed addresses, then joins the
  gossip mesh. **No LIST, no service discovery dependency, no DNS requirement.** The bucket
  we already depend on is the discovery mechanism — this preserves the "one stateful
  dependency" property.
- The roster object is refolded lazily (every ~60 s, by whichever node wins an optimistic
  CAS) from the gossip view. It is a *cache of gossip*, not the source of truth. Cost:
  ~1 PUT/min = negligible.
- Gossip carries piggybacked hints: current epoch per hot index, node load, cache-fill
  status, and the node's software version.
- **Partition behaviour:** two partitions form two gossip meshes and two rosters. Because
  membership is only an optimization, both continue serving correctly at a reduced cache hit
  rate. When the partition heals, gossip merges. **There is no split-brain to resolve** —
  a direct dividend of Design rule 9.

### Zone-sharded gossip is required, not optional

> **C-7 — measured, M4b.** This section was an argument; it now has a number. Flat gossip on
> a **100-node fleet** costs **22,776 / 37,423 / 75,391 bytes/s/node** at 25 / 50 / 100 nodes
> — each doubling multiplies per-node cost by 1.64× then 2.01×, against 2.0× for O(N) and
> ~1.2× for O(log N). Per-node cost is **linear in fleet size**, so total gossip traffic is
> **quadratic**, at 100 nodes and without waiting for 10,000.
>
> ⚠️ Two things this does *not* say. It is not a measurement of the cross-AZ **bill**, which
> is what makes zone-sharding a cost requirement rather than a scaling one — these 100 nodes
> share a host and cross no AZ boundary. And it is `chitchat` (Scuttlebutt + phi-accrual),
> not SWIM: `foca` is MPL-2.0 and outside the licence allow-list. A SWIM mesh gossips a
> digest of similar shape, so the slope should carry, but that is an expectation and not a
> measurement. Evidence: [`M4b/VERIFIED.md`](../../milestones/M4b/VERIFIED.md) criterion 5.
> **OQ-13's crossover point remains open**, and is now bounded from below: flat gossip is
> already the dominant per-node cost at 100.
Shard the mesh by **AZ**, with a small number of cross-AZ relays, so per-node fanout stays
constant *and* cross-AZ chatter stays negligible. This was framed as a scaling option; per
[`az-topology.md`](az-topology.md) it is a **cost requirement** — cross-AZ traffic is
$0.02/GB round trip and any flat-mesh chatter is billed.

Note also that per-tenant state (epoch changes) is **not** gossiped at all — the committer
computes the placements and unicasts them. See [`epoch-propagation.md`](epoch-propagation.md).

## Node identity

`node_id = uuid` generated at process start, **not** derived from IP or hostname. Reasons:
- A restarted node has a genuinely cold cache, so it *should* be a different placement target
  — reusing the identity would route traffic to a cold node believing it is warm.
- Avoids IP-reuse aliasing in Kubernetes.
- Placement stability comes from the hashing scheme (`routing-and-placement.md`), not from
  identity reuse.

## Cost summary

| Operation | RA |
|---|---|
| Node join | 1 Rseq (roster) |
| Steady-state membership | **0 blob requests** (gossip) |
| Roster refold | 1 W/min cluster-wide |

## Open questions raised

- OQ-12: Does Lifeguard's false-positive suppression hold when nodes are routinely CPU-pinned
  by SIMD distance computation? Our workload is exactly the pathological case SWIM struggles
  with — needs a load test.
- OQ-13: Flat vs. zone-sharded gossip crossover point. Measure at 2K, 5K, 10K.

## Sources

- [SWIM: Scalable Weakly-consistent Infection-style Process Group Membership Protocol](https://www.semanticscholar.org/paper/SWIM:-scalable-weakly-consistent-infection-style-Das-Gupta/068f65c0271ed16a6bf4a1c2de1a962eec08edbf)
- [Lifeguard: Local Health Awareness for More Accurate Failure Detection — arXiv](https://arxiv.org/pdf/1707.00788)
- [Making Gossip More Robust with Lifeguard — HashiCorp](https://www.hashicorp.com/en/blog/making-gossip-more-robust-with-lifeguard)
- [Architecture — WarpStream docs (stateless agents, no rebalancing)](https://docs.warpstream.com/warpstream/overview/architecture)
