# Build Roadmap: De-Risking Order

**Synthesizes:** D35
**Status:** v1 proposal

## The principle

Build in **descending order of "if this is wrong, the architecture is wrong."** Not in order
of user-visible value. Three things could invalidate the design, and all three are cheap to
test before writing a search engine:

1. Does blob CAS actually behave as assumed under contention? (M0)
2. Does the lane design actually remove the write bottleneck? (M1)
3. Does LIRE survive being batched into immutable-segment rewrites? (M4 — **prototype early,
   out of order, because it is the biggest unknown**)

## Milestones

### M0a — Local substrate (2–3 weeks, unblocked)
**Goal: find the breaking points, since we cannot yet measure the real ones.**

No cloud accounts yet, and **no emulator implements our core CAS primitive faithfully** —
MinIO rejects `If-None-Match: *`, Azurite got `If-Match: "*"` wrong until recently, SeaweedFS
breaks it under versioning. See
[`../09-rust-stack/dev-and-test-environment.md`](../09-rust-stack/dev-and-test-environment.md).

- `pstore-blob`: the `BlobStore` trait over `object_store`, with congestion control and
  per-tenant request accounting.
- The **fault-injecting store**: latency distributions, 412/409/503, delayed visibility,
  deterministic seeds. **Promoted to the primary correctness vehicle**, not a testing aid.
- **`pstore-fake-s3`** on the `s3s` crate: ~12 operations with AWS's documented semantics plus
  protocol-level fault injection (~1–2 weeks). Built **before** the sweeps, which want a
  realistic transport. Unblocks OQ-6 and the 412/409 distinction, which are testable nowhere
  else ([`../09-rust-stack/blob-store-fakes.md`](../09-rust-stack/blob-store-fakes.md)).
- **Conformance suite**: probes each backend's real behaviour and *populates* the
  `Capabilities` matrix rather than assuming it (D-100). The same suite is the contract the
  fake must satisfy, and later must satisfy *identically* against real S3 — that is what stops
  the fake from merely encoding our own beliefs.
- **Sensitivity sweeps** (D-101): rather than "what is the CAS rate?", sweep 0.5–50 CAS/s and
  find where the design breaks. Same for latency and error rate.
- Containerized WSL2 dev environment with nested resource caps; CI parity.

**Exit:** we know *what would break and at what threshold*, plus a recorded capability matrix
per backend. Numbers measured here are **relative only** (D-104).

### M0b — Real-cloud validation (deferred until accounts exist)
- Run the **same** conformance suite against real S3, GCS, Azure → complete the matrix and
  confirm the CAS premise.
- OQ-5 CAS contention curves, OQ-3 TTFB percentiles, OQ-2 `G*`, OQ-1 409 behaviour,
  OQ-6 ABA under multipart ETags, OQ-24 Express One Zone, OQ-98 NVMe endurance.
- Re-measure every number tagged provisional under D-104.

**Exit:** every "provisional" tag removed from the cost model. Because M0a produced sensitivity
curves, this is *"take five measurements and read off which regime we are in"* — days, not a
re-analysis.

### M1 — Single-node storage engine (4–6 weeks)
- `pstore-format`: segment encode/decode, footer, blocks, zone maps.
- `pstore-manifest`: HEAD, epochs, commit protocol, delta chains.
- `pstore-wal`: lanes, group commit, lane bitmap, tail probing, **cross-index bundles**
  (footer index, sort by `(index_id, shard)`) and the **memtable/freshness layer** — these are
  not a later optimization; a per-index write path would have to be rewritten (OQ-84/85).
- `pstore-cache`: `foyer` + class-aware admission.
- **Exact brute-force vector search only.** No ANN yet — it already serves the majority of
  real indexes (D-10).

**Exit:** write, read, filter, exact-search a single index on real S3. Measured
`RA(write) = 1 W` and `RA(cold read) ≤ 3 Rseq`, **asserted in tests** (D-34).

### M2 — Multi-writer correctness (3–4 weeks)
- `pstore-sim`: deterministic simulation harness (**built before the distributed features**,
  D-32).
- N concurrent writers on N lanes, one index; property tests over commit interleavings;
  injected pauses, partitions, and CAS storms; **assert Invariant I1**.
- Compaction as optimistic work + CAS-on-publish, with duplicate-work suppression.
- GC with epoch retention.
- **Bundle-recovery proof (OQ-91):** under node death, placement change, and fallback writes,
  `HEAD.lane_watermarks` + forward probing of placement lanes must find *every* un-folded
  record. If this cannot be shown, cross-index bundling is unsafe and the cost model collapses.

**Exit:** linearizability of the epoch sequence under adversarial scheduling, at 100+ logical
writers, plus the bundle-recovery proof. This is where the architecture is either proven or
disproven.

> ✅ **Delivered.** See [`docs/milestones/M2/VERIFIED.md`](../../milestones/M2/VERIFIED.md).
> **The architecture is proven, and OQ-91 failed before it passed** — recovery lost two
> acknowledged rows because a refused `PUT` still consumed a lane sequence number, turning
> one transient failure into a permanent, silent truncation of the lane. Fixed, and the
> obligation it revealed is recorded as
> [C-2](../00-plan/open-questions.md#c-2--oq-91-recovery-requires-a-dense-lane).
>
> ⚠️ **Duplicate-work suppression is NOT delivered.** Compaction is shown to be *safe*
> under concurrency, not *rare*; suppression depends on placement, which is M4. The line
> above claims it and this milestone does not.

### M3 — Vector index (5–7 weeks)
- `pstore-quant`: RaBitQ + int8 SQ + SIMD kernels (`simsimd`), rerank ladder.
- `pstore-index-vec`: SPANN-family build — hierarchical balanced clustering, boundary
  augmentation, query-aware pruning.
- Recall harness against SIFT1M/Deep1B subsets; **recall as a CI gate** (D-35).

**Exit:** 90–95% recall@10 at ≤3 round trips cold, measured, on 100M vectors.

> ✅ **Delivered, except the scale.** See
> [`docs/milestones/M3/VERIFIED.md`](../../milestones/M3/VERIFIED.md). Recall and depth are
> met — **recall@10 = 0.981 at 3 round trips cold** — at **20,000 × 384d**, not 100M.
> 100M × 768d is ~300 GB against WSL2 with no cloud account, and brute-force ground truth
> alone would exceed the gate's time budget by orders of magnitude. **The scale is NOT-RUN
> and blocked on M0b**, not on effort; it is carried to the milestone with real storage.
>
> ⚠️ **[C-3](../00-plan/open-questions.md) corrects D-11.** Rung 0 alone measures 0.30
> recall@10, not the 90–95% `quantization.md` claims. int8 rerank reaches 0.981 in the same
> three round trips, so the default rerank mode is `fast` and the budget still closes.
>
> ⚠️ **M3 also shipped violating two of the three v1 data-model properties**
> `06-indexing/modalities-and-sequencing.md` §3 requires: vectors were singular
> ("the migration trap") and the layout was one-row-one-vector (D-28: "a rewrite").
> Corrected out of order in **M3b** — see
> [`docs/milestones/M3b/VERIFIED.md`](../../milestones/M3b/VERIFIED.md) — because the cost
> of that correction grows with every milestone built on top, and M5a is where it would
> have bitten.

### M3.5 — LIRE spike (run in parallel from M1, 2 weeks of effort)
**Out of order deliberately.** Prototype LIRE-style split/merge/reassign batched into an
immutable segment rewrite, and measure partition quality against a global rebuild (OQ-51).

**Exit:** either "batched LIRE preserves quality" or "we need a different maintenance
strategy" — known *before* M3 hardens around it.

> ✅ **Delivered: "batched LIRE preserves quality."** See
> [C-4](../06-indexing/incremental-maintenance.md). Recall within 0.4 points of a rebuild,
> better balanced, 3.4× less work. ⚠️ **Run after M3 hardened, not before**, which the
> milestone was placed out of order to avoid — so the verdict arrived against code that
> already existed. It came back positive, so the cost was zero this time; that is luck, not
> process.

### M4 — Cluster (4–6 weeks)

> ⚠️ **Split, and scoped to the machine.** M4a (roster + placement, pure and deterministic)
> and M4b (gossip + a **100-node** Docker fleet, every number `provisional`) are specified;
> M4c (hierarchical membership), then M4d (cache and the post-scale-out dip) and M4e (per-AZ
> cells, gray failure) follow. ⚠️ M4c shipped as `pstore-gossip` instead — hierarchy was
> **gated out by its own criterion** once per-node cost stopped growing with the fleet. M4d is
> complete across three phases; M4e phase 1 (zone identity) is done, with placement and gray
> failure to follow. ⚠️ Membership took the M4c letter after the 1,000-node run
> measured flat gossip at ~17 of 20 cores: a cache benchmarked on a fleet whose membership
> consumes the host measures the host. The
> exit's **1,000 real nodes** is `NOT-RUN` — measured, a container costs ~1.4 MB of host
> memory so 100 is affordable, and 1,000 is not on this hardware. The split is by *evidence
> regime*: M4a's criteria are exact, M4b's are protocol-period counts on a network that is
> not a datacentre.
- `pstore-cluster`: gossip (SWIM+Lifeguard via `chitchat`/`foca`), LRH+CHBL placement,
  work assignment.
- Roster seeding from the blob store; routing hop; hedged requests.
- Simulated 10,000-node runs in `pstore-sim`; measure convergence and post-scale-out cold
  ratio.
- **Per-AZ cells** ([`../04-cluster/az-topology.md`](../04-cluster/az-topology.md)) and
  **gray-failure detection** ([`../04-cluster/gray-failure.md`](../04-cluster/gray-failure.md)):
  cross-AZ probe mesh, blob health bulletin, peer-relative outlier detection, self-eviction
  draining. Gray fault injection in the simulator is part of this milestone, not a follow-up.

**Exit:** 1,000 real nodes, 10,000 simulated; add/remove 50% of the fleet with zero data
movement and a measured, bounded cache dip.

### M5a — Sparse vectors and hybrid (2 weeks)
- Sparse/learned-sparse retrieval over the generic-impact posting lists built in M3.
- RRF fusion; exercise the `prefetch[]` + `fusion` path.
- **Before BM25 deliberately:** sparse search is *exact*, so it is a much smaller subsystem and
  gives a real hybrid story for a fraction of the work
  ([`../06-indexing/modalities-and-sequencing.md`](../06-indexing/modalities-and-sequencing.md) §7).

### M5b — Full-text (4–5 weeks)
- Tantivy behind a `BlobStore`-backed `Directory`; block-max metadata in the index section.
- BM25, two-pass IDF, trigram regex.
- MS MARCO quality evaluation; NDCG/MRR as CI gates.

### M6 — Multi-tenancy at scale (3–4 weeks)
- Sharded catalog; enumeration in one parallel round.
- Per-tenant quotas and metering, especially **blob requests** (Design rule 13).
- Synthetic 1M-index workload (OQ-72).

**Exit:** 1M indexes, open latency unaffected by index count, zero LISTs on any hot path.

### M7 — Production hardening (ongoing)
- GCS + Azure backends and the `Capabilities` matrix.
- Time travel, branching, warm API, streaming responses.
- Observability, SLOs, chaos testing, Jepsen-style verification.
- BYOC packaging.

## Cross-cutting, from day one

| Discipline | Enforcement |
|---|---|
| Round-trip depth ≤3 | Test assertion against the fault-injecting store (D-34) |
| Zero LIST on hot paths | Lint + the awkward `list_unrestricted` API name (D-2) |
| Blob requests per op | Counter assertions in tests; per-tenant metrics in prod |
| Recall / NDCG | CI gates (D-35) |
| Invariant I1 | Deterministic simulation (D-32) |

## Rough sequencing

```
M0 ──▶ M1 ──▶ M2 ──▶ M3 ──▶ M4 ──▶ M5 ──▶ M6 ──▶ M7
        └─────── M3.5 LIRE spike (parallel) ──┘
```
~7–9 months to a credible single-cloud beta with one engineer-equivalent of focus; faster with
parallelism on M3/M5, which are largely independent of M2/M4.

## The three questions that decide whether to continue

0. **Before M1:** what is the real tenancy distribution (OQ-84)? If most indexes turn out to
   be large and continuously written, cross-index bundling is over-engineering and the simpler
   per-index path is fine. This is a customer-discovery question, not an engineering one, and
   it is cheap to answer first.
1. **After M0a:** at what CAS rate does the design break? (A threshold, not a measurement —
   and more useful, because it says how much headroom we have.) **After M0b:** which side of
   that threshold is reality on? If a single key can't sustain
   even a few writes/second reliably, the partitioned-register plan needs rethinking.
2. ~~**After M2:** does the simulation find correctness bugs we cannot close? A masterless
   design that needs a lock service is just a worse design with extra steps.~~
   **ANSWERED: no.** The simulation found four real defects — a lane truncated by a
   consumed-but-unwritten sequence number, a compaction that swallowed a concurrent fold,
   an unbounded probe loop, and an untested GC guard — and every one was closed without
   introducing a lock, a lease, or a leader. CAS on a blob remained sufficient throughout.
   ⚠️ `provisional`: the harness models a writer that stops and a store that refuses. It
   does not reproduce a kernel, a socket, or a machine losing power mid-`PUT`.
3. **After M3.5:** does incremental clustering work on immutable storage? If not, the fallback
   is periodic full re-clustering during compaction — more expensive, still viable, but it
   changes the cost model.
