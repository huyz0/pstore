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

> ⚠️ **COMPLETE**, across five milestones each with a `VERIFIED.md`: M4a (roster,
> placement), M4b (membership, the Docker fleet), M4c (`pstore-gossip`), M4d (caching),
> M4e (per-AZ cells, gray failure).
>
> Three things went differently from this plan, and each is recorded where it happened:
>
> * **M4c was going to be hierarchical membership; it became our own gossip.** Hierarchy was
>   **gated out by its own criterion** — a checksum cannot be bolted onto Scuttlebutt, because
>   its digest carries a heartbeat that every node increments every round, so the cluster state
>   never reaches a fixed point. Taking liveness from message arrival instead is SWIM, which is
>   what D-4 specified before a licence sent us to `chitchat`. Per-node cost went from 406 KB/s
>   at 1,000 nodes to **74 bytes per round, flat to 10,000**, at which point a tier of delegates
>   was solving a problem that no longer existed.
> * **1,000 real nodes ran**, contradicting the note that used to stand here. Flat gossip
>   converged at that size but cost ~17 of 20 cores; the replacement costs 1.9.
> * **The 10,000-simulated exit criterion is withdrawn**, not deferred: it would have measured
>   a simulator. `pstore-gossip`'s cost example runs the **real protocol** in-process at 10,000
>   nodes instead, which measures message sizes and round counts exactly and claims nothing
>   about CPU.
- `pstore-cluster`: gossip (SWIM+Lifeguard via `chitchat`/`foca`), LRH+CHBL placement,
  work assignment.
- Roster seeding from the blob store; routing hop; hedged requests.
- Simulated 10,000-node runs in `pstore-sim`; measure convergence and post-scale-out cold
  ratio.
- **Per-AZ cells** ([`../04-cluster/az-topology.md`](../04-cluster/az-topology.md)) and
  **gray-failure detection** ([`../04-cluster/gray-failure.md`](../04-cluster/gray-failure.md)):
  cross-AZ probe mesh, blob health bulletin, peer-relative outlier detection, self-eviction
  draining. Gray fault injection in the simulator is part of this milestone, not a follow-up.

**Exit:** met, with one criterion withdrawn.

| Exit criterion | Status |
|---|---|
| 1,000 real nodes | **met** — converged, all 1,000 holding a full view ([M4b](../../milestones/M4b/VERIFIED.md)) |
| 10,000 simulated | ⚠️ **withdrawn** — it would measure a simulator; the real protocol is run in-process at 10,000 instead ([M4c](../../milestones/M4c/VERIFIED.md)) |
| add/remove 50% of the fleet, zero data movement | **met** — `a_fleet_change_copies_nothing` ([M4a](../../milestones/M4a/VERIFIED.md) criterion 7) |
| a measured, bounded cache dip | **met** — sequential depth 2 → 1 on the first query ([M4d](../../milestones/M4d/VERIFIED.md) criterion 15) |

⚠️ **Not built, and named rather than omitted:** the NVMe cache tier (D-23), so a rolling
restart still flushes every cache; and the cross-AZ probe mesh (D-82) and blob health bulletin
(D-83) as running subsystems — the gray-failure *decisions* ship, their transports need a
query path and a real multi-AZ deployment.

### M5 — Retrieval beyond dense

> ⚠️ **COMPLETE**, across three milestones each with a `VERIFIED.md`: M5a (sparse postings and
> the impact encoding), M5b (`prefetch[]` + `fusion`), M5c (BM25 and two-pass IDF).
>
> Four things went differently from this plan, and each is recorded where it happened:
>
> * **M5 was two milestones; it is three.** Fusion was going to ride along with sparse, and
>   spec review returned four blocking findings against that shape. A change that cannot
>   survive two review rounds is one that gets amended mid-implementation.
> * **D-14 is declined** — full-text is native, not Tantivy behind a `Directory`. M5a had
>   already built the posting codec, the impact payload, the sidecar pattern and the compaction
>   round trip, so what was left was what D-72 says is left: a scorer plus a tokenizer.
>   **[C-11](../06-indexing/full-text-search.md) is the first correction banner here recorded
>   as an argument rather than a measurement**, and says what would overturn it.
> * **The corpus's term-dictionary placement does not survive its own arithmetic.** A
>   SPLADE-sized vocabulary in the index section is 88× `INDEX_BUDGET`, and `try_finish`
>   *refuses* an over-wide segment — so the letter of it does not make the open slow, it makes
>   the segment unwritable. [C-10](../06-indexing/full-text-search.md).
> * **Two open questions came back with numbers**, and one of them contradicted its own
>   milestone's assumption. OQ-126: u8 impacts are 2.08× smaller than f32 at 0.9910 top-10
>   agreement, so u8 ships. OQ-64: per-segment IDF returns a different **top-1** from global
>   IDF on **27 of 37** queries when two segments are unlike each other.

**Exit:** met.

| Exit criterion | Status |
|---|---|
| Sparse retrieval over generic-impact postings | **met** — exact candidates, exact f32 ranking ([M5a](../../milestones/M5a/VERIFIED.md)) |
| `prefetch[]` + `fusion` exercised (D-73) | **met** — three legs, one open, depth is the max not the sum ([M5b](../../milestones/M5b/VERIFIED.md), [M5c](../../milestones/M5c/VERIFIED.md)) |
| BM25 with two-pass IDF (D-30) | **met** — agrees with an independent implementation to 1e-4; global IDF changes the top-1 ([M5c](../../milestones/M5c/VERIFIED.md)) |
| Ranking quality as a CI gate (D-31) | **met** — `scripts/ndcg.sh`, which also scores a control ranker and fails if that clears the floor |
| MS MARCO evaluation | **met** — MRR@10 **0.1775**, NDCG@10 **0.2241** over 6,980 dev-small queries against 8,841,823 passages in 23 shards, versus the published BM25 reference of ≈0.18 ([M5i](../../milestones/M5i/VERIFIED.md)). ⚠️ `provisional`: WSL2, one run. ⚠️ This row read "no network and no dataset here" and **both halves were false when re-tested** — one range request to the official corpus settled it |

⚠️ **Not built, and named rather than omitted:** block-max pruning (OQ-45), which
`modalities-and-sequencing.md` §6 lists as a **prerequisite** for deferring FTS safely —
segments are immutable, so every segment M5c writes is permanently unprunable; a **stable
cross-segment identity** and the multi-segment caller that two-pass IDF is built for; a query
parser, language analyzers, phrase queries, positions and trigram regex.

### M5a — Sparse vectors (2 weeks)
- Sparse/learned-sparse retrieval: the generic-impact posting lists D-72 reserved, built for
  real. ⚠️ The lists M3 built are the **dense** index's; the inverted index did not exist.
- **Before BM25 deliberately:** sparse search is *exact*, so it is a much smaller subsystem and
  gives a real hybrid story for a fraction of the work
  ([`../06-indexing/modalities-and-sequencing.md`](../06-indexing/modalities-and-sequencing.md) §7).

> ⚠️ **Split into M5a and M5b, and full-text renumbered to M5c.** Fusion was going to ride
> along with sparse. It is six commits of layout, engine and retrieval before a line of ranking
> exists, and the corpus's own sequencing argument — sparse first *because* it is small —
> applies again one level down. See [`docs/milestones/M5a/SPEC.md`](../../milestones/M5a/SPEC.md).

### M5b — `prefetch[]`, `fusion`, and RRF (3 days)
- The D-73 request shape as Rust types, with the retriever it does not have yet refused **by
  name** rather than dropped.
- RRF at `k = 60` (D-27); weighted fusion named and not defaulted.
- The claim worth testing: two legs cost the **max** of their depths, not the sum.
  → [`docs/milestones/M5b/SPEC.md`](../../milestones/M5b/SPEC.md)

### M5c — Full-text (4–5 weeks)
→ [`docs/milestones/M5c/SPEC.md`](../../milestones/M5c/SPEC.md)
- Tantivy behind a `BlobStore`-backed `Directory`; block-max metadata in the index section.
- BM25, two-pass IDF, trigram regex.
- MS MARCO quality evaluation; NDCG/MRR as CI gates.

### M6 — Multi-tenancy at scale (3–4 weeks)
- Sharded catalog; enumeration in one parallel round.
- Per-tenant quotas and metering, especially **blob requests** (Design rule 13).
- Synthetic 1M-index workload (OQ-72).

**Exit:** 1M indexes, open latency unaffected by index count, zero LISTs on any hot path.

| Exit criterion | Status |
|---|---|
| 1M indexes | **met** — 1,000,000 tenants × 50 index names seeded, folded and enumerated ([M6i](../../milestones/M6i/VERIFIED.md)) |
| Zero LISTs on any hot path | **met** — the request-class counter reads 0 after seeding, folding, the census, and every open |
| Open latency unaffected by index count | ⚠️ **met across tenants, NOT met within one tenant.** Round trips are flat everywhere — 3 reads at 1 index and at 5,000, at 1 tenant and at 1,000,000. **Bytes are not**: HEAD is one object read whole, so a tenant pays ~**106 bytes per index it owns** on every open of every *other* index, and at 5,000 indexes 98.6% of an open is manifest. Measured, pinned by a test, and deliberately not fixed here ([M6i](../../milestones/M6i/VERIFIED.md)) — and **deliberately not fixed at all**, since [M7b](../../milestones/M7b/VERIFIED.md) measured the trade: 106 bytes an index is **0.05 ms** at K=50 against the **30 ms** round trip any layout avoiding a whole-manifest read must add, so the crossover is K ≈ 29,677 against a product ceiling of 50 |
| Enumeration cost independent of tenants | **met** — 32,769 reads at 200,000 tenants and at 1,000,000, both exactly `2 × width + 1` |

⚠️ **So M6's exit is not fully met**, and the unmet half is a byte cost with a number rather
than an unknown. `provisional`: WSL2, a `MemoryStore`, one run.

> ⚠️ **Split into M6a and M6b**, on the same grounds M5 was: the three bullets share a subject
> and nothing else. Metering is a `pstore-blob` decorator with no catalog in it; the workload
> needs the catalog to exist before it can measure anything.

#### M6a — The catalog
→ [`docs/milestones/M6a/SPEC.md`](../../milestones/M6a/SPEC.md) ·
[`VERIFIED.md`](../../milestones/M6a/VERIFIED.md)

**Done.** `pstore-catalog`: derived bucket keys, a CAS'd pointer per bucket carrying pending
records, immutable runs named by epoch **and content digest**, enumeration in **two rounds**
(pointers, then runs) at a request count that is a function of width and not of tenants, and
**zero LIST** anywhere in the lifecycle. 85 of 85 viable mutants caught.

⚠️ **[C-12](../03-metadata-consistency/catalog-without-master.md) — the per-lane change log is
not built and its "lanes again" reuse could not have worked.** The engine's lanes are
node-scoped, so a reader needs a registry object to learn which exist: a second mutable object
per bucket and a third sequential round in every enumeration. Pending records in the bucket's
own pointer cost a CAS on the append path — affordable only because an append is a
tenant-lifecycle event, which `Appender::observe` is what enforces — and save `LOG_LANES ×
window` probes per bucket on every enumeration, 524,288 requests at `DEFAULT_WIDTH` against
16,384.

⚠️ ~~**Not measured at 1M.** The invariant is measured at 2,000 tenants; 1M is arithmetic on
it.~~ **Measured since [M6i](../../milestones/M6i/VERIFIED.md)**: 32,769 reads at 1,000,000
tenants, the same as at 200,000. ⚠️ And ~~bucket splitting is unbuilt~~ — **it is built**:
[M6g](../../milestones/M6g/VERIFIED.md) splits a bucket and
[M6h](../../milestones/M6h/VERIFIED.md) re-partitions a catalog that has drifted, both of them
after this line was written. OQ-8's remaining half is the **threshold** — what should trigger a
split — not whether one can happen.

#### M6b — Quotas and metering
→ [`docs/milestones/M6b/SPEC.md`](../../milestones/M6b/SPEC.md) ·
[`VERIFIED.md`](../../milestones/M6b/VERIFIED.md)

~~Not started.~~ **Done.** Design rule 13, built on the per-tenant counters `pstore-blob`'s
`Accounted` already keeps: a quota that **refuses** rather than merely counts, and that is
transparent when unlimited.

#### M6i — One million indexes, measured
→ [`docs/milestones/M6i/SPEC.md`](../../milestones/M6i/SPEC.md) ·
[`VERIFIED.md`](../../milestones/M6i/VERIFIED.md)

**Done, and it scores the exit table above rather than claiming it.** `scripts/scale.sh` — not
a gate, because the catalog arm peaks at 9.28 GB.

### M7 — Production hardening (ongoing)

> ⚠️ **"Ongoing" is not a milestone.** Four bullets and no exit criterion, which the first
> non-negotiable in `AGENTS.md` refuses. Each bullet gets its own spec; **M7a** is the first.

#### M7a — The capability matrix, measured
→ [`docs/milestones/M7a/SPEC.md`](../../milestones/M7a/SPEC.md) ·
[`VERIFIED.md`](../../milestones/M7a/VERIFIED.md) ·
[`docs/profiles/capability-matrix.md`](../../profiles/capability-matrix.md)

**Done.** D-100 honoured for the two primitives a probe can settle: `scripts/conformance.sh`
runs the ten-probe suite against MinIO, Azurite and fake-gcs-server and checks the matrix back
in, `--check` fails when a backend changes under us, and a backend whose profile cannot fence
is now **refused** at every door and at every CAS in `pstore-engine` and `pstore-catalog` —
a rule three documents stated and nothing enforced. Closes M0a.12 and M0a.13.

#### M7b — the four rows the backlog left open
→ [`docs/milestones/M7b/SPEC.md`](../../milestones/M7b/SPEC.md) ·
[`VERIFIED.md`](../../milestones/M7b/VERIFIED.md)

**Done.** Not a subject but a provenance: each row is a previous milestone reporting something
it found and did not fix, and after this the carried-forward list is empty. `review.sh`'s round
counter no longer charges a change for rounds spent on another one; `Split::head` has the
caller that kills its mutant; **`BlobStore::get_tag` is fallible**, so a refused probe is no
longer indistinguishable from an absent object and M0c's refusal axis reaches the commit loop's
only read; and row 19 is closed **by measuring the trade rather than by building the layout**.

⚠️ **Two corrections came out of measuring rather than reading**, which is the whole argument
for a measured matrix: [C-13](../09-rust-stack/dev-and-test-environment.md) — MinIO's
`If-None-Match: *` works now, and the reason to distrust it is ABA instead — and
[C-14](../02-object-storage/request-efficiency-patterns.md) — **Azure has no suffix range at
all**, so Pattern 6's cold open is 3 round trips there, not 2.

#### M7c — the first door
→ [`docs/milestones/M7c/SPEC.md`](../../milestones/M7c/SPEC.md) ·
[`VERIFIED.md`](../../milestones/M7c/VERIFIED.md)

**Done.** `pstore-server`: the index is the noun, a write is batched or durable and never
downgraded, every response reports what it cost and how fresh it is, and a tenant is a required
header that is never defaulted. ⚠️ **The criterion that matters is the second instance**: a
durable write, folded, read back by a *different* server over the same store with an empty
memtable — every other criterion is satisfiable by a process holding everything in RAM.

⚠️ **Deliberately not**: authentication, quotas, a scheduler, and the schema — M7c makes the
caller exist, **M7d** asks it the policy questions [M6c](../../milestones/M6c/VERIFIED.md)
could not. ⚠️ And it found what only a caller finds: a **wrong-dimension query returned
scored, ranked results with a `200`**, because the dense leg took the index's dimension from
the query rather than from the segment.

#### M7d — the schema
→ [`docs/milestones/M7d/SPEC.md`](../../milestones/M7d/SPEC.md) ·
[`VERIFIED.md`](../../milestones/M7d/VERIFIED.md)

**Done.** M6c's three questions, answered now that [M7c](../../milestones/M7c/VERIFIED.md) has
built a caller to ask: **who sets it** — the first fold, by inference, so nothing precedes a
write; **may it change** — no, and `PATCH` refuses with the migration path named rather than
404ing; **what a disagreement does** — refused at the door and at the flush, and if it ever
reaches a fold, **dropped and counted rather than allowed to stop the tenant**.

⚠️ That last clause is the milestone. Spec review measured the first draft: refusing a
contradicting fold would have stopped every later fold for the whole tenant, forever, because a
fold is all-or-nothing across its bundle set. One accepted API call would have bricked a tenant.

⚠️ `Head` gains an **optional trailing section**, so every HEAD written before this milestone
still decodes — with the price asserted rather than tolerated: exactly two truncations now
decode, and a test enumerates every one to prove there is no third.

#### M7e — time travel
→ [`docs/milestones/M7e/SPEC.md`](../../milestones/M7e/SPEC.md) ·
[`VERIFIED.md`](../../milestones/M7e/VERIFIED.md)

**Done, and it cost nothing.** OQ-82 asked whether epochs should be public; they are, and
`as_of` turned out to need no version store at all — **a segment's key already carries the
epoch it was born at, and the graveyard already records every burial**, so a past manifest is
arithmetic over the present one. Zero extra writes on the commit path, one HEAD read on the
query, bounded by exactly the retention GC already enforces and refused outside it rather than
answered short.

⚠️ Making the claim true required repairing the invariant underneath it: `compact` derived its
output key **once** and committed it later, so a contended compaction stamped a segment with an
epoch it was not live at — and reconstructing that epoch returned the merge *and* its inputs.

⚠️ **Not built, and argued rather than asserted**: branching (needs a copy-on-write manifest and
a name that is not a tenant's HEAD), the warm API (needs the NVMe tier D-23 calls mandatory and
M1.13 blocks on a real device — a warm endpoint over no cache is a lie with a 200), and
streaming responses (OQ-83, protocol work whose value is RAG UX rather than correctness).

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
