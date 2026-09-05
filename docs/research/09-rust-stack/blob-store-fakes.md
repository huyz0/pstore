# Building Our Own S3 Fake

**Answers:** Q45
**Status:** Complete (v1)
**Follows from:** [`dev-and-test-environment.md`](dev-and-test-environment.md) — no emulator
implements our core CAS primitive faithfully.

## 1. Yes — but be precise about what it is

This is not a replacement for the in-process fault-injecting store (D-3). They test **different
layers**, and confusing them would leave a real gap:

| Layer | Tests | Speed |
|---|---|---|
| **In-process `BlobStore`** | *Our logic*: commit protocol, Invariant I1, bundle recovery, lane merge. No HTTP at all. | µs, every `cargo test` |
| **Fake S3 over HTTP** | *Our client*: `object_store` → reqwest → wire → XML → error mapping. Plus protocol-level faults. | ms, integration tests |
| Emulators (MinIO etc.) | Third-party plumbing sanity | ms |
| Real S3 | Economics: latency, contention, throttling | M0b |

> **D-105.** Build **`pstore-fake-s3`**: a deliberately small S3 server implementing exactly
> AWS's documented semantics for the operations we use, with protocol-level fault injection. It
> supplements the in-process store; it does not replace it.

## 2. Build on `s3s`, not from scratch

The [`s3s`](https://docs.rs/s3s) crate is precisely the right foundation:

- It *"implements Amazon S3 REST API in the form of a generic hyper service"* — we implement an
  `S3` trait with async methods and get the HTTP layer for free.
- It converts HTTP requests into operation inputs and outputs/errors back into HTTP responses,
  so *"S3-compatible services can focus on the S3 API itself."*
- **The data types and (de)serialization are generated from the Smithy model in the
  `aws-sdk-rust` repository** — which means the wire format is derived from AWS's own machine
  -readable spec rather than from our reading of the docs. That is a meaningful reduction in the
  surface where we can be wrong.
- `s3s-fs` is a filesystem-backed sample implementation *"designed for integration testing which
  can be used to mock an S3 client"* — a starting point, and `s3s-test` exists as a harness.

So the work is ~12 operation bodies plus a fault-injection layer, not an object store.

## 3. The circularity trap, and how to escape it

This is the failure mode to design against, and it is easy to walk into:

> **If we write a fake encoding *our belief* about S3's semantics, then test against it, we are
> testing our belief.** A wrong belief passes every test and fails in production — with more
> confidence than if we'd had no fake at all.

Four defences, in order of strength:

1. **The conformance suite is the contract, not the fake.** The suite from
   [`dev-and-test-environment.md`](dev-and-test-environment.md) §3 is written against the AWS
   *documentation*, and **both `pstore-fake-s3` and real S3 must pass it identically**. In M0b
   any divergence is a bug in the fake — or a discovery about S3, which is equally valuable.
2. **Cite the spec inline.** Every semantic rule in the fake carries a comment quoting the AWS
   documentation sentence it implements, with a link. A rule that cannot be traced to a
   citation is a guess and must be labelled as one.
3. **Adopt `ceph/s3-tests`** as an independent external corpus. Note that *conditional writes
   were only recently added to it* (#583) — which independently confirms this corner is
   under-tested across the whole ecosystem, and is part of why every emulator gets it wrong.
4. **Fail loudly on anything unimplemented.** An unimplemented operation returns
   `NotImplemented`, never a plausible-looking success. A fake that silently succeeds on a path
   it doesn't model is worse than no fake.

> **D-106.** The fake is **deliberately incomplete and loud about it.** Scope creep toward
> "a working object store" is a failure mode: every operation added is more of our belief under
> test, and we have no way to validate it until M0b.

## 4. Scope: the operations we actually use

| Operation | Why |
|---|---|
| `PutObject` **with `If-None-Match: *` and `If-Match: <etag>`** | **The reason this exists.** |
| `GetObject` with `Range` and `If-None-Match` (→ 304) | Ranged reads; cheap freshness checks |
| `HeadObject` | GC and repair only |
| `DeleteObject` / `DeleteObjects` (≤1000) | Reaping |
| `CreateMultipartUpload` / `UploadPart` / `CompleteMultipartUpload` (+ `If-Match`) / `AbortMultipartUpload` | Large segments |
| `ListObjectsV2` | The three sanctioned uses only |

Plus correct error responses — the part MinIO gets wrong and the part we depend on:

| Status | Code | Meaning |
|---|---|---|
| **412** | `PreconditionFailed` | **We lost the race.** Rebase and retry. |
| **409** | `ConditionalRequestConflict` | **S3 couldn't evaluate the condition** — concurrent write in flight. Retry *without* rebasing. |
| 503 | `SlowDown` | Throttling; normal signal, not an error |
| 404 | `NoSuchKey` | Bounds the lane tail during forward probing |

> The 412/409 distinction is the sharp one. Conflating them causes needless rebase storms
> ([`../03-metadata-consistency/manifest-and-cas.md`](../03-metadata-consistency/manifest-and-cas.md) §5),
> and **no emulator produces 409 at all** — so this behaviour is currently untestable anywhere.

## 5. What this unblocks *now*

The real payoff is a set of questions that were parked pending cloud accounts:

| Was blocked | Now testable |
|---|---|
| **OQ-6** — does the ABA nonce close the hole under S3 multipart ETags? | ✅ Synthesize the `md5(concat(part_md5s))-N` ETag form, and SSE-KMS-style ETags that are **not** MD5 at all. **Design rule 3** ("treat ETags as opaque tokens") becomes an executable test rather than a stated intention. |
| 412 vs 409 handling | ✅ Inject 409 deliberately; assert we retry without rebasing |
| Congestion controller under `503 SlowDown` | ✅ Inject 503 with `Retry-After`; assert per-prefix backoff behaves |
| Retry / timeout / partial-body handling | ✅ Truncated bodies, mid-stream connection resets |
| Round-trip depth invariant (**D-34**) end-to-end | ✅ Inject a latency distribution matching published S3 figures; assert sequential depth ≤3 **over real HTTP**, not just in-process |
| Read-after-overwrite and 304 semantics | ✅ |

**Still blocked, and honestly so:** real latency distributions, real CAS throughput under
contention (OQ-5), real throttling thresholds, `G*` (OQ-2). A fake cannot tell us how S3
*performs* — only how we *behave* when S3 does something. That distinction should stay sharp in
the docs.

## 6. Fault-injection catalogue

The fake's second job, and the one the in-process store cannot do because it never speaks HTTP:

```
faults:
  latency:        per-op distribution (p50/p99/p999), configurable per prefix
  status:         412 / 409 / 503 / 500 at a given rate or on a matched key
  etag_style:     Md5 | MultipartConcat | Opaque(KmsLike)   # Design rule 3
  body:           Truncate(at) | ResetMidStream | SlowDrip(bps)
  visibility:     DelayedBy(dur)     # for backends weaker than S3
  clock_skew:     ±dur on Last-Modified / Date
  concurrency:    force 409 when N in-flight writes hit one key
```

All seeded and deterministic, so a failing test replays exactly.

## 7. Should we fake GCS and Azure too?

**Not speculatively.** Three fakes is three times the belief-under-test, and the decision should
be evidence-based:

> **D-107.** Run the conformance suite against `fake-gcs-server` and Azurite first, and **fake
> only what fails.** The suite exists precisely to make this decision on data.

Two prior expectations:
- **GCS may need nothing.** Generations are monotonic integers rather than opaque ETags —
  unambiguous, immune to the ABA hazard, and the cleanest CAS story of the three. If
  `fake-gcs-server` is faithful to `ifGenerationMatch` ([OQ-153](../00-plan/open-questions.md)),
  **GCS becomes the better primary development target than S3.**
- **Azurite may now be adequate** — its `If-Match: "*"` bug is fixed. Pin the version and let
  the suite confirm.

## 8. A note on the boundary

Keep `pstore-fake-s3` a clean standalone crate with no dependency on `pstore` internals. Two
reasons: it stops the fake from drifting toward "whatever makes our tests pass", and a correct,
fault-injecting S3 fake with real conditional-write semantics is genuinely useful to anyone else
building on object storage — SlateDB, Iceberg implementations, other zero-disk systems. Not a
goal, but a cheap option to preserve.

## 9. Cost

~12 operation bodies on `s3s` plus the fault layer: on the order of **1,500–2,500 lines, one to
two weeks**. Against the fact that compare-and-swap is the foundation of the entire architecture
and is currently **untestable anywhere**, that is clearly worth it.

Slots into **M0a**, ahead of the sensitivity sweeps — the sweeps want a realistic transport to
run against.

## 10. Open questions raised

- OQ-154 — Does `s3s` model conditional-request headers on `PutObject`, or do we handle them
  ourselves above the generated types? Determines whether this is one week or two.
- OQ-155 — Should the fake be exercised by `ceph/s3-tests` in CI, or is that corpus too broad
  for our deliberately narrow scope? Probably run it and record failures as *expected* for
  unimplemented operations.
- OQ-156 — Can the fake also serve as the **GCS and Azure** fake behind a translation layer, or
  are the semantics different enough that separate implementations are cleaner? Generations vs.
  ETags suggests separate.

## Sources

- [s3s — docs.rs](https://docs.rs/s3s)
- [s3s — crates.io](https://crates.io/crates/s3s)
- [s3s-fs — crates.io](https://crates.io/crates/s3s-fs/0.8.1)
- [s3s-test — crates.io](https://crates.io/crates/s3s-test)
- [ceph/s3-tests — Compatibility tests for S3 clones](https://github.com/ceph/s3-tests)
- [Add conditional writes — ceph/s3-tests #583](https://github.com/ceph/s3-tests/issues/583)
- [Add preconditions to S3 operations with conditional requests — AWS docs](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-requests.html)
- [Building multi-writer applications on Amazon S3 using native controls — AWS Storage Blog](https://aws.amazon.com/blogs/storage/building-multi-writer-applications-on-amazon-s3-using-native-controls/)
- [MinIO is not compatible with new S3 conditional write feature — minio/minio #20346](https://github.com/minio/minio/issues/20346)
