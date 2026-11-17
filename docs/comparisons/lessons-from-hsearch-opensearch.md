# Lessons for pstore from hsearch, the OpenSearch serverless branches and lucene-rust

**Snapshot:** 2026-11-17. **pstore:** `bce14ef` (main after M272). **Scope:** recommendations
only. Nothing here is built, and nothing here claims a pstore measurement.

Three sibling projects attack the same problem as pstore from other directions:

- **OpenSearch serverless** (`huyz0/OpenSearch`): five branches turn OpenSearch into a
  blob-store-authoritative engine. `feature/serverless` and `feature/serverlessplusplus`
  run it as a plugin, `feature/serverlessshell` as a separate "shell" with a per-node write log,
  `low-latency-serverless` adds a Raft journal so a write is acknowledged before any S3
  request, and `features/rbs-cost-saving` packs remote-store uploads into tar bundles.
- **hsearch** (`huyz0/hsearch`): a Rust store over lucene-rust. It is mostly planning plus
  evidence so far: fenced commit lanes, provider conformance, tenant fairness, a cost
  calibrator and KMS accounting.
- **lucene-rust** (`huyz0/lucene-rust`): a Rust port of Lucene, with benchmarks for IVF, disk
  HNSW and quantizers measured against Lucene and Faiss.

Every number below is quoted from those repositories. All of them were measured on 4-vCPU
cloud VMs, RustFS or a filesystem stand-in, so they are **`provisional`** and relative, as
pstore's own rules require. Links point at the branch that holds each file.

## Summary, ranked by value

| # | Lesson | Kind | Size | pstore today |
|---|---|---|---|---|
| 1 | Bundle writes across tenants, not only across one tenant's indexes | adopt, as a setting | L | Per tenant: `bundle_key(tenant, lane, seq)`, and D-41's cohort fan-in was never built |
| 2 | Make request budgets ratchet both ways | adopt, gate | S | `check-slos.py` checks status and gate, not "below budget" |
| 3 | Run a concurrent one-winner probe against every real backend | adopt, gate | S | `conformance.rs` probes one writer; the race test runs on the memory store only |
| 4 | Weighted tenant admission with a reserve, plus per-tenant cache floors | adopt, as a setting | M | Fixed 8-of-64 tenant cap; the cache has per-class quotas only |
| 5 | Audit every reap path against every pin kind | adopt, review and test | S | Pins: snapshots (M236), branches and copies (M271), `as_of`, replicas |
| 6 | Pin the S3 client's retry policy and never repeat a write that got a status | adopt, check | S | `AmazonS3Builder` keeps `object_store`'s defaults |
| 7 | IVF tuning from lucene-rust: centroid cost, a final Lloyd round, filtered probing | adopt, measure first | M | IVF + RaBitQ/exq/sq8 already (`pstore-index`) |
| 8 | Measure write throughput with an open-loop load ladder | adopt, bench | S | Closed-loop ingest numbers only (for example M119) |
| 9 | Copy a warm cache from peers when a node joins | consider, as a setting | M | M4d phase 3 specifies metadata-only warming from the blob store |
| 10 | Do not add a consensus journal for low-latency acknowledgement | avoid | n/a | Correct today: no leader, no quorum |
| 11 | Never infer "latest" from a listing in GC | avoid; pstore already complies | n/a | `reap_plan` works from the HEAD |
| 12 | BYOK decides lane sharing; plan it with workstream I | input to I | n/a | KMS is out of scope until accounts exist |

Items 1, 4, 7 and 9 change on-disk or runtime behaviour, so under the project rule each one is a
setting with a cluster default and a per-index (or per-tenant) override. Each picks its default
from a measurement, and its tests pin the old mode unchanged while new tests cover the new
one. Items 2, 3, 5, 6 and 8 are gates, probes or benchmarks and change no behaviour.

## 1. Bundle writes across tenants (highest value)

**What pstore does.** A WAL bundle carries many indexes, but all of them belong to one tenant.
The key is `{tenant}/wal/{lane}/{seq}.bundle` (`crates/pstore-engine/src/lib.rs`,
`bundle_key`), and the lane registry is per tenant (`crates/pstore-engine/src/lanes.rs`,
`key`). The research proposed fanning writes into a cohort of nodes, so that one PUT carries
many tenants (D-41,
[`tenancy-scale-model.md`](../research/10-benchmarks-cost/tenancy-scale-model.md) §3). No code
implements that: `cohort` appears only in the ingress limiter. So when 1,000 sparse tenants
each write once in a flush window, the node makes 1,000 PUTs.

**Evidence that sharing pays.**

- hsearch measured fenced lanes on RustFS with 1M active indexes, `steady_small` load:
  **0.968 commit PUTs per write with a lane per index, and 0.199 with shared lanes** (5.0
  records per slot). Under `write_storm` the figures were 0.995 and 0.175
  ([m03-08-lanes.md][hs-lanes]). The model it checked predicted 0.247, and the measurement
  came in 4% under it ([m03-09-lane-sharing-model.md][hs-model]).
- OpenSearch's shell writes one log per node: one blob per 250 ms window holding 250 to 2,500
  documents. That is **about 3.9 log PUTs a second at any write rate**, and 15 to 50 PUTs a
  second in all, at 1,000 to 10,000 docs/s over 2,000 shards
  ([write-throughput-results.md][os-tput]).

**Downsides, recorded by both projects.**

- *Blast radius.* One object holds many tenants. A reader can still fetch only its own slice
  by range, as pstore's bundle index already allows.
- *Hot tenants.* hsearch promotes a lane with at least 1,000 records and a quarter of the
  window's bytes to a lane of its own. Promotion costs 3 PUTs and 18.6 ms on RustFS.
- *Encryption.* A tenant with its own key cannot share an object without per-range
  encryption, which is open question OQ-87. hsearch's BYOK tenants pay the full
  per-index price (0.97 to 0.999 PUTs per write). See item 12.
- *Replay.* A recovering reader must find its tenant's slices in cohort lanes it did not
  write. D-41 derives the cohort from the tenant id, so no LIST is needed, but every replay
  pays a fetch of the bundle index.

**Recommendation.** First, a measurement milestone (M277 or later) that counts pstore's PUTs
per write and per tenant on a sparse multi-tenant workload. Without that number the default
cannot be chosen. If it confirms hsearch's ratio, build `wal_bundling = tenant | cohort` with
`tenant` (today's behaviour) as the cluster default until the measurement shows `cohort` wins
in most cases. The per-index override opts an index back into its tenant's own lane, which is
also the BYOK path. Tests keep every existing lane test on `tenant` and add a cohort suite.

## 2. Request budgets that ratchet both ways

OpenSearch's shell keeps a table of request and round-trip budgets per lifecycle step (first
write, next write, publish, idle hour, cold and warm search). Each row is `exact` or `max`.
**An `exact` row fails when a measurement comes in below its budget as well as above it**, so a
saving must be written down and cannot be lost silently later
([request-budgets.md][os-budgets]). pstore already has the enforced-or-blocked rule
(`scripts/check-slos.py`) and depth gates (`scripts/depth.sh`), but a request count that
improves does not tighten anything.

**Recommendation.** Add an `exact` or `max` bound column to the SLO table and have each gate
read its number from that table rather than repeat it, as `RequestBudgetTableTests` does. This
is rung 3 of the gate-design ladder: a predicate over files. No setting is needed.

The same table shows what pstore gets right. An idle, held OpenSearch index costs 16 requests
an hour (head checks, descriptor refresh, sweeps), and its "zero" target is `blocked`. pstore's
rule against per-index, per-elapsed-time requests is what keeps it at zero.

## 3. A concurrent one-winner probe against real backends

Both projects rest their safety on one property: concurrent conditional writes to one key admit
exactly one winner. OpenSearch's shell has a linearizability checker that passes on MinIO and
SeaweedFS. Reading the AWS contract also turned up two race outcomes, a `409` under concurrency
and a `404` when a delete beats an `If-Match`. Its S3 register had mapped both to exceptions
rather than conflicts ([README.md, "The open risk"][os-shell]). hsearch's provider suite runs
`one_winner_among_concurrent_updates` with 8 threads against every provider
([m03-06-conformance.md][hs-conf]).

pstore's `crates/pstore-testkit/src/conformance.rs` probes create-if-absent, CAS and ABA
resistance with one writer each. The race test, `concurrent_creates_have_one_winner`
(`crates/pstore-engine/tests/it/create.rs`), runs only on the memory store.

**Recommendation.** Add a concurrent create probe and a concurrent CAS probe to
`conformance.rs`, so they run wherever the suite runs (MinIO, GCS emulator, RustFS). Add a
probe that races a delete against an `If-Match` write. `object_store_backend.rs` already maps
409 and 412 for the retry policy, and the probe pins the 404 case as well. Size S.

## 4. Weighted tenant admission and per-tenant cache floors

pstore admits at most 8 active requests per tenant out of 64 per node
(`crates/pstore-server/src/ingress.rs`, `tenant_active`). That protects small tenants, but a
hot tenant can never use idle capacity, and eight hot tenants fill the node. The cache has a
quota per class, so a bulk burst evicts only bulk (D-21). It has none per tenant
(`crates/pstore-cache/src/lib.rs`), so one tenant's cold scan evicts other tenants' entries in
the same class.

hsearch modelled both ([m13-02-fairness.md][hs-fair]):

| admission, 1 hot tenant among 999 small | small tenants refused | hot tenant served | utilisation |
|---|---|---|---|
| one global cap | 76.26% | 23.8% | 100.0% |
| weighted share, reserve 10% | 0.00% | 17.9% | 91.7% |
| weighted share, reserve 25% | 0.00% | 14.2% | 76.7% |

The reserve is idle capacity held for tenants that may arrive, so it is a setting, not a
constant. In a cold-cache storm, one shared LRU kept small tenants at an 82.5% hit ratio. A
floor of 16 KiB and a ceiling of 512 KiB per tenant kept them at 100%.

**Recommendation.** A per-tenant share of in-flight capacity that may borrow while a reserve
stays free (`reserve_permille`, cluster default, with a per-tenant weight), and a per-tenant
cache floor and ceiling inside each class. Today's fixed cap is the `reserve = cap` special
case, so it stays the default until a mixed-tenant benchmark shows the weighted mode wins. Note
the downside hsearch recorded: no preemption, so a tenant owed its share waits for running work
to finish.

## 5. Audit every reap path against every pin kind

OpenSearch's correctness review found five defects. **Three of them had one shape: a guard
that existed on the snapshot path was never mirrored onto the point-in-time path**, so a sweep
deleted files a freeze was still reading. Every finding came from probing rather than reading,
and the last came from grepping every consumer of the pin set instead of reading the next file
([STATUS.md, "What a correctness review found"][os-status]).

pstore now has several pin-like mechanisms: snapshots (M236), branches and copies (M166, M271),
`as_of` reads, replicas (M22) and the reap horizon itself (`reap_horizon` and `gc_through` in
`crates/pstore-engine/src/lib.rs`).

**Recommendation.** One table-driven test that creates each pin kind, runs GC past its
horizon, and checks that nothing pinned is reaped. A new pin kind must then add a row to the
table, or the test fails. Size S, no setting.

## 6. Pin the S3 client's retry policy

hsearch's adapter retries reads after 429 or 5xx, but **retries a write only when no
connection was made**. A 503 does not prove that a write did not commit, so a write that got
any status is handed back to the caller, which replays it by request id. Its first version
retried writes after 429 and 503, and review narrowed it ([m03-12-retry-attribution.md][hs-retry]).
The same project found that RustFS accepts an `If-Match` update from a stale version when the
bytes are unchanged, because the ETag is a content hash ([m03-11-fencing.md][hs-fence]). pstore
already guards that ABA case: see the head's ABA guard in `crates/pstore-engine/src/head.rs`.

pstore's own layer is careful. `congestion.rs` retries only `SlowDown` and never retries
`put_conditional`, and M17 treats a lost acknowledgement on a bundle as its own write
(`crates/pstore-engine/tests/it/lane_taken.rs`). However, `crates/pstore-server/src/main.rs`
builds `AmazonS3Builder` without a retry configuration, so `object_store`'s built-in retries
also run underneath pstore's.

**Recommendation.** Set the client's retry configuration explicitly. Add a test with a backend
that commits and then answers 503: the head CAS and the bundle create must each end as their own
write, never as `Lost` or as a second writer. Size S.

## 7. IVF tuning from lucene-rust

pstore already made the choice lucene-rust's numbers support. On blob storage, a probe is one
sequential range read, while an HNSW search makes hundreds of dependent random reads. lucene-rust
measured IVF-BBQ at 3 to 10 times the speed of HNSW+BBQ at equal recall, using a third to nine
tenths of the pinned memory ([ivf.md][lr-ivf]). Prefetching every probed cluster takes **2 round
trips a query against 45 to 111** when each cluster is read on its own ([disk-hnsw.md][lr-dhnsw]).
Four findings may still apply to `pstore-index`. Each needs a pstore measurement before it
becomes a setting:

- **Centroid ranking is a fixed cost.** On fastText-300, ranking 6,437 centroids took about
  330 µs a query, a third of the latency at recall 0.93. Reduced-precision centroids or a
  two-level centroid tree would cut it. Measure what share of pstore's query time goes to
  centroid scoring at its largest cluster counts.
- **A full Lloyd round after the balanced or hierarchical k-means** moved about one vector in
  ten to its nearest centroid, and raised recall from 0.949 to 0.955 at a fixed nprobe. It
  costs about 1 s a round on GloVe and 20 s on fastText. `crates/pstore-index/src/cluster.rs`
  uses capacity-bounded k-means, so compare recall at a fixed nprobe with and without an
  unconstrained final round. It is a build-time setting because it costs build time.
- **Filtered probing.** Grow probes until about 2,000 accepted vectors have been scanned for
  k = 10, skip clusters holding no accepted vector, and switch to an exact scan when that is
  cheaper (8 code scans count as one exact score). A 300-document tenant of 390k went from
  1,192 µs to 30 µs.
- **SOAR spilling gave no recall gain per entry scanned**, so lucene-rust leaves it off. Do not
  build it without a measurement that disagrees.

## 8. An open-loop write-throughput ladder

OpenSearch's old soak test ran 16 closed-loop writers, so it measured latency and request
cost, never throughput. An open-loop ladder (fixed arrival rate, latency counted from arrival,
stop at the first step under 95% acknowledged or over 5 s p99) found the real limits
([write-throughput-results.md][os-tput]):

- about 1,000 single-document writes a second per core, or 3,300 documents a second per core
  in bulks of 10;
- an in-flight cap that refused bursts until it was scaled with the core count;
- one serial log flusher that encoded the next blob before sending its PUT. At 20,000 docs/s a
  blob took 263 ms to encode against a 31 ms PUT, and encoding the next blob while the previous
  PUT is in flight would lift that ceiling;
- per-shard admission checks run one after another, which stalled a window for 24 s on a
  node's first writes to 3,000 shards.

**Recommendation.** A `scripts/write-ladder.sh` outside `cargo test` (for `depth.sh`'s reason)
that offers fixed rates to a real `pstore-server` and reports docs/s per core at the knee. Then
check whether pstore's flush overlaps encoding with the PUT in flight. Size S.

## 9. Warm a joining node from its peers

When a search node joins OpenSearch's shell, it first copies from its peers the cache blocks of
the shards it will be ranked first for, hottest first, capped at 128 MB/s. It turns ready
after that, or after 3 minutes. A draining node hands its shards to their next readers before it
stops ([README.md, "Scaling search nodes without a cold cache"][os-shell]). On a cold join the
first search on a shard made 11 to 17 block reads and took about 178 ms. pstore's M4d phase 3
specifies warming metadata only, from the blob store (D-44).

**Consider** a peer copy over the existing peer transport as `warm_from_peers` (cluster default
off, per-index override). It turns a scale-out's blob GETs into intra-AZ transfer. The cost is
peer bandwidth during a scale event, and a readiness delay bounded by `max_wait`. Measure the
post-scale-out dip both ways before choosing the default.

## 10. Avoid: a consensus journal for low-latency acknowledgement

`low-latency-serverless` adds Raft groups (Apache Ratis) so that a write is acknowledged before
any S3 request. Its consensus ADR runs to 79 KB, and its plan states plainly that quorum
persistence, refresh and queueing remain real costs, so single-digit-millisecond end-to-end
latency is not promised ([low-latency-serverless-plan.md][os-ll]). hsearch's cost calibrator
puts its two journal profiles about USD 495 a month apart on a busy load. The difference is
exactly the cross-AZ copies, and the journal's own volumes and compute are not priced
([m02-07-cost-calibration.md][hs-cost]).

pstore's rules already exclude this: no lock, lease or leader election for correctness, and
nodes own nothing. **Keep it that way** unless a customer needs acknowledgement faster than one
PUT. Even then, a group-commit window is the dial to turn first: hsearch found a 1 s window costs
26% of a 250 ms window on busy load.

## 11. Avoid: inferring "latest" from a listing in GC

`feature/serverlessplusplus` fixed five data-loss paths in one review. The first one was that
GC never read the shard head and took "latest" from a listing, while the publisher writes the
manifest before it swaps the head. A crash in that gap made the live commit look superseded and
collectable (commit `a8d685244`, [serverless-storage README][os-plugin]). pstore reaps from the
HEAD's own plan (`reap_plan(h: &Head, …)`), and its rule against LIST on any path keeps it there.
Recorded as a confirmation, so that nobody "simplifies" GC into a listing later.

## 12. BYOK decides lane sharing: an input for workstream I

When KMS arrives (workstream I), lane sharing and encryption become one decision. hsearch's
model at 10M indexes, 10% active ([m03-09-lane-sharing-model.md][hs-model]):

| BYOK share | requests per write, own lane for BYOK | requests per write, shared lane with per-index payload keys |
|---|---|---|
| 5% | 0.487 | 0.455 |
| 25% | 0.808 | 0.653 |
| 50% | 1.212 | 0.905 |

The KMS path costs **1 wrap per new object and 0 calls per warm read**, and 64 concurrent cold
readers of one object make 1 unwrap. A revoked key costs one unwrap per attempt, so revocation
needs a negative cache ([m05-08-kms-path-accounting.md][hs-kms]). If item 1 ships first, its
per-index override is already the "own lane" path these numbers price.

## Smaller lessons worth keeping

- **"If something looks like a hang, measure."** A `static` memo keyed by index uuid pinned one
  metadata object per index for the life of the JVM. It turned two suites into GC thrash and an
  828-second timeout that looked like a deadlock ([README.md, "A lesson that cost a day"][os-shell]).
- **A skipped test is a stale test.** Two OpenSearch cost assertions failed for weeks unnoticed,
  because the suite skips without an S3 endpoint. hsearch sets `HSEARCH_S3_REQUIRED=1` so that a
  missing store fails instead of skipping ([CURRENT.md][hs-current]).
- **Rank cost findings by what they scale with**, not by "looks redundant". Two independent
  reviews of OpenSearch's cost audit overturned its ranking this way. pstore's Never list states
  the same rule.
- **Hibernation is a worked example of the project's setting rule.** Sleeping and waking a
  writer costs 5 + 8 requests, against about 17 to give the head back and take it again, but
  only for gaps under the idle release time. So hibernation shipped off by default, with the
  trade-off written down in numbers ([README.md, "Hibernating idle writers"][os-shell]).
- **Packing a refresh's files into one tar** cut remote-store PUTs by 80 to 90% in
  `features/rbs-cost-saving` ([rbs-cost-saving.md][os-rbs]). pstore already writes one bundle
  per flush and one object per segment, so this confirms the design rather than adding to it.

## Sources

[hs-lanes]: https://github.com/huyz0/hsearch/blob/main/docs/plan/evidence/m03-08-lanes.md
[hs-model]: https://github.com/huyz0/hsearch/blob/main/docs/plan/evidence/m03-09-lane-sharing-model.md
[hs-conf]: https://github.com/huyz0/hsearch/blob/main/docs/plan/evidence/m03-06-conformance.md
[hs-fair]: https://github.com/huyz0/hsearch/blob/main/docs/plan/evidence/m13-02-fairness.md
[hs-retry]: https://github.com/huyz0/hsearch/blob/main/docs/plan/evidence/m03-12-retry-attribution.md
[hs-fence]: https://github.com/huyz0/hsearch/blob/main/docs/plan/evidence/m03-11-fencing.md
[hs-cost]: https://github.com/huyz0/hsearch/blob/main/docs/plan/evidence/m02-07-cost-calibration.md
[hs-kms]: https://github.com/huyz0/hsearch/blob/main/docs/plan/evidence/m05-08-kms-path-accounting.md
[hs-current]: https://github.com/huyz0/hsearch/blob/main/docs/plan/CURRENT.md
[os-tput]: https://github.com/huyz0/OpenSearch/blob/feature/serverlessshell/serverless/write-throughput-results.md
[os-budgets]: https://github.com/huyz0/OpenSearch/blob/feature/serverlessshell/serverless/request-budgets.md
[os-shell]: https://github.com/huyz0/OpenSearch/blob/feature/serverlessshell/serverless/README.md
[os-status]: https://github.com/huyz0/OpenSearch/blob/feature/serverlessshell/serverless/STATUS.md
[os-ll]: https://github.com/huyz0/OpenSearch/blob/low-latency-serverless/serverless/low-latency-serverless-plan.md
[os-plugin]: https://github.com/huyz0/OpenSearch/blob/feature/serverlessplusplus/plugins/serverless-storage/README.md
[os-rbs]: https://github.com/huyz0/OpenSearch/blob/features/rbs-cost-saving/plan/rbs-cost-saving.md
[lr-ivf]: https://github.com/huyz0/lucene-rust/blob/main/docs/benchmarks/ivf.md
[lr-dhnsw]: https://github.com/huyz0/lucene-rust/blob/main/docs/benchmarks/disk-hnsw.md

Also read, and not cited above: the shell's
[fencing RFC](https://github.com/huyz0/OpenSearch/blob/feature/serverlessshell/serverless/rfc-fencing-closure.md),
which closes a takeover window by putting a fence blob in the predecessor's next log slot. It
reaches the same design as pstore's lanes, which are put-if-absent per writer and use no clock.
Also the [early-acknowledgement RFC](https://github.com/huyz0/OpenSearch/blob/feature/serverlessshell/serverless/rfc-wal-first-takeover.md),
deferred because the engine, not the log, assigns sequence numbers, a problem pstore does not
have. And lucene-rust's
[full-text AVX-512 sweep](https://github.com/huyz0/lucene-rust/blob/main/docs/benchmarks/fts-avx512.md):
only hit compaction (2.2 to 6.7 times faster) and bitset expansion (1.25 to 1.75 times) gained,
because the rest of the profile is branchy, latency-bound work.
