# M127 measurements

These provisional Docker/WSL2 results qualify the security-patched tree:
quinn-proto 0.11.18, Tokio 1.53.3, and maintained rustls-pki-types PEM APIs.
Source hashes identify the dirty tree based on 20bea70; that commit alone does
not identify the measured source. Split-search results are recorded below.

Intel i5-13600KF, four CPU quota, 8 GiB container ceiling, Rust 1.90.0 /
LLVM 20.1.8, release profile. Metadata records compiler, dependency lock,
source/config hashes and load. No timing threshold is added to CI.
Subsequent qualification adds tests, relocates test-only benchmark/wire helpers,
and tightens remote QUIC-close classification so TLS alerts cannot permit an H2
retry. The peer transport campaign below was rerun after that correction.
Authentication and split-search samples retain their earlier source hashes;
compiler and dependency versions are unchanged.
The later PEM, scope and heading coverage controls change only test sources;
shipping source files still match the recorded transport campaign hashes.

## JWT admission

Five alternating-order samples of 5,000 requests per case, each preceded by
1,000 warmups. All successful complete responses/digests match; signature
refusals match the expected complete error. Every case asserts zero blob I/O.

| Case | Median sample mean/request | Sample mean min-max | Allocations/request | Allocated bytes/request |
|---|---:|---:|---:|---:|
| Explicit private mode | 3.470 us | 3.445-3.501 us | 48 | 7,697 |
| Valid JWT | 6.036 us | 5.989-6.042 us | 66 | 9,273 |
| Invalid signature | 3.605 us | 3.543-3.661 us | 52 | 6,622 |

The paired valid-minus-private median is 2.541 us, with 18 additional
allocations and 1,576 additional allocated bytes. This includes routing,
collection, assertions, digest work and allocator instrumentation; it is not an
isolated cryptographic cycle count. Allocated bytes are cumulative allocation
traffic, not live memory.

Raw [samples](measurements/auth-after-security.csv),
[summary](measurements/auth-after-security-summary.json) and
[source/toolchain/resource metadata](measurements/auth-after-security-metadata.json)
preserve the experiment. The executable alone used 0.4094 user CPU seconds over
0.4252 wall seconds and peaked at 12,404 KiB RSS. Compilation is excluded.
Totals include setup, warmups and all cases; they are not a per-request memory bound.

Reproduce alone, without another test using its global allocator:

    cargo test --release -p pstore-server --test auth_bench -- --ignored --nocapture

The earlier pre-advisory campaign is retained as [samples](measurements/auth-admission.csv),
[summary](measurements/auth-admission-summary.json) and
[metadata](measurements/auth-admission-metadata.json). It measured a paired
2.634 us admission difference. The two campaigns use distinct dependency trees;
no causal speedup is attributed to the security patch.

## Peer transport

Two Tokio workers in the same capped container, loopback, five samples with
reversed protocol order. Each sample uses ten cold pools, 256 warm serial calls,
256 calls at concurrency 32, and 32 large calls. The typed echo contains 768 u32
query bits; representative id=1 frames are 3,887 bytes and 134,963 bytes with the
large string. Real IDs vary by a few encoded bytes. Negotiated-protocol counters
and matching bit digests pass on both transports.

| Case | HTTP/2 | HTTP/3 |
|---|---:|---:|
| Cold RPC, median sample mean | 0.643 ms | 2.401 ms |
| Cold RPC, sample mean range | 0.613-0.702 ms | 2.119-3.299 ms |
| Warm serial, median sample mean | 120.547 us | 131.945 us |
| Warm serial, median sample p95 | 139.325 us | 151.706 us |
| Concurrency 32, wall time/completed RPC | 28.554 us | 36.968 us |
| Concurrency 32, median sample individual p95 | 956.691 us | 886.995 us |
| Large RPC, median sample mean | 0.560 ms | 0.744 ms |

These loopback numbers do not establish cloud latency or a universal winner.
Auto still prefers H3 as the transport contract requires.

The H3 UDP relay control averaged 151.932 us/RPC. Dropping every twentieth
observed datagram independently in each direction increased the median sample
mean to 6.446 ms and median sample p95 to 29.198 ms. Raw samples preserve
packet/byte/drop counts. This measures QUIC loss recovery only; TCP loss was
not injected. With UDP unavailable, Auto took 257.295 ms for its cold
probe/fallback and 165.349 us/RPC for cached H2 calls. Counters assert one probe
and one fallback. Wrong-CA refusal took a median 3.866 ms, with one probe,
no H2 fallback and no application work.

Raw [samples](measurements/transport-after-close.csv),
[cold samples](measurements/transport-after-close-cold.csv),
[certificate refusals](measurements/transport-after-close-refusals.csv),
[summary](measurements/transport-after-close-summary.json) and
[metadata](measurements/transport-after-close-metadata.json) preserve this campaign.
The executable alone used 1.359 user and 1.006 system CPU seconds over
6.557 wall seconds, peaking at 61,736 KiB RSS. Compilation is excluded.
These totals include setup, validation and digest work. Batch CPU ticks have
100 Hz resolution. Per-RPC allocations and live memory were not measured.

The earlier security-tree campaign remains in [samples](measurements/transport-after-security.csv),
[cold samples](measurements/transport-after-security-cold.csv),
[refusals](measurements/transport-after-security-refusals.csv),
[summary](measurements/transport-after-security-summary.json) and
[metadata](measurements/transport-after-security-metadata.json). These campaigns
do not isolate a performance effect from the remote-close classifier correction.

Reproduce the typed transport campaign with:

    cargo test --release -p pstore-server --lib peer_transport_microbench -- --ignored --nocapture

## TCP_NODELAY finding

The earlier matched campaign found H2 cold RPCs at 45.643 ms and large RPCs at
161.349 ms. Accepted TCP sockets lacked TCP_NODELAY. A socket-option test
observed that behavior before the fix, then passed with TCP_NODELAY set before
TLS. The matched rerun measured 0.683 ms cold and 0.574 ms large RPCs. That
comparison predates the dependency security patch and is kept separately:

- Before: [samples](measurements/transport-before-nodelay.csv), [cold samples](measurements/transport-before-nodelay-cold.csv), [summary](measurements/transport-before-nodelay-summary.json), [metadata](measurements/transport-before-nodelay-metadata.json).
- After: [samples](measurements/transport-after-nodelay.csv), [cold samples](measurements/transport-after-nodelay-cold.csv), [refusals](measurements/transport-after-nodelay-refusals.csv), [summary](measurements/transport-after-nodelay-summary.json), [metadata](measurements/transport-after-nodelay-metadata.json).

Do not attribute RSS differences to TCP_NODELAY: that later campaign also added
the certificate-refusal case. No comparison across the subsequent security patch
is treated as an isolated socket-option experiment.

## Application split search

One campaign per transport, 48 segments of 2,000 rows, 64 dimensions, three
co-located peers sharing one MemoryStore. Each case warms five queries, then
records 100. Every split result array is compared with unsplit execution;
no-fallback counters are asserted. This is an application latency measurement,
not just the echo RPC. The backend has either no injected wait or 20 ms before
every read. It is not a cloud storage latency model.

| Backend | Query | H2 unsplit p50/p95 ms | H2 split p50/p95 ms | H3 unsplit p50/p95 ms | H3 split p50/p95 ms |
|---|---|---:|---:|---:|---:|
| cpu | dense | 1.63/1.79 | 1.57/1.69 | 1.58/1.67 | 1.54/1.70 |
| cpu | text | 0.83/0.88 | 1.05/1.13 | 0.71/0.77 | 1.08/1.18 |
| cpu | hybrid | 1.75/1.90 | 2.05/2.17 | 1.74/1.89 | 2.05/2.13 |
| wait | dense | 94.08/97.02 | 94.68/96.86 | 95.12/96.99 | 95.10/97.05 |
| wait | text | 91.96/93.99 | 93.26/95.44 | 92.24/94.14 | 92.99/95.72 |
| wait | hybrid | 95.87/97.87 | 96.61/99.16 | 95.85/97.77 | 97.22/99.56 |

The split does not establish a general latency win on this shared host: CPU-only
text/hybrid cases were slower; the injected-wait cases were approximately even.
Placement differs between campaigns (H2 shares 22/10/16 segments; H3 shares
20/17/11), so these are comparisons against each campaign's own unsplit baseline,
not an isolated H2-versus-H3 application experiment. The raw parts/query counter
is two for split and zero for unsplit; it is not a count of text-phase wire RPCs.

[CSV percentiles](measurements/split-search.csv),
[H2 output](measurements/split-search-h2.txt),
[H3 output](measurements/split-search-h3.txt),
[H2 metadata](measurements/split-search-h2-metadata.json) and
[H3 metadata](measurements/split-search-h3-metadata.json) preserve the evidence.
The H2 executable used 25.524 user / 5.092 system CPU seconds over 92.045 wall
seconds; H3 used 26.643 / 5.226 over 92.418 seconds. Compilation is excluded.
Totals include corpus construction, warmups, validation and untimed unsplit
comparison queries. They are not per-query CPU costs. Search allocations/RSS
were not measured.

Reproduce each transport with:

    PSTORE_SPLIT_TRANSPORT=h2 scripts/split-bench.sh
    PSTORE_SPLIT_TRANSPORT=h3 scripts/split-bench.sh

Remote vector/phased-text controls separately prove matching epochs, actual
whole/open/scan work and consumed held state. The current recall, NDCG and
20,000-row depth/byte gates also passed; sparse depth was 1 + 1 with 6,358 bytes,
text depth 1 + 1 with 2,024 bytes. Those instrumented gates establish budgets;
application wall time divided by the injected delay does not measure depth.
No real-cloud latency, scaling throughput or total TCO is claimed here.
