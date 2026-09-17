# M7g — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Criteria 1–5 and 7 are verified by `scripts/byoc.sh`, which needs Docker and is therefore
not in `cargo test` and not in AGENTS.md's Gates table.** It is in [`dev/README.md`](../../../dev/README.md)'s
table beside `conformance.sh`, which is the same status for the same reason. Every arm below
was observed green on a real run; the arms named as falsified were also observed **red** with
the behaviour removed.

1. **The image builds and serves** — `./scripts/byoc.sh` builds `dev/Dockerfile.server` and runs
   it against MinIO; a `durable` write and a query round-trip over HTTP against the container.
   ⚠️ A `.dockerignore` was required to make this run at all: the build context was measured at
   **82 GB**, almost all of it `target/`, and the build timed out before the compiler started.
2. ⚠️ **Two containers, one bucket, and the write crosses the process boundary** — A writes and
   folds on lane 1; B, a different process with an empty memtable, returns the document, and
   returns `404` **after A's write and before A's fold**. Then the reverse: B writes and folds
   on lane 2 and A returns **both** documents.
   ⚠️ **The `404` arm was vacuous when first written** — it ran *before* A's write, where
   container A would have answered `404` too. Moved between the write and the fold and then
   falsified: aimed at A it returns `doc-1` with `"unfolded_hits":1` and the script exits 1.
   ⚠️ **The reverse arm exists because code review found the lane registry untested.**
   the lane registry's register call happens on the flush path, so a container that only queries never
   registers its lane — and this system CASes exactly two objects, HEAD and that registry.
   Without B writing, the one with no ABA nonce behind it was untouched by the milestone whose
   whole purpose is two writers against one bucket. `./scripts/byoc.sh`.
3. **An unprobed profile refuses to serve** — `./scripts/byoc.sh`, and the profile's two
   capability shapes are pinned by `an_unprobed_s3_profile_cannot_fence_and_a_conforming_one_can`
   and `s3_without_an_endpoint_is_refused_at_startup`
   (`cargo test -p pstore-server --test deploy`). With `PSTORE_BACKEND=s3` and no profile set:
   the container exits **1**, names the backend `s3(http://127.0.0.1:9210)` and the primitive
   `cas=Divergent`. ⚠️ The backend must be set: with the default `memory` the profile is
   `Supported` and the container serves correctly, so an arm that omitted it would pass while
   proving nothing. ⚠️ The arm is wrapped in `timeout 30` and treats **124 as a failure** —
   code review found that without it, a regressed door guard means the container *serves*, the
   command substitution never returns, and the assertion below it is unreachable.
4. **A lane is required in the image too** — `./scripts/byoc.sh`: with no lane in the
   environment the container exits **1** and the message names the variable. Same `timeout` treatment, for the same reason: a default lane baked into
   the entrypoint would otherwise hang the gate rather than fail it.
5. **`/metrics` is reachable from outside the container** — `./scripts/byoc.sh`; B's scrape
   carries
   `pstore_http_requests_total{route="/v1/indexes/{index}/query"}` and
   `pstore_blob_requests_total{class="list"} 0`.
   ⚠️ **This caught a real defect in the harness on its first run**: the script bound both
   servers to `127.0.0.1:$port`, overriding the image's `PSTORE_BIND=0.0.0.0:8080` — which is
   exactly the mutation this criterion names, made by the test rather than by the image. Every
   request is now made from a container on the host network, because a shell here measurably
   cannot reach a host-network container's port, and a gate that hangs while the server is
   healthy is worse than one that fails.
6. **Both confinements are checked by the same gate** — `deny.toml` bans the object_store
   crate outside `pstore-blob`, `pstore-node`, `pstore-server` **and `axum` outside
   `pstore-server`**,
   which until now was a comment in `Cargo.toml` and therefore not a gate at all. Verified by
   `cargo deny check bans`, in `./scripts/gates.sh`.
7. **The deployment document cannot fall behind the code** — the ids served at
   `/v1/admin/duties` equal the ids under `docs/deploy.md`'s `## Unscheduled duties`:
   `auth fold reap tls`. ⚠️ The extraction rule is part of the criterion, and
   `every_duty_id_is_a_lower_case_token` (`cargo test -p pstore-server --test deploy`) is what
   stops a duty id that the gate's character class would silently fail to match — the vacuity
   mode of every grep-shaped check. ⚠️ **Honest limit**: only the *ids* are compared. The prose
   under each heading is hand-written and can disagree with the constant's `instead` line;
   `deploy.md` says so rather than claiming to be generated.
8. **Region coverage ≥95%** — `./scripts/coverage.sh --fail-under-regions 95`: `pstore-server`
   95.67% regions, workspace total 95.11%. ⚠️ `main.rs` is excluded as a composition root, which
   is why criteria 3 and 4 are asserted against a running container. The decisions were moved
   out of it as review asked: the endpoint requirement, the profile's capabilities and the
   credential pairing are all in `lib.rs`, with unit tests. Mutation sweep over the two
   changed server files: **76 mutants, 28 caught, 48 unviable, 0 missed**
   (`./scripts/mutants.sh --file crates/pstore-server/src/lib.rs --file crates/pstore-server/src/types.rs`,
   run in the dev container).

## Not claimed

- ⚠️ **Nothing here says anything about S3.** MinIO is an emulator and D-99 is explicit that an
  emulator tests plumbing. What criterion 2 proves is that the *process boundary* is crossed.
  A real account is still M0b.
- ⚠️ **`PSTORE_PROFILE=conforming` is the operator's word**, re-probed by nothing at runtime.
- ⚠️ **The image is not hardened**: root, writable filesystem, no seccomp profile.
- ⚠️ **There is no Azure or GCS backend**, and backlog row 32 records why with C-14's three
  exits — the fix is a format decision needing a real account, not an afternoon.
