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
| Emulators (MinIO/Azurite/fake-gcs) | `scripts/conformance.sh` | Third-party plumbing sanity — a compatibility check, not the default target. Writes [`docs/profiles/capability-matrix.md`](../docs/profiles/capability-matrix.md); `--check` fails if a backend has changed under us. ⚠️ Outside `cargo test` because it needs the containers |
| Conformance suite | `cargo test -p pstore-testkit --test conformance` (local stores) · `scripts/conformance.sh` (emulators) | Records what each backend actually does. ⚠️ It lives in `pstore-testkit::conformance`; there is no `pstore-conformance` crate and there never was |
| **Two containers, one bucket** | `scripts/byoc.sh` | ⚠️ **The only test here that crosses a process boundary.** Builds `dev/Dockerfile.server`, runs two servers on two lanes against one MinIO, and asserts that a document written and folded through one is returned by the other — and returns 404 before the fold. Also the image's refusal arms (unprobed profile, missing lane) and the `docs/deploy.md` gate. ⚠️ Outside `cargo test`, and outside AGENTS.md's Gates table, because it needs Docker and CI has none |
| Real clouds | **deferred — milestone M0b** | Economics: latency, CAS contention, cost |

## Running the 100-node fleet on WSL2

`scripts/cluster.sh up 100` uses **host networking**, and that is not a stylistic choice.

A bridged container needs an entry in the host's **ARP neighbour table** per node. Its
default ceiling is `net.ipv4.neigh.default.gc_thresh3` (1024, with garbage collection from
128), and once a bridge is busy enough to cross it the kernel starts **dropping packets
silently** and logs:

```
neighbour: arp_cache: neighbor table overflow!
```

Measured here, that surfaced as joins stalling at ~33 of 100 nodes with the blob store idle
at 1% CPU, every stuck node sitting in `SYN_SENT`, and the store's listen queue reporting
zero overflows — a network failure wearing a distributed-systems costume. Raising the sysctl
needs root on the **WSL2 host**, which the dev container does not have; host networking
removes the entries instead of raising the ceiling.

⚠️ **What it costs the numbers.** The nodes become 100 processes on one loopback rather than
100 network peers, so convergence and gossip traffic measured this way are a **floor** — a
real network can only be slower. Per-node RSS and CPU are unaffected, because `--memory` and
`--cpus` still apply. [`M4b/VERIFIED.md`](../docs/milestones/M4b/VERIFIED.md) states this
beside every timing it reports.

If you have root on the host and prefer a bridge, this is the knob:

```bash
sudo sysctl -w net.ipv4.neigh.default.gc_thresh1=4096 net.ipv4.neigh.default.gc_thresh2=8192 net.ipv4.neigh.default.gc_thresh3=16384
```

`scripts/cluster.sh down` also removes the Docker network. Reusing a bridge across runs of
~100 containers was observed to leave it in a state where **new** containers got no
connectivity at all while existing ones kept working.

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
