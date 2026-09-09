# Dev and Test Environment: WSL2, Containers, and Emulator-Only Clouds

**Answers:** Q44
**Status:** Complete (v1)
**Retrieved:** 2026-09-05

## 1. The constraints

| Constraint | Consequence |
|---|---|
| Development on **WSL2** | Local builds and tests must be containerized and resource-capped, or they take the host down |
| **CI is already containerized** | Do not nest containers there; same commands, different execution context |
| **No real cloud accounts yet** | All blob-store testing is against emulators / S3-compatible servers, with real-cloud validation as a later milestone |

The third one turns out to be the interesting one, and not for the reason you'd expect.

## 2. The finding: emulators diverge on exactly the primitive we depend on

`pstore` rests on one primitive — compare-and-swap on a blob
([`../03-metadata-consistency/manifest-and-cas.md`](../03-metadata-consistency/manifest-and-cas.md)).
**Every self-hostable implementation has documented divergences on it.**

| Backend | CAS status |
|---|---|
| **MinIO** | Supports `If-Match` and `If-None-Match` (since Feb 2023, earlier than AWS) **but does not support the `*` wildcard** — it always requires an exact ETag. `If-None-Match: *` does **not** prevent a second write, so you get two successes where S3 gives one success and a 412. Tracked as minio#20346, *"MinIO is not compatible with new S3 conditional write feature."* |
| **LocalStack** | `If-Match` implemented via PR #11941 after AWS shipped CAS; historically **crashed when returning 412** (#3168). |
| **Azurite** | Did **not** return 412 for `If-Match: *` on a non-existent blob — inconsistent with real Azure and with the legacy emulator. Fixed in a recent release, so version pinning matters. |
| **SeaweedFS** | Conditional write implemented, but **broken when versioning and locking are enabled** (#8073). |
| **RustFS** | ETag quoting mismatches — lenient in responses, strict in requests (#1458). |
| **Garage** | Core operations only; no versioning or object locking. |
| **fake-gcs-server** | Precondition fidelity undocumented — must be measured, not assumed. |

`object_store` reflects this reality: `S3ConditionalPut` offers only `ETagMatch` (standard
`If-Match`/`If-None-Match`) and `Disabled`. There is no "works everywhere" mode.

> **Finding T-1.** No emulator can validate our core primitive. `If-None-Match: *` — which
> underpins index creation, write-once lane objects, and advisory claims — is the *most* poorly
> supported operation across S3-compatible servers, and it is the one we lean on hardest.

> **C-13 — MinIO's wildcard works now, and T-1's premise is half wrong. M7a. Measured.**
> The table above says MinIO "does not support the `*` wildcard" and that `If-None-Match: *`
> "does not prevent a second write". Probed against
> `minio/minio:RELEASE.2025-04-22T22-12-26Z` — the version this repo pins — **both
> `create_if_absent` and `compare_and_swap` are `Supported`**: the second create is refused,
> as S3 does it. minio#20346 has evidently been fixed since the table was written.
>
> ⚠️ **T-1's conclusion survives its premise, for a different reason.** MinIO fails
> `aba_resistance`: its ETags are content-derived, so writing `v1 → v2 → v1` returns a tag
> equal to the first and a paused writer's CAS lands against a world that changed and changed
> back. Our HEAD carries a nonce precisely so that this cannot bite
> ([`head.rs`](../../../crates/pstore-engine/src/head.rs)) — which means MinIO is usable
> *because of a design decision taken before it was measured*, not because it is faithful.
>
> **What this changes:** "use `pstore-fake-s3`, treat MinIO as a third-party compatibility
> check" is still right, and the reason is now ABA rather than the wildcard. Evidence:
> [`docs/profiles/capability-matrix.md`](../../profiles/capability-matrix.md),
> [`M7a/VERIFIED.md`](../../milestones/M7a/VERIFIED.md).

This is not a reason to stop; it is a reason to change what each test layer is *for*.

## 3. Three test layers, three different jobs

> **D-99.** Correctness is proven against our own in-process store; emulators test plumbing;
> real clouds test economics. Conflating these is how a system passes every test and fails in
> production.

### Layer 1 — In-process fault-injecting store (**the primary correctness vehicle**)
The `BlobStore` implementation from [`../02-object-storage/rust-object-store-crates.md`](../02-object-storage/rust-object-store-crates.md)
D-3, now promoted from "useful" to **load-bearing**. It is the only backend where we control
and can assert exact semantics: deterministic seeds, injected latency distributions, 412/409/503
storms, delayed visibility, partial failures.

This is where **Invariant I1**, the commit protocol, and bundle recovery
([OQ-91](../00-plan/open-questions.md)) are actually proven. Runs in-process, no containers, no
network, fast enough for every `cargo test`.

### Layer 1b — Our own S3 fake (**decided: build it**)
Since no emulator implements the primitive, we build `pstore-fake-s3` on the `s3s` crate —
~12 operations with exactly AWS's documented semantics, plus protocol-level fault injection.
It tests a layer the in-process store cannot: `object_store` → reqwest → wire → XML → error
mapping. Crucially it **unblocks OQ-6 now** (ABA under multipart ETags) and makes the 412/409
distinction testable, which it is nowhere else. Design, scope, and the anti-circularity rules:
[`blob-store-fakes.md`](blob-store-fakes.md).

### Layer 2 — Emulator integration (plumbing, not semantics)
MinIO, Azurite, fake-gcs-server in containers. What they genuinely validate:
- HTTP client wiring, auth, retries, timeouts
- Request/response encoding, multipart, ranged GETs, batch delete
- Error parsing and mapping
- End-to-end flows

What they **cannot** validate: CAS semantics under contention, real latency, throttling
behaviour, or cost.

### Layer 3 — Conformance suite (the bridge)
A single suite, run against **every** backend including the real clouds later, that probes each
capability and **records** the answer:

```
conformance::cas_create_if_absent_wildcard   -> Supported | Unsupported | Divergent(note)
conformance::cas_match_exact_etag            -> ...
conformance::etag_stable_across_multipart    -> ...
conformance::412_vs_409_distinction          -> ...
conformance::delete_is_free / batch_delete_max
conformance::range_read_semantics / suffix_range
conformance::read_after_overwrite_consistency
```

> **D-100.** The `Capabilities` struct ([`../02-object-storage/api-semantics.md`](../02-object-storage/api-semantics.md) §6)
> is **populated by the conformance suite, not hand-written**. Each backend gets a recorded,
> dated capability profile checked into the repo. Divergence becomes data instead of a
> production surprise, and adding a real cloud later is one suite run rather than an audit.

This turns the constraint into an asset: we are forced to build the capability matrix we would
have needed anyway for multi-cloud and BYOC.

### The wildcard workaround
Where `If-None-Match: *` is unsupported, create-if-absent can be emulated as *"CAS from a
known-absent state"* — but the emulation is not atomic in the same way, so the conformance
profile must record which mode is in play and the engine must refuse to run in
`durable` mode on a backend whose profile says CAS is `Divergent`. **Fail loudly at startup
rather than silently corrupt.**

For *development*, the answer is simpler: use `pstore-fake-s3`, which implements the wildcard
correctly, and treat MinIO as a third-party compatibility check rather than the default target.

## 4. What "no cloud accounts" actually blocks

| Open question | Blocked? |
|---|---|
| **OQ-5** CAS throughput / loss curves under contention | **Blocked** — and it is the #2 Tier-1 risk |
| **OQ-3** real p50/p99/p999 TTFB | **Blocked** |
| **OQ-2** range-coalescing break-even `G*` | **Blocked** (emulator latency is unrepresentative) |
| OQ-1 409 retry cost under a herd | Blocked |
| OQ-24 S3 Express One Zone semantics/cost | Blocked |
| OQ-98 instance-store NVMe endurance | Blocked (needs real instance store) |
| **OQ-6** ABA hazard under multipart ETags | **Unblocked by the fake** — synthesize the `-N` and non-MD5 ETag forms ([`blob-store-fakes.md`](blob-store-fakes.md) §5) |
| OQ-51 LIRE quality under batched rewrites | **Not blocked** — algorithmic |
| OQ-91 bundle recovery completeness | **Not blocked** — simulator |
| OQ-111 bytes-scanned/s | **Partly** — measurable locally, but see §5 |
| OQ-104 per-query memory profile | Not blocked |
| OQ-144 scan roofline | Partly |

Most of the *architecture* risk is unblocked. What's blocked clusters entirely around
**blob-store performance economics**.

> **D-101. Where we cannot measure, parameterize.** Instead of "what is the CAS rate?", ask
> "at what CAS rate does the design break?" Sweep the fault-injecting store across 0.5–50
> CAS/s and find the breaking point. Then a single real-cloud measurement later tells us which
> regime we are in, rather than starting the analysis from scratch.

Sensitivity analysis is arguably *more* valuable than a point measurement, because it tells us
how much headroom the design has. Do it deliberately, not as a consolation prize.

## 5. WSL2: keeping the host alive

Three distinct resource layers, each needing its own cap.

### (a) The WSL2 VM itself — `%UserProfile%\.wslconfig`
By default **WSL2 can take up to 50% of host RAM**, and `vmmem` is simply the Windows-side view
of that VM. Linux page cache inside it is not waste, but *reclamation across the VM boundary* is
the weak point.

```ini
# %UserProfile%\.wslconfig   — requires `wsl --shutdown` to take effect
[wsl2]
memory=12GB            # hard ceiling; leave the host ≥8GB
processors=8           # leave cores for Windows
swap=8GB
autoMemoryReclaim=gradual
```

### (b) Container limits — belt and braces
```yaml
deploy:
  resources:
    limits: { cpus: '6', memory: 8G }
```
So a runaway test hits a container OOM (one container dies, diagnosable) rather than a VM OOM
(everything dies, mysteriously). This mirrors D-58's logic exactly: **a soft, attributable limit
inside a hard one.**

### (c) The build itself — the real memory hog
Rust compilation, and linking in particular, is where builds OOM:
- **Linker.** `lld` uses less memory and links faster than the default; **mold** performs
  similarly or better *while using less memory*. `lld` has been the default on Linux since Rust
  1.90 — verify it is actually in use.
- **`-j` and `codegen-units`.** More codegen units means more LLVM parallelism and more peak
  memory. Cap `jobs` in `.cargo/config.toml` rather than letting cargo see all cores.
- **Debug info** is a large contributor; `debug = 1` or `split-debuginfo` for dev profiles.
- **`sccache`** (`RUSTC_WRAPPER`) so rebuilds don't re-link the world.

> **D-102.** WSL2 filesystem rule: **the repository and `target/` live in the Linux filesystem
> (`~/`), never under `/mnt/c`.** Cross-filesystem IO through 9p is an order of magnitude
> slower and it silently makes every build and test feel broken. Use a **named Docker volume**
> for `target/`, not a bind mount.

## 6. CI: the same commands, one flag

> **D-103.** One entrypoint (`just` / `make`) with a `PSTORE_CONTAINERIZED` switch. Locally it
> wraps commands in `docker compose run`; in CI, which is already containerized, it executes
> them directly. **The commands themselves are identical**, so "works locally, fails in CI" has
> one fewer cause.

Service dependencies (MinIO, Azurite, fake-gcs-server) are `docker compose` services in both
places — in CI as service containers, locally as siblings. `testcontainers` is the alternative
if we want per-test isolation; `testcontainers-modules` has a MinIO module for Rust. Start with
compose (simpler, faster, one shared instance) and adopt testcontainers only where test
isolation demands it.

For image builds: **`cargo-chef` + `sccache`**, which give large speedups but *"only if the
cache is persisted between builds"* — sccache matters most in CI, where Docker layer caches are
often unavailable.

## 7. Benchmarking on WSL2: treat results as relative

A VM under Windows with dynamic memory, no CPU pinning, and a shared host is not a benchmarking
environment. Frequency scaling, page-cache behaviour, and `vmmem` reclamation all add noise.

> **D-104.** WSL2 benchmark numbers are **relative** (did this change make it faster?), never
> **absolute** (what is our QPS/node?). Any figure destined for the cost model — OQ-111,
> OQ-144, OQ-75 — is marked *provisional* until re-measured on real hardware. Record the
> environment alongside every number.

This applies especially to the scan roofline: measuring *cycles per vector* is reasonably robust
under virtualization, while measuring *achieved memory bandwidth* is not.

## 8. Roadmap change

M0 was "Substrate truth — replace every guessed number with a measured one," and assumed
microbenchmarks inside AWS/GCP/Azure. Split it:

### M0a — Local substrate (unblocked, do now)
- `BlobStore` trait + fault-injecting implementation
- Conformance suite; capability profiles recorded for MinIO / Azurite / fake-gcs-server
- **Sensitivity sweeps** (D-101): CAS rate, latency distribution, error rate → find breaking
  points
- Containerized dev environment, CI parity, resource caps

**Exit:** we know *what would break and at what threshold*, and have a capability matrix.

### M0b — Real-cloud validation (deferred; unblocks when accounts exist)
A single, well-defined milestone rather than a vague intention:
- Run the **same** conformance suite against real S3, GCS, Azure → complete the matrix
- OQ-5 CAS contention curves; OQ-3 TTFB percentiles; OQ-2 `G*`; OQ-1 409 behaviour
- OQ-24 Express One Zone; OQ-98 NVMe endurance (needs real instance store)
- Re-measure every provisional number from D-104

**Exit:** every "provisional" tag removed from the cost model.

> Because M0a produces sensitivity curves rather than assumptions, M0b becomes *"take five
> measurements and read off which regime we are in"* — a few days, not a re-analysis.

## 9. Open questions raised

- **OQ-150 (Tier 2)** — Which backend has the **highest CAS fidelity** for day-to-day
  development? MinIO's missing wildcard is a real obstacle; LocalStack may be closer to AWS
  semantics now. Run the conformance suite against all of them and pick on evidence.
- OQ-151 — Should we contribute a wildcard `If-None-Match` fix upstream to MinIO? It would
  benefit the whole ZDA ecosystem and is probably a small patch.
- OQ-152 — Is a **shared remote dev container** (a cloud VM) worth it later, to escape WSL2's
  benchmarking limitations without needing cloud *storage* accounts?
- OQ-153 — Does `fake-gcs-server` implement `ifGenerationMatch` correctly? GCS generations are
  our cleanest CAS story on paper; if the emulator is faithful, GCS may be the best *primary*
  development target rather than S3.

## Sources

- [Leading the Way: MinIO's Conditional Write Feature — MinIO blog](https://blog.min.io/leading-the-way-minios-conditional-write-feature-for-modern-data-workloads/)
- [MinIO is not compatible with new S3 conditional write feature — minio/minio #20346](https://github.com/minio/minio/issues/20346)
- [Plans to support conditional writes? — minio/minio discussion #20318](https://github.com/minio/minio/discussions/20318)
- [S3ConditionalPut — object_store docs.rs](https://docs.rs/object_store/latest/object_store/aws/enum.S3ConditionalPut.html)
- [implement S3 conditional write IfMatch — localstack/localstack #11941](https://github.com/localstack/localstack/pull/11941)
- [S3: Crash when returning 412 errors — localstack/localstack #3168](https://github.com/localstack/localstack/issues/3168)
- [Azurite does not return 412 for IfMatch="*" on non-existent blob — Azure/Azurite #2589](https://github.com/azure/azurite/issues/2589)
- [S3 API: conditional write broken with versioning and locking — seaweedfs #8073](https://github.com/seaweedfs/seaweedfs/issues/8073)
- [Conditional requests fail with unquoted ETags — rustfs #1458](https://github.com/rustfs/rustfs/issues/1458)
- [Exploring S3 Mocking Tools: S3Mock, MinIO, and LocalStack — LocalStack blog](https://blog.localstack.cloud/2024-04-08-exploring-s3-mocking-tools-a-comparative-analysis-of-s3mock-minio-and-localstack/)
- [MinIO module — testcontainers-modules for Rust](https://docs.rs/testcontainers-modules/latest/testcontainers_modules/minio/struct.MinIO.html)
- [WSL2 Memory/CPU Limits: Stop It from Eating Your RAM — cr0x.net](https://cr0x.net/en/wsl2-memory-cpu-limits/)
- [Docker on Windows/WSL2 is slow: fixes that actually help — cr0x.net](https://cr0x.net/en/docker-wsl2-performance-fixes/)
- [Build Configuration — The Rust Performance Book](https://nnethercote.github.io/perf-book/build-configuration.html)
- [Optimal Dockerfile for Rust with cargo-chef and sccache — Depot](https://depot.dev/docs/container-builds/optimal-dockerfiles/rust-dockerfile)
- [LukeMathWalker/cargo-chef — GitHub](https://github.com/LukeMathWalker/cargo-chef)
