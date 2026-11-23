# Our own object store layer, or `object_store`? (M286, 2026-11-18)

**Recommendation: build our own S3 client, and later our own Azure one, and drop
`object_store` from the server.** The S3 half exists now behind the `s3-native` feature
(`pstore_blob::s3::S3Backend`). It is tested but not wired into the server yet. On all seven
patterns pstore uses, it makes 59% to 87% fewer allocations a request and allocates 13% to
85% fewer bytes. Its CPU is lower or within noise. It removes the two flagged crates and 74 others from `pstore-blob`'s tree.
It also takes out a retry layer that the congestion controller cannot see.

PR 72's advisory ignore stays as it is: that thread owns the decision. The ignore can be
deleted when the server no longer links `object_store`, which needs M287 and M288 below.

## Why the question came up

`cargo deny` now flags `chrono` (RUSTSEC-2026-0543) and `humantime` (RUSTSEC-2026-0542) as
unmaintained. Both are flagged as unmaintained, with no vulnerability. Only `object_store`
pulls them in, and it does so **without a feature gate**. Every version up to 0.14.2 lists
both as required dependencies, so no feature set removes them. The only ways out are to wait
for upstream or to stop using `object_store`.

pstore already does what OpenSearch's repository plugins do. Everything above `pstore-blob`
uses our own `BlobStore` trait (D-1, D-2), and the accounting, congestion control,
coalescing, caching and capability probes are ours. `object_store` sits only under that
trait, as a transport. So the question is narrower than "write an abstraction": **which
transport goes under the abstraction we already have.**

## The candidates

| Option | Removes chrono/humantime | What it brings | Verdict |
|---|---|---|---|
| Keep `object_store` 0.14 + ignore | no | nothing new | the fallback if the numbers had lost |
| OpenDAL 0.59 | yes (uses `jiff`) | `opendal-core` has 20 direct deps; S3 signing comes from `reqsign`, and its Azure signer pulls `rsa` 0.9, which has **RUSTSEC-2023-0071** (Marvin timing attack, no patched 0.9) | trades an unmaintained crate for a vulnerable one on Azure. Its own operator API sits under our trait as a second abstraction. Not measured |
| `aws-sdk-s3` | yes | 28 direct deps, the smithy runtime, slow to compile, S3 only (Azure still needs something else) | heaviest. Gives us an S3 client and nothing for Azure |
| `rusty-s3` / `reqsign` + our own HTTP | yes | signing and URL building only. `reqsign`'s Azure signer has the same `rsa` issue | only saves the SigV4 code (about 150 lines, now written and checked against AWS's examples) |
| **Own client on `hyper`** | **yes** | **no new crates**: `hyper`, `hyper-util`, `hyper-rustls`, `ring`, `md-5`, `base64` were already in the lockfile | **chosen**. See the numbers |

## What our own client does differently

- **One retry layer.** `object_store` retries any request up to 10 times inside the client,
  a conditional PUT included. As a result, `Congested` never sees a `503` until the client
  has given up. M282 needed a second `object_store` client with retries turned off so that a
  conditional write is never repeated. `S3Backend` has no retries at all, so `Congested`
  sees every throttle once (`a_503_is_slowdown_after_one_request`).
- **Typed errors from the status code.** `object_store` folds S3's `409
  ConditionalRequestConflict` and `503 SlowDown` into a generic error. `ObjectStoreBackend`
  recovers them by matching on the message text (`map_cas_err`, `map_err`). `S3Backend`
  reads the status.
- **Fewer allocations.** A one-frame body is passed on with no copy, and there is no `Path`
  type or `GetResult` stream layer. See the numbers.
- **Batch deletes are ours.** `DeleteObjects` is built in quiet mode, so a 1,000-key
  delete's answer is empty rather than 1,000 `<Deleted>` entries to parse. The same approach
  covers GCS's batch endpoint, which `gcs_batch.rs` already hand-builds around
  `object_store`.

## The numbers

`cargo run --release -p pstore-blob --features object_store,s3-native --example s3_clients`,
with `PSTORE_BENCH_SCALE=10`: a 4-core cloud container, and the fake S3 in a child
process on loopback, so the CPU and allocations are the client's alone. Each row is the mean
of two runs, one in each order. ⚠️ **`provisional`**: these are relative numbers on a shared
container, and loopback has no network latency. A request's own client cost is what is
measured. It is the only cost that differs between the two clients.

| pattern | requests/s, object_store → native | client CPU µs/request | allocations/request | bytes allocated/request |
|---|---|---|---|---|
| WAL bundle, conditional create, 16 KiB | 18,600 → 20,000 | 20.9 → 21.0 (+1%) | 136.6 → 54.8 (−60%) | 38,338 → 16,852 (**−56%**) |
| ranged GET, 64 KiB | 28,300 → 37,100 | 25.3 → 22.9 (−10%) | 145.1 → 60.1 (−59%) | 162,488 → 140,918 (−13%) |
| suffix GET, 64 KiB (segment footer) | 29,000 → 39,900 | 24.8 → 20.4 (−18%) | 144.0 → 59.0 (−59%) | 162,216 → 140,534 (−13%) |
| 256 concurrent 4 KiB GETs (one query round) | 26,600 → 49,700 | 20.0 → 11.9 (**−41%**) | 145.1 → 60.1 (−59%) | 38,990 → 17,512 (**−55%**) |
| segment PUT, 32 MiB | 18 → 18 | 11,063 → 11,688 (+6%) | 131.1 → 53.2 (−59%) | 36,846 → 16,692 (**−55%**) |
| HEAD (`stat`) | 32,700 → 54,100 | 17.0 → 11.8 (**−30%**) | 131.0 → 49.0 (−63%) | 36,408 → 16,553 (**−55%**) |
| delete 1,000 keys | 900 → 2,350 | 77.5 → 62.5 (−19%) | 11,649 → 1,526 (−87%) | 735,809 → 113,742 (**−85%**) |

⚠️ **The CPU column is noisy; the allocation columns are not.** CPU comes from
`/proc/self/stat` in 10 ms ticks, on a container shared with the fake. The same pattern
moved by up to 15% between runs: an earlier run, before the request timeout was added,
had the WAL put at −13% and the segment PUT at −4%. The allocation counts are the same
from run to run to within a few bytes. The request timeout added 390 bytes and no
allocations per request.

The SPEC's decision rule (criterion 4) was: at least 25% less CPU **or** 25% fewer bytes
allocated on at least four of the seven patterns, and no more than 10% worse on any. The
own client allocates at least 25% fewer bytes on five patterns (WAL put, fan-out, segment
put, HEAD, delete) and uses at least 25% less CPU on two (fan-out, HEAD). The worst CPU
result is the segment PUT at +6%, which is hashing 32 MiB in both clients and within this
column's noise. **The rule holds.**

What these numbers mean in production: at a real S3's 10 to 30 ms a round trip, 10 µs of
CPU is under 0.1% of a request's latency. The benefit is CPU and allocator load per node,
not latency. At 10,000 requests/s a node saves about 0.05 to 0.1 of a core and about 200
MB/s of allocations. The read-throughput gains above (1.3× to 2×) come from loopback, where
the client and the fake share four cores. They will not show up on a network.

The SHA-256 of the payload dominates the segment PUT for both clients, about 11 ms for 32
MiB, because both sign it. `UNSIGNED-PAYLOAD` over TLS would remove that cost. That works
with both clients, so it is a separate setting and plays no part in this comparison.

**Build cost and dependencies.** `pstore-blob` with `object_store` + `aws` has 132 crates
in its normal dependency tree. With `s3-native` it has 58. A clean debug build of
`pstore-blob` took 33 s with the first and 22 s with the second, on 4 cores. `chrono`,
`humantime`, `reqwest` and `quinn` (whose own advisories turned `cargo deny` red on
2026-11-14) are all absent from the second.

## Downsides, measured or named

1. **We own the S3 quirks.** These include redirects for a wrong region, `x-amz-checksum`
   rules on new buckets, path-style deprecation, and clock skew (`RequestTimeTooSkewed`).
   `object_store` absorbs some of these today. The conformance suite against `rustfs` and
   Azurite (`scripts/conformance.sh`) is what would show a gap. It has not run against the
   new client: there is no Docker in this container.
2. **No credential chain yet.** `S3Backend` takes static or `AWS_*` environment
   credentials. On EC2 or EKS the server relies on `object_store`'s chain (instance profile
   IMDSv2, ECS container credentials, web identity/IRSA). This is roughly 300 lines plus
   tests, and M287 must have it before the native client can be the default.
3. **Azure stays on `object_store` until M288.** The server enables both `aws` and `azure`,
   so `chrono` and `humantime` remain in the server build until Azure has a native client
   too. That client needs SharedKey signing (one HMAC, like SigV4) and Entra tokens (managed
   identity and workload identity, via an HTTP token endpoint). The `rsa` crate is not
   needed unless certificate credentials are wanted, and they are not.
4. **The fake is not S3.** Every client test runs against `pstore_testkit::fake_s3`, which
   verifies each signature with pstore's signer. `object_store`'s requests pass that check,
   so two independent implementations agree, but neither has signed a request to AWS in
   this milestone. That is the M0b gap every backend here already has.
5. **More code to keep**: about 720 lines of client, signer and XML outside their tests, and
   440 of fake (test-only, in `pstore-testkit`, `ships = false`).

6. **`parquet` also brings `chrono`**, as a dev-dependency only: M235's Parquet export
   tests in `pstore-format` and `pstore-server` read their output back with it.
   `cargo deny` checks dev-dependencies too, so the ignore stays until those tests
   check their output another way, or `parquet` drops `chrono`. No shipped binary links
   it.

## Plan

- **M287**: credential chain (IMDSv2, ECS, web identity), `UNSIGNED-PAYLOAD` over TLS as
  an option, and a cluster setting `PSTORE_S3_CLIENT=object_store|native` in the server.
  The default stays `object_store` until the emulator conformance run passes with
  `native`, then becomes `native`. A per-index override makes no sense here: one server has
  one store. Both modes stay tested.
- **M288**: native Azure (SharedKey + Entra), then `object_store` leaves `pstore-server`.
  Only GCS's probe-only `gcs` feature still uses it, and it can follow. When the server no
  longer links it, the humantime ignore matches nothing and `cargo deny` says so. The
  chrono ignore also needs `parquet` (a dev-dependency, downside 6) dealt with.
