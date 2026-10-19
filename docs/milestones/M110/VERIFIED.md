# M110 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, Docker 29.3.1, base images pulled from
`mirror.gcr.io` (Docker Hub answered 429) and tagged with the names the Dockerfile uses. Here
`deb.debian.org` is refused by the network policy (`CONNECT tunnel failed, response 403`).

⚠️ **The whole file was not built here.** The build stage's `cargo build` fails in this
container before it compiles anything. The session's proxy re-signs TLS, so rustup gets
`invalid peer certificate: UnknownIssuer`, and putting that proxy's CA into an image was refused
by this session's policy. That is why `scripts/node-image.sh` builds the binary offline instead.
The build stage itself is unchanged by M110.

1. `grep -n '^RUN' dev/Dockerfile.node` prints only line 9, the build stage's `cargo build`.
   The runtime stage starts at line 19.
2. `./scripts/node-image.sh` exits 0, as root and as a non-root user in the `docker` group,
   and leaves no image and no `/tmp` build behind. Review round 1 found the non-root run
   red: the build container wrote root-owned files, the trap's `rm` failed, and the script
   exited 1 after printing `ok`. That was reproduced here, then fixed by running the build as
   the caller's uid. **Red:** `./scripts/node-image.sh --old a4a0af3` exits 1
   with `process "/bin/sh -c apt-get update && apt-get install ..." did not complete
   successfully: exit code: 100`. Built normally, the full file at `a4a0af3` printed
   `Ign:1 http://deb.debian.org/debian bookworm InRelease` at that step.
3. `./scripts/node-image.sh` printed `ok: runtime stage built with no network; CA bundle
   a3413a37…f72d37d matches rust:1.90-slim-bookworm`. That image carries
   `ca-certificates 20230311+deb12u1`.
4. `./scripts/cluster.sh up 3` printed `up: 3 nodes`, and `converge` printed `converged: every
   node sees 3 in 74ms = 0 periods (provisional)`. ⚠️ **Not the command as written.** It ran
   from a copy with only the `docker build` line removed, which is M104's workaround. The
   image was built by hand: the repo's runtime stage, verbatim, behind the same offline binary,
   tagged `pstore-node`. This criterion shows a node runs on the new base. It says nothing
   about TLS; criterion 3 covers the roots.
5. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.
