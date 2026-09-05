# AZ Topology: Zero Inter-AZ Cost and Surviving an AZ Loss

**Answers:** Q41 — *How do we avoid inter-AZ transfer cost, and how do we survive losing an AZ?*
**Status:** Complete (v1)

## 1. These are the same question

Both reduce to: **what crosses an AZ boundary?** Cost says "as little as possible"; resilience
says "enough that losing one AZ doesn't lose anything." In a shared-nothing system those pull
hard against each other — you must replicate across AZs, and you must pay for it.

We are not shared-nothing. That changes the answer completely.

## 2. What getting it wrong costs

Cross-AZ transfer is **$0.01/GB in each direction — $0.02/GB round trip** — unchanged in 2026,
and the same rate for private IPs as public.

| Traffic that crosses AZ | Volume | $/month |
|---|---|---|
| Memtable replication R=3, 1 GB/s writes | 2 GB/s | **$103,680** |
| Query fan-out results, 10,000 QPS × 320 KB | 3.2 GB/s | **$165,888** |
| Query fan-out results, 100,000 QPS | 32 GB/s | **$1,658,880** |
| Flooded epoch gossip ([`epoch-propagation.md`](epoch-propagation.md)) | 6.9 GB/s | **$355,208** |
| *All of the above, AZ-local* | 0 | **$0** |

For comparison, the entire blob-store bill for 50M indexes is **~$14,000/month**. **Inter-AZ
transfer, done carelessly, is 10–100× the cost of the thing the whole architecture is
optimized around.** It would silently become the dominant line item.

### The landmine: NAT gateway

If blob-store traffic is routed through a NAT gateway instead of an **S3 VPC gateway
endpoint**, it is billed at **$0.045/GB processed**:

| Blob read volume | Via NAT | Via gateway endpoint |
|---|---|---|
| 1 GB/s | **$116,640/month** | **$0** |
| 10 GB/s | **$1,166,400/month** | **$0** |

Gateway endpoints for S3 and DynamoDB have **no hourly charge and no per-GB charge** — one
source calls them *"the single highest-ROI AWS networking configuration change available."*

> **D-78.** An S3 **gateway** VPC endpoint (not an interface endpoint, not NAT) is a deployment
> prerequisite, verified by a startup assertion: on boot, a node resolves the blob endpoint and
> refuses to start if the route would traverse NAT. This is a one-line misconfiguration that
> costs six figures a month and produces no error.

Also: run one NAT gateway **per AZ** for any remaining egress, or cross-AZ charges stack on top
of NAT processing charges.

## 3. The insight: S3 is our cross-AZ replication, and it's free

S3 Standard stores objects across **a minimum of three Availability Zones**, and there are **no
cross-AZ data transfer charges** for accessing it — S3 is a regional service, and AWS does not
charge for EC2↔S3 transfer within a region.

So:

> **Finding A-1.** In a shared-disk architecture, **the blob store *is* the cross-AZ
> replication mechanism, and it is free.** We never need to replicate durable data between
> nodes across AZs, because the durable copy is already multi-AZ before any node sees it.

This is a structural advantage over shared-nothing systems, which must pay cross-AZ transfer
for every replica of every write, forever. It is worth stating in the architecture doc as a
first-class property, because it inverts the usual trade: **we get AZ durability *and* zero
inter-AZ cost, rather than choosing.**

## 4. The design: independent per-AZ cells

> **D-79.** Each AZ runs its **own placement ring**. A request entering AZ-*a* is routed,
> forwarded, fanned out, and served entirely within AZ-*a*. The only thing crossing an AZ
> boundary is traffic to the blob store, which is free.

```
   AZ-a                    AZ-b                    AZ-c
 ┌────────────┐          ┌────────────┐          ┌────────────┐
 │ LRH ring a │          │ LRH ring b │          │ LRH ring c │
 │ caches     │          │ caches     │          │ caches     │
 │ memtables  │          │ memtables  │          │ memtables  │
 └─────┬──────┘          └─────┬──────┘          └─────┬──────┘
       └────────────────────────┼────────────────────────┘
                    ┌───────────▼────────────┐
                    │  S3 (multi-AZ, free)   │
                    └────────────────────────┘
```

Consequences:

| Concern | Result |
|---|---|
| Query fan-out | AZ-local. **$0** |
| Memtable replication (R-way) | Within one AZ. **$0** |
| Epoch notifications | AZ-local. **$0** |
| Routing / forwarding hops | AZ-local, ~0.2 ms instead of ~1 ms |
| Gossip | Zone-sharded with a few cross-AZ relays — tiny |
| Durable data | Multi-AZ via S3, free |
| **Cache** | **Duplicated per AZ** — the one real cost |

### The cache duplication is nearly free

Each AZ independently caches the hot set. At R = 316:1
([`../07-caching/storage-to-cache-ratio.md`](../07-caching/storage-to-cache-ratio.md)):

| Dataset | Hot set | × 3 AZs | Nodes' worth of cache | Nodes needed for QPS |
|---|---|---|---|---|
| 0.1 PB | 0.32 TB | 0.95 TB | 0.4 | 100–500 |
| 1 PB | 3.16 TB | 9.5 TB | 4.0 | 100–500 |
| 10 PB | 31.7 TB | 95 TB | 40 | 100–500 |

Because **capacity never binds — QPS does** (Finding R-2), the nodes are already bought. The
3× duplication consumes cache we were not otherwise using.

> Three findings compose here: high R makes the hot set small; QPS-binding makes cache capacity
> free; therefore per-AZ cells cost nothing. This is the cleanest example in the project of
> earlier decisions paying off later.

## 5. AZ failure safety: statically stable by construction

AWS's term for the goal is **static stability**: *"the overall system keeps working even when a
dependency becomes impaired"*, with capacity **pre-provisioned** so recovery does not depend on
control planes being available.

`pstore` is statically stable almost accidentally, because of decisions taken for other
reasons:

| Usual AZ-failure problem | Why we don't have it |
|---|---|
| Data loss when replicas die | Nodes own nothing; durable data is in S3, already multi-AZ |
| Quorum lost, writes stall | **No quorum, no consensus, no election** — nothing to lose |
| Split-brain on recovery | Nothing to split; CAS fences stale writers ([manifest-and-cas](../03-metadata-consistency/manifest-and-cas.md) §2) |
| Rebalancing storm | Nothing to rebalance; surviving nodes just read S3 |
| Control-plane dependency in the recovery path | **There is no control plane** |
| Failover window | No ownership ⇒ no failover |

**An AZ loss degrades to a cold-start event**, not a data event. That is the best possible
failure mode and it is a direct dividend of the masterless, ownership-free design.

### What is actually lost

| | Impact |
|---|---|
| `durable` writes | **Nothing.** Acked only after the S3 PUT. |
| `batched` writes buffered in that AZ | **Lost.** Memtables were replicated AZ-locally. Window = flush interval `T` (≤5 s). |
| Cache in that AZ | Lost; survivors serve cold until warm |
| In-flight queries | Fail; clients retry into surviving AZs |
| S3 Express One Zone WAL, if used | **Lost — it is single-AZ.** A third reason to treat Express with care (see [C-1](../05-storage-engine/batching-and-visibility.md)) |

> **D-80.** `batched` mode's documented loss window is **"one AZ, up to `T` seconds"**, not
> "one node". This must be stated plainly in the API docs — it is the honest price of
> AZ-local replication, and it is the right default because `durable` mode remains one
> parameter away.
>
> Optional tier: `batched_az_redundant` replicates the memtable to one node in a second AZ.
> At 1 GB/s that is ~$51,840/month for the extra copy — offered and priced, never default.

### Capacity headroom

| AZs | Max utilization per AZ | Survivors then carry |
|---|---|---|
| 2 | 50% | 2.00× |
| **3** | **66.7%** | **1.50×** |
| 4 | 75% | 1.33× |

> **D-81.** Deploy across **≥3 AZs** and run each at ≤66% of capacity, pre-provisioned. Do not
> rely on autoscaling during an AZ event — that puts an EC2 control plane in the recovery path,
> which is precisely what static stability forbids.

The cold-start spike is the real operational risk, not capacity: surviving AZs suddenly serve
50% more traffic against caches that never held it. Mitigations already specified —
metadata-only shadow warming (D-44), centroid-first cache fill, singleflight, admission control
on cold fills — plus **shed rather than stampede**, since a metastable collapse during an AZ
event would turn a survivable failure into an outage
([`../09-rust-stack/cpu-management.md`](../09-rust-stack/cpu-management.md) §5).

## 6. Hazards

| Hazard | Mitigation |
|---|---|
| **NAT-routed S3 traffic** | Startup assertion (D-78); alarm on NAT `BytesProcessed` |
| Cross-AZ traffic creeping in via a "helpful" optimization | **Alarm on any non-zero cross-AZ byte count**; treat it as a defect, not a metric |
| LB spreading a client's requests across AZs | Zone-aware LB / same-AZ target preference; session hints keep follow-ups local |
| A tenant's writes arriving in all 3 AZs → 3 cohort lanes | Fine — lanes are designed for this, and `W* = bytes/s × T / B` self-adjusts per AZ, so total `W` is unchanged |
| One AZ has fewer nodes (uneven capacity) | Per-AZ rings are independent; size each AZ for its own share plus headroom |
| An AZ that is degraded but not dead ("gray failure") | Hardest case: shed and drain by health signal, not liveness. Needs explicit design (OQ-136) |
| S3 itself degraded in one AZ | S3 handles this internally; our reads may see elevated latency. Congestion control and `bounded` reads absorb it |

## 7. Metrics

- **Cross-AZ bytes, by direction and by cause** — target zero; any non-zero value is a bug.
- NAT gateway `BytesProcessed` — target zero.
- Per-AZ: node count, utilization, cache hit rate, cold-query ratio.
- Headroom: current utilization vs. the N−1 threshold, alarmed *before* an event.
- AZ-event drill results: time to recover hit rate, shed rate during the drill.

## 8. Open questions raised

- **OQ-133 (Tier 2)** — Are three independent per-AZ caches actually better than one shared
  cache with cross-AZ reads? The arithmetic says yes overwhelmingly, but verify the hot set
  really is small enough that duplication is free at our largest expected tenant.
- OQ-134 — Should `W` write cohorts be per-AZ (implied by D-79) or global? Per-AZ is required
  for zero cross-AZ, and the formula self-adjusts — confirm it does not raise the PUT floor.
- OQ-135 — Multi-region is out of scope for v1, but does per-AZ cell structure generalize to
  per-region cells with async replication of immutable objects?
- OQ-136 — Gray AZ failure (degraded, not dead) is the hardest case and is not yet designed.
  Health-based draining, not liveness-based.
- OQ-137 — Does GCP/Azure have equivalent cross-zone pricing and a free regional-storage path?
  The design assumes it does; verify per cloud before promising BYOC parity.

## Sources

- [AWS Inter-AZ Data Transfer Costs: 2026 Architect's Guide](https://itmagic.pro/blog/aws-inter-az-data-transfer-costs-2026-architects-guide)
- [AWS Data Transfer Costs: The Complete 2026 Guide — Usage.ai](https://www.usage.ai/blogs/aws/networking-cost/data-transfer-costs/)
- [AWS Cross-AZ Data Transfer Costs More Than AWS Says — Last Week in AWS](https://www.lastweekinaws.com/blog/aws-cross-az-data-transfer-costs-more-than-aws-says/)
- [AWS NAT Gateway Pricing: How It Works and 6 Ways To Cut Costs — CloudZero](https://www.cloudzero.com/blog/reduce-nat-gateway-costs/)
- [NAT Gateway Costs: Find and Fix S3 Traffic — Cloud Tech by Victor](https://cloudtechbyvictor.com/blog/aws-nat-gateway-cost-vpc-endpoints)
- [Static stability using Availability Zones — AWS Builders' Library](https://aws.amazon.com/builders-library/static-stability-using-availability-zones)
- [REL11-BP05 Use static stability to prevent bimodal behavior — AWS Well-Architected](https://docs.aws.amazon.com/wellarchitected/latest/framework/rel_withstand_component_failures_static_stability.html)
- [Object Storage Classes — Amazon S3 (Standard spans ≥3 AZs)](https://aws.amazon.com/s3/storage-classes/)
- [Amazon S3 FAQs — AWS](https://aws.amazon.com/s3/faqs/)
- Arithmetic reproducible in `04-cluster/az-budget.py`.
