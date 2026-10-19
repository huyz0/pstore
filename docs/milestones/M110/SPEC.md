# M110 — the node image builds without a Debian mirror

**Serves:** M104's caveat that `./scripts/cluster.sh up` could not build its own image. No
backlog row: this is a harness defect, found because it blocked a fleet run.

## What is true today

- **The runtime stage runs `apt-get`.** `dev/Dockerfile.node` installs `ca-certificates` into
  `debian:bookworm-slim`. Where `deb.debian.org` is unreachable, that step prints `Ign:` for
  every source and the image does not build. M104 worked around it by hand. It built the binary
  on the host and ran `cluster.sh` from a copy with the build line removed. So the fleet it
  measured ran a base image the repository does not describe.
- **The file was already one stage away.** The build stage, `rust:1.90-slim-bookworm`, carries
  Debian's `ca-certificates` package for the same release (bookworm). Its bundle sits at
  `/etc/ssl/certs/ca-certificates.crt`.
- **The node needs that file.** object_store's TLS goes through `rustls-platform-verifier`,
  which on Linux reads the platform's roots through `rustls-native-certs`. Without the file, the
  node cannot verify a real cloud endpoint.

## Delta

1. The runtime stage copies `/etc/ssl/certs/ca-certificates.crt` from the build stage. It has
   no `RUN` step left.
2. `scripts/node-image.sh` builds that stage verbatim, with `--network=none`, behind a stage
   that only holds a binary. It builds the binary with `cargo build --offline` inside the build
   stage's own image, then checks the image's CA bundle against that image's. It takes
   `--old REV` to run the same check on the stage at REV. It is `portable: no` because it
   needs Docker, so it stays out of `gates.sh` and runs by hand, as `byoc.sh` does.

**Not changed:** the build stage, the base images and the binary. Not covered: the build
stage's own network needs. It still fetches crates, and on a host whose proxy re-signs TLS it
needs that proxy's CA. That is the host's configuration, not the image's.

## Acceptance criteria

1. **The runtime stage installs nothing.** No `RUN` line follows the last `FROM` in
   `dev/Dockerfile.node`.
2. **The runtime stage builds with no network.** `./scripts/node-image.sh` exits 0. Run with
   `--old` against M110's parent, it fails at the `apt-get` step.
3. **The roots are Debian's.** `./scripts/node-image.sh` finds the image's
   `/etc/ssl/certs/ca-certificates.crt` byte-identical to the build image's (`sha256sum`).
4. **The fleet runs on it.** `./scripts/cluster.sh up 3` with that image starts 3 nodes, and
   `converge` sees 3 of 3.
5. `./scripts/gates.sh` passes.
