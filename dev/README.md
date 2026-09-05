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
| **In-process fault-injecting store** | `cargo test` | **Correctness.** The only backend with exact, assertable semantics. |
| Emulators (MinIO/Azurite/fake-gcs) | `cargo test --features integration` | Plumbing: HTTP, auth, retries, encoding, error mapping |
| Conformance suite | `cargo test -p pstore-conformance` | Records what each backend actually does |
| Real clouds | **deferred — milestone M0b** | Economics: latency, CAS contention, cost |

## The thing to know

**No emulator implements our core primitive faithfully.** MinIO rejects
`If-None-Match: *` (it wants an exact ETag), Azurite got `If-Match: "*"` on a
missing blob wrong until recently, SeaweedFS breaks it under versioning. So:
correctness is proven in-process; emulators test plumbing only; CAS semantics are
confirmed against real clouds in M0b. Backend versions are pinned because this
behaviour changes between releases.
