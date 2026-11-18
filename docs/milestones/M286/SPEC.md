# M286: pstore's own S3 client, measured against `object_store`

## Serves
D-1 and D-2 ([rust-object-store-crates.md](../../research/02-object-storage/rust-object-store-crates.md)):
whether `pstore-blob` keeps `object_store` under `BlobStore`, and RUSTSEC-2026-0543 (chrono)
and RUSTSEC-2026-0542 (humantime), which `object_store` 0.14 cannot be configured out of.

## Delta
1. **`pstore-blob` gains `S3Backend`** behind a new feature `s3-native`: a `BlobStore` over
   `hyper` 1 with SigV4 signed in-crate with `ring` (already in the lockfile). No new crates.
   Every method of the trait, one HTTP request each, `delete_batch` as one `DeleteObjects`.
   - **No retries inside the client.** `Congested` is the one retry layer, so it sees every
     `503`; M282 needed a second `object_store` client to get this for conditional writes.
   - **Errors are typed from the status**, never from a message: 404 absent, 412 `Lost`,
     409 `Contended`, 503 or `SlowDown` throttled, a short `206` `RangeOutOfBounds`.
   - A body is collected into one buffer sized from `Content-Length`; a one-frame body is
     handed on without a copy.
   - Credentials: static or `AWS_*` environment only in this milestone.
2. **A benchmark, `examples/s3_clients.rs`**: a fake S3 in a **separate process** on
   loopback, so the bench process's CPU and allocations are the client's alone. Both clients
   run pstore's patterns against it: a WAL bundle's conditional create (16 KiB), a ranged
   GET (64 KiB), a suffix GET (64 KiB), 256 concurrent ranged GETs, a 32 MiB segment PUT,
   a HEAD, a 1000-key delete. Per pattern: requests a second, client CPU a request, heap
   allocations and bytes allocated a request.
3. **A decisions doc**, `docs/comparisons/object-store-layer-2026-11-18.md`: OpenDAL,
   `aws-sdk-s3`, `rusty-s3`/`reqsign` and this client weighed, with the numbers and the
   downsides, and the recommendation.

**Not changed:** `pstore-server` still builds `object_store`, Azure and GCS are untouched,
no on-disk byte changes. Wiring the client in as the server's S3 client (a cluster setting,
`PSTORE_S3_CLIENT`; per index makes no sense, one server has one store) and the credential
chain (IMDSv2, ECS, web identity) are the next milestone, **only if** criterion 4 holds.

## Acceptance criteria
1. `S3Backend` passes the same conformance cases `ObjectStoreBackend` does, against an
   in-process fake S3: `native_s3_conformance`.
2. Its SigV4 matches AWS's published examples, the SigV4 suite's `get-vanilla` and the S3
   documentation's ranged GET Object: `sigv4_matches_the_aws_examples`.
3. A `503` and a conditional `409` reach the caller typed and after exactly one request:
   `a_503_is_slowdown_after_one_request`, `a_409_is_contended`.
4. The decision rule, measured by the bench: build on only if the own client spends at
   least 25% less client CPU or allocates at least 25% fewer bytes a request on at least
   four of the seven patterns, and is no worse than 10% on any; otherwise the doc
   recommends keeping `object_store` with a documented advisory ignore.
5. `cargo tree -p pstore-blob --features s3-native --no-default-features` names neither
   `chrono` nor `humantime`: recorded in VERIFIED.md.

## Test plan
- 1: written against `ObjectStoreBackend`'s behaviour first; fails red until each method
  exists (mutation: a `get_range` that drops the short-read check).
- 2: the signer is written after the vectors; red until it is exact (mutation: an
  unsorted canonical query, an unescaped `/` in a key).
- 3: the fake answers 503 once and counts requests; a retrying client answers `Ok` after
  two (mutation: a retry loop inside the client).

## RA budget
Unchanged: one request per trait call, the same as `ObjectStoreBackend`.

## Risks
- Loopback has no network latency, so it measures the client's overhead only. That is the
  question; the doc says what share of a 20 ms S3 round trip it is. `provisional`.
- An own client owns every S3 quirk `object_store` already absorbed (redirects, chunked
  responses, `x-amz-checksum`). The conformance suite is what would show a gap.

## Tasks
- M286.1 SPEC.
- M286.2 `S3Backend`, its signer, its tests.
- M286.3 the bench and the decisions doc, VERIFIED.md.
