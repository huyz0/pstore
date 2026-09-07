# Gray AZ Failure: Detection and Health-Based Draining

**Answers:** Q42 — **closes [OQ-136](../00-plan/open-questions.md)**
**Status:** Complete (v1)
**Companion to:** [`az-topology.md`](az-topology.md), which handles *dead* AZs

## 1. Why this is the hardest case

A dead AZ is easy: nodes stop responding, SWIM marks them dead, placement routes around them,
and because nodes own nothing it degrades to a cold-start event. A *gray* AZ is the opposite —
it keeps answering, keeps passing health checks, and keeps ruining latency.

The canonical definition is **differential observability**: gray failure is when *at least one
app observes the system as unhealthy while the system's own observer observes it as healthy*
(Huang et al., HotOS 2017). Typical forms: performance degradation, random packet loss, flaky
I/O, memory thrashing, capacity pressure, non-fatal exceptions.

What it costs us while undetected, with 3 AZs and one degraded:

| Degraded AZ p50 | Effective mean latency | Traffic affected |
|---|---|---|
| 500 ms (vs 10 ms) | **173 ms — 17×** | 1/3 |
| 2,000 ms | **673 ms — 67×** | 1/3 |

A third of traffic at 50–200× normal latency, with every liveness check green.

## 2. Three tensions with decisions we already made

Honesty first — this problem cuts against three earlier choices, and the resolutions matter
more than the detection algorithm.

| Tension | Resolution |
|---|---|
| **Lifeguard was chosen to suppress false positives** (50× fewer), precisely so a CPU-pinned node isn't declared dead. That makes it *structurally reluctant* to notice a slow node. | Keep them separate. **Lifeguard governs binary membership; gray detection is a distinct, continuous signal that never removes a node from membership** — it only reduces routing weight. Two detectors, two jobs, no conflict. |
| **Static stability says don't react during an event** — pre-provisioned capacity, no control-plane dependency in the recovery path. Gray failure *requires* reaction. | The reaction is **data-plane only**: nodes change their own health-endpoint answer, and the LB reacts. No API call, no control plane. Capacity stays pre-provisioned. Static stability is preserved. |
| **Per-AZ cells removed all cross-AZ traffic** ([`az-topology.md`](az-topology.md) D-79) — so **no node in AZ-a has any first-hand evidence about AZ-b.** | We created the blind spot; we buy the observability back deliberately (§3). It costs ~$4–16/month. |

That third one deserves emphasis: **our own cost optimization is what made gray detection
hard.** Worth remembering as a general lesson — eliminating traffic also eliminates signal.

## 3. Detection: build differential observability in

The Panorama result (OSDI 2018) is the guide: *"the missing piece in failure detection is
detecting what the requesters of a failing component see"* — turn clients into observers.
Panorama detected all 15 reproduced real-world gray failures in **under 7 seconds**, where
existing approaches caught one of them in under 300 s.

So we need observers *outside* the suspect AZ. Three classes, deliberately diverse:

### (a) Cross-AZ probe mesh — fast, direct, cheap
A few designated nodes per AZ issue real (not synthetic-shallow) queries against the other AZs
and record latency and success.

| Config | Probes/s | Cross-AZ cost |
|---|---|---|
| 3 probers/AZ @ 2 Hz, 2 KB | 36 | **$3.82/month** |
| 5 probers/AZ @ 5 Hz, 2 KB | 150 | **$15.93/month** |
| 10 probers/AZ @ 10 Hz, 4 KB | 600 | $127/month |

> **D-82.** Run a cross-AZ probe mesh. It is the one piece of cross-AZ traffic we *want*, and at
> ~$16/month it buys back the observability that per-AZ cells removed. Probes must exercise the
> **real query path** — a shallow `/health` probe cannot see a gray failure by definition.

### (b) Blob-store health bulletin — partition-tolerant, no cross-AZ traffic
Each AZ periodically publishes a self-report to a derived key; every AZ reads all of them.

```
{h}/clu/health/{az}/{gen}   ← p50/p99, error rate, shed rate, blob latency,
                              PSI, utilization, node count, observed peers
```

| Period | Cost |
|---|---|
| every 2 s | $24.11/month |
| **every 5 s** | **$9.64/month** |
| every 15 s | $3.21/month |

This is the pleasing one: **it works when the AZs cannot talk to each other at all.** During an
inter-AZ network partition the probe mesh goes dark and gossip splits, but every AZ can still
reach S3 — a regional service — so the bulletin board still functions. It turns our single
stateful dependency into a partition-tolerant communication channel.

> **D-83.** The blob store is the cross-AZ health bulletin board. It costs ~$10/month and is
> the only channel that survives an inter-AZ partition.

Self-reports are necessary but **not sufficient** — a sick component frequently reports itself
healthy, which is the whole point of gray failure. They are weighted lower than external
observations.

### (c) Passive outlier detection on real traffic
The LB's per-target-group metrics, client SDK telemetry, and the probe mesh all feed one
judgement. **Compare peers; never threshold absolutely** — absolute latency thresholds fail
because normal latency varies with load, cache state, and query shape.

Envoy's success-rate outlier detection is the model: aggregate success rate across hosts, then
eject those below `mean − (stdev × factor)`, with a default factor of **1.9 standard
deviations** (~97th percentile). Apply the same statistic at AZ granularity, on both success
rate and latency.

> **D-84.** Gray detection is **peer-relative and self-calibrating**: is AZ-b an outlier versus
> AZ-a and AZ-c *right now*? Never "is AZ-b slower than 100 ms?"

> **C-9 — the statistic, corrected. M4e, measured.** D-84's principle holds exactly; the
> **Envoy default it borrows does not transfer**. With *n* samples the largest z-score any one
> can reach is `(n − 1) / √n`, so with **three zones the ceiling is 1.155** and a factor of
> **1.9 can never fire, whatever the degradation**. Implemented as written, gray failure would
> have been undetectable at precisely the fleet shape D-79 prescribes — three AZs.
>
> Excluding the candidate from its own baseline fixes the masking but not the sensitivity:
> with two peers a standard deviation is nearly meaningless, and a zone 4% slower than its
> neighbours becomes a 3σ event. The shipped test is a **relative margin against the peer
> median** — 50% on latency, 5 points on success rate — robust with two peers and
> scale-invariant, so it stays peer-relative in D-84's sense rather than an absolute threshold
> in disguise. Evidence: [`M4e/VERIFIED.md`](../../milestones/M4e/VERIFIED.md) criterion 10.

### Detection latency

| Signal | Sample | Confirmations | Time to suspect |
|---|---|---|---|
| Probe mesh | 0.5 s | 3 | **1.5 s** |
| Blob bulletin | 5 s | 2 | 10 s |
| Passive outlier | 10 s | 2 | 20 s |

Probe mesh for speed, the other two for corroboration and partition tolerance.

## 4. The safety rule that matters most: gray ≠ overload

**Draining an overloaded AZ moves its load onto the others and can cascade.** That is a
metastable failure with extra steps ([`../09-rust-stack/cpu-management.md`](../09-rust-stack/cpu-management.md) §5):
the detector would cause the outage it was built to prevent.

Distinguishing signal: **normalize degradation by load.**
- *Overload* — degradation rises **with** request rate and utilization. Shed, don't drain.
- *Infrastructure gray failure* — degradation appears at **normal or low** utilization; the AZ
  is slow while doing less work.

> **D-85.** Draining is authorized only for degradation that persists **after** normalizing for
> load, and only when survivors have measured headroom. Load-correlated degradation triggers
> shedding, never draining. Encode this as a precondition, not a runbook note.

## 5. Deciding to drain

Graduated, never binary:

| State | Trigger | Action |
|---|---|---|
| **Healthy** | — | Full weight |
| **Suspect** | 1 observer class, above threshold | Reduce weight ~50%; stop assigning new background work; **hedge away from it** |
| **Draining** | ≥2 observer classes, quorum, load-normalized, headroom confirmed | Weight → 0 for new requests; in-flight completes |
| **Recovering** | Signals clear for a dwell period | Ramp weight back gradually |

Five guardrails, each of which exists to stop the detector from becoming the outage:

1. **Never drain more than one AZ.** A regional problem, a bad deploy, or a shared-dependency
   failure looks like "all AZs unhealthy." If the logic would drain two of three, drain none —
   the fault is not zonal. This is the single most important rule here.
2. **Quorum of independent observers**, from ≥2 other AZs. One node's opinion cannot drain an AZ.
3. **Headroom precondition.** Only drain if survivors can absorb 1.5× ([`az-topology.md`](az-topology.md) D-81).
   Otherwise degrade in place.
4. **Hysteresis and minimum dwell.** Enter slowly, exit slowly, no flapping.
5. **Always alert, even when automated.** Automation buys minutes; humans decide what happens
   next.

This mirrors AWS's own guidance for health checks: *automation should stop traffic to a single
bad server but keep serving if the entire fleet appears to be in trouble* — and NLB, ALB and
Route 53 all **fail open** when no targets report healthy. Our rules are the AZ-scale version of
the same principle.

## 6. Draining without a control plane

> **D-86.** Draining is executed by **node self-eviction from the load balancer**: nodes in the
> affected AZ begin returning unhealthy on their LB health endpoint. This is pure data plane —
> no API call, no control-plane dependency in the recovery path — so static stability is
> preserved and it works with any load balancer.

A node decides to self-evict when *external* observers say it is bad (read from the bulletin
board), **not** from its own opinion of itself. That inverts the usual health check and is what
makes it capable of catching gray failure at all.

Plus one unconditional local trigger:

> **D-87.** A node that cannot reach the blob store self-evicts immediately. The blob store is
> our only dependency; a node that cannot reach it is definitionally useless, and this is the
> one case where self-assessment is reliable.

Health-check design, following AWS's guidance:
- The endpoint is a **deep-enough** check (blob reachability, cache health, scan-pool liveness)
  but includes **only critical dependencies** — adding a non-critical one converts it into a
  critical one and widens the blast radius.
- **Fail open at the fleet level**: if a large fraction of nodes would go unhealthy at once,
  they do not. Individual eviction, never collective suicide.
- The health path uses reserved resources (memory reserve, priority queue) so an overloaded node
  can still answer honestly rather than timing out.

### Operator backstop: AWS zonal shift
Route 53 ARC **zonal shift** *"temporarily moves load balancer traffic away from an Availability
Zone"*, manually initiated. **Zonal autoshift** lets AWS start it automatically from its own
internal telemetry — a genuinely independent observer with visibility we do not have.

> **D-88.** Enable **zonal autoshift** as a belt-and-braces layer, and keep manual zonal shift
> in the runbook. AWS's telemetry sees infrastructure we cannot. Our own draining must be
> idempotent with respect to it — both mechanisms shifting at once must not double-count as
> two AZs lost (guardrail 1).

Autoshift also runs **weekly practice runs** to verify capacity exists in the remaining AZs.
That is a free, recurring test of D-81's headroom assumption, and we should adopt it rather
than build our own.

## 7. Refinement: hedging should be health-aware

[`../09-rust-stack/cpu-management.md`](../09-rust-stack/cpu-management.md) §5 says disable
hedging above a load threshold, because a hedge doubles CPU when CPU is scarce. But hedging is
also the best *per-request* mitigation for gray failure.

> **Refinement to D-65's context.** Hedging is not a single global switch. **Hedge away from
> suspect targets even under load; do not hedge blindly.** A hedge that re-issues to a
> known-healthy peer costs one extra request and rescues a request stuck behind a gray failure;
> a blind hedge under overload is the amplifier we were worried about. The decision is
> per-target-health, not global.

## 8. Testing

Gray failure that is never exercised will not be detected in production.

> **D-89.** `pstore-sim` must model **gray** failures, not just crash-stop: injected latency
> distributions, partial packet loss, asymmetric partitions, partial capacity loss, and slow
> blob responses confined to one AZ. A simulator that only kills nodes tests the easy case.

Plus: weekly practice drains (mirroring autoshift practice runs), and game days that inject
degradation rather than termination.

## 9. Metrics

- Per-AZ p50/p99/error rate, **and each AZ's deviation from the peer mean in stdevs** — the
  actual decision variable.
- Probe-mesh success and latency, per source→target AZ pair (asymmetry is diagnostic).
- Bulletin freshness per AZ (a stale bulletin is itself a signal).
- Current state per AZ (healthy/suspect/draining) and time in state.
- **Drain events, drain duration, and false-positive rate** — the last one governs how
  aggressive the thresholds may be.
- Divergence between self-report and external observation — **that gap is literally the
  definition of gray failure**, so it should be a first-class metric, not a derived one.

## 10. Open questions raised

- **OQ-138 (Tier 2)** — Threshold calibration: how many stdevs, how many confirmations, what
  dwell? Requires real latency distributions across AZs; start conservative (suspect-only, no
  auto-drain) and tighten with data.
- OQ-139 — Should probers be dedicated nodes or rotate among all nodes? Rotating avoids a
  correlated blind spot if the probers themselves are the sick ones.
- OQ-140 — Load normalization (D-85) needs a concrete model: degradation-per-unit-utilization,
  or a residual against a fitted latency-vs-load curve?
- OQ-141 — Does the blob bulletin need signing? A compromised or buggy node publishing a false
  "AZ-b is sick" could trigger a drain. Guardrail 2 (quorum) limits it; signing would close it.
- OQ-142 — Interaction between our draining and AWS zonal autoshift: can both fire and
  effectively remove two AZs? Needs an explicit interlock.
- OQ-143 — Gray failure of the **blob store itself** in one AZ (elevated S3 latency from one
  AZ's network path). Detection is the same; the response differs, since draining does not help
  if the problem is regional. Distinguishing "our AZ is sick" from "S3 is sick for our AZ"
  needs its own signal.

## Sources

- [Gray Failure: The Achilles' Heel of Cloud-Scale Systems — Huang et al., HotOS 2017 (Microsoft Research PDF)](https://www.microsoft.com/en-us/research/wp-content/uploads/2017/06/paper-1.pdf)
- [Gray failure: the Achilles' heel of cloud-scale systems — the morning paper](https://blog.acolyer.org/2017/06/15/gray-failure-the-achilles-heel-of-cloud-scale-systems/)
- [Capturing and Enhancing In Situ System Observability for Failure Detection (Panorama) — OSDI 2018](https://www.usenix.org/conference/osdi18/presentation/huang)
- [Panorama — the morning paper](https://blog.acolyer.org/2018/10/15/capturing-and-enhancing-in-situ-system-observability-for-failure-detection/)
- [The φ accrual failure detector — Hayashibara et al.](https://www.researchgate.net/publication/29682135_The_ph_accrual_failure_detector)
- [Outlier detection — Envoy documentation](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/upstream/outlier)
- [Outlier detection (proto) — Envoy API reference (success_rate_stdev_factor)](https://www.envoyproxy.io/docs/envoy/latest/api-v3/config/cluster/v3/outlier_detection.proto)
- [Implementing health checks — Amazon Builders' Library (PDF)](https://d1.awsstatic.com/builderslibrary/pdfs/implementing-health-checks.pdf)
- [Amazon Builders' Library in Focus #6: Implementing Health Checks — Lumigo](https://lumigo.io/blog/amazon-builders-library-in-focus-6-implementing-health-checks/)
- [Zonal autoshift in ARC — AWS documentation](https://docs.aws.amazon.com/r53recovery/latest/dg/arc-zonal-autoshift.html)
- [Zonal autoshift: automatically shift traffic away from AZs — AWS News Blog](https://aws.amazon.com/blogs/aws/zonal-autoshift-automatically-shift-your-traffic-away-from-availability-zones-when-we-detect-potential-issues)
- [StartZonalShift — Route 53 ARC API reference](https://docs.aws.amazon.com/arc-zonal-shift/latest/api/API_StartZonalShift.html)
- [Making Gossip More Robust with Lifeguard — HashiCorp](https://www.hashicorp.com/en/blog/making-gossip-more-robust-with-lifeguard)
- Arithmetic reproducible in `04-cluster/gray-failure-budget.py`.
