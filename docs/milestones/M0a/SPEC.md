# M0a — Local substrate

**Serves:** D-1 (`BlobStore` trait), D-2 (nothing above it touches `object_store`), D-3
(fault-injecting store is the primary correctness vehicle), D-99 (three test layers),
D-101 (where we cannot measure, parameterize). Answers the local half of OQ-5, OQ-22.

**Exit condition** (roadmap): *we know what would break and at what threshold, plus a
recorded capability profile per backend.*

## Delta

Against the corpus, this milestone adds the bottom layer and nothing else.

**Adds**
- `pstore-blob` — the `BlobStore` trait, an in-memory backend, a fault-injecting
  decorator, per-tenant request accounting, range coalescing, adaptive concurrency.
- `pstore-testkit` — the conformance suite that *populates* `Capabilities` (D-100), and
  the sensitivity sweeps that find breaking points rather than measuring absolutes.

**Does not add** — deliberately, and each is a later milestone:
- Real S3/GCS/Azure backends wired to `object_store`. The trait is shaped for them and
  one adapter is written, but **integration is unverified without the compose stack**.
- `pstore-fake-s3` on `s3s`. The in-process store carries correctness; the fake carries
  the client/wire path, which no code exercises yet.
- Manifests, WAL, segments, indexes. Nothing above layer 1.

## Acceptance criteria

1. A `BlobStore` implementation supports the portable primitive set — ranged GET, atomic
   PUT, create-if-absent, compare-and-swap, batch delete — and CAS failure distinguishes
   `Lost` (412) from `Contended` (409) as distinct types.
2. Every blob operation increments a per-class, per-tenant counter, so `RA` is assertable
   from a test rather than reasoned about.
3. `get_ranges` coalesces ranges whose gap is below a configurable threshold, issuing
   strictly fewer requests than ranges, and returns byte-identical results to fetching
   each range separately.
4. The fault-injecting store reproduces an identical failure sequence from the same seed,
   and can inject 412, 409, 503 and latency independently.
5. Adaptive concurrency reduces in-flight requests on `503 SlowDown` and recovers, with
   the retry count bounded.
6. The conformance suite produces a `Capabilities` profile from observed behaviour, and a
   backend that fails the CAS probe is recorded as divergent rather than assumed capable.
7. A sensitivity sweep over CAS contention reports, for a range of contention levels, the
   observed commit success rate — a curve, not a point.
8. `pstore-blob` and `pstore-testkit` are ≥95% region-covered; no crate but
   `pstore-kernel` relaxes the unsafe lint; the whole gate set is green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `cas_lost_and_contended_are_distinct` | mapping 409 to `Lost`, which would cause a needless rebase storm |
| 1 | `create_if_absent_rejects_second_write` | ignoring the precondition, so both writes succeed |
| 1 | `cas_rejects_stale_tag` | accepting any tag, which is the fencing property |
| 2 | `accounting_counts_one_write_per_put` | counting bytes instead of requests, or not counting at all |
| 2 | `accounting_is_per_tenant` | summing all tenants into one bucket |
| 3 | `coalescing_merges_adjacent_ranges` | never merging (no saving) |
| 3 | `coalescing_respects_the_gap_threshold` | merging unconditionally, causing unbounded read amplification |
| 3 | `coalesced_reads_equal_separate_reads` | returning the merged buffer rather than each range's slice |
| 4 | `same_seed_reproduces_the_same_failures` | using real randomness, making failures unreproducible |
| 4 | `injected_status_is_returned` | swallowing the injected fault |
| 5 | `slowdown_reduces_concurrency_then_recovers` | never reducing, or never recovering |
| 5 | `retries_are_bounded` | retrying forever on a persistent 503 |
| 6 | `conformance_records_divergent_cas` | recording `Supported` for a backend that failed the probe |
| 7 | `sweep_reports_a_curve_not_a_point` | reporting a single number, losing the breaking point |

## RA budget

This is the layer that *defines* RA, so the budget is the thing under test rather than a
consequence: `put` = 1 W. `get_ranges` over *n* ranges = *m* Rpar where *m* ≤ *n*.
`cas` = 1 W. **`list_unrestricted` is the only listing entry point** and no other method
may call it. Sequential depth: unchanged, nothing above exists yet.

## Risks

- **The trait shape is wrong for real backends.** Revealed by writing the `object_store`
  adapter in this milestone rather than later, even unverified.
- **The fault-injecting store models a blob store we imagine.** Mitigated only partly:
  its semantics come from `02-object-storage/api-semantics.md`, and the conformance suite
  is the contract both it and a real backend must satisfy. Fully closed only in M0b.
- **Sensitivity sweeps measure our simulation, not S3.** Stated in every output. The
  curve's *shape* is the deliverable; the position of the real operating point is M0b.

## Tasks

| ID | Task |
|---|---|
| M0a.1 | `pstore-blob` skeleton: `Key`, `Capabilities`, `Precondition`, `CasError`, `BlobStore` trait |
| M0a.2 | In-memory backend with full CAS semantics |
| M0a.3 | Per-tenant, per-class request accounting |
| M0a.4 | Range coalescing with a configurable gap threshold |
| M0a.5 | Fault-injecting decorator: seeded, deterministic |
| M0a.6 | Adaptive concurrency and bounded retry on 503 |
| M0a.7 | `object_store` adapter (integration unverified — no emulator running) |
| M0a.8 | `pstore-testkit`: conformance suite producing `Capabilities` |
| M0a.9 | `pstore-testkit`: CAS contention sensitivity sweep |
