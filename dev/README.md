# Local dev environment

WSL2 host, everything containerized and resource-capped. Rationale and the
research behind it: [`../docs/research/09-rust-stack/dev-and-test-environment.md`](../docs/research/09-rust-stack/dev-and-test-environment.md).

## Setup

1. Copy `wslconfig.sample` to `%UserProfile%\.wslconfig`, then `wsl --shutdown`.
2. Copy `cargo-config.sample.toml` to `.cargo/config.toml`.
3. **Keep this repo in the Linux filesystem (`~/`), never under `/mnt/c`** — 9p
   cross-filesystem IO is an order of magnitude slower and makes every build feel broken.
4. `docker compose -f dev/docker-compose.yml up -d`

## Three resource ceilings, deliberately nested

| Layer | Limit | Why |
|---|---|---|
| WSL2 VM | `.wslconfig` | Hard ceiling; protects Windows |
| Container | compose `deploy.resources` | A runaway test OOMs one container, diagnosably |
| Build | `CARGO_BUILD_JOBS`, mold, `debug=1` | Linking is the peak |

## Testing layers

| Layer | Runs | Proves |
|---|---|---|
| **In-process fault-injecting store** | `cargo test` | **Correctness of our logic.** Exact, assertable semantics; no HTTP. |
| **`pstore-fake-s3`** (ours, on `s3s`) | `cargo test --features integration` | **Correctness of our client**: wire, XML, error mapping. Correct `If-None-Match: *`, and the only place 409 vs 412 is testable. |
| Emulators (MinIO/Azurite/fake-gcs) | `cargo test --features compat` | Third-party plumbing sanity — a compatibility check, not the default target |
| Conformance suite | `cargo test -p pstore-conformance` | Records what each backend actually does |
| Real clouds | **deferred — milestone M0b** | Economics: latency, CAS contention, cost |

## The thing to know

**No emulator implements our core primitive faithfully.** MinIO rejects
`If-None-Match: *` (it wants an exact ETag), Azurite got `If-Match: "*"` on a
missing blob wrong until recently, SeaweedFS breaks it under versioning. **So we
build our own** — `pstore-fake-s3`, on the `s3s` crate, with exactly AWS's
documented semantics and protocol-level fault injection.

The trap to avoid: a fake we write encodes *our belief* about S3, so testing
against it tests our belief. The escape is that the **conformance suite is the
contract** — the fake and real S3 must pass it identically in M0b, and any
divergence is a bug in the fake. See
[`../docs/research/09-rust-stack/blob-store-fakes.md`](../docs/research/09-rust-stack/blob-store-fakes.md).

Emulator versions are pinned here because this behaviour changes between releases.
