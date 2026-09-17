# M7g — BYOC packaging, and the first proof across two processes

**Serves:** M7's fourth bullet (*BYOC packaging*), **D-99/D-100** (a backend serves only the
profile the conformance suite recorded), and the promise M7c deferred in as many words: *"a
real backend is a milestone with a cloud account in it."*

**Depends on** [M7a](../M7a/SPEC.md) (the measured capability matrix and the refusal that
rests on it), [M7c](../M7c/SPEC.md) (the server), [M7f](../M7f/SPEC.md) (`/metrics`, which is
what makes a container observable).

## ⚠️ What packaging is actually for, here

A BYOC image is not a Dockerfile. It is the first configuration in which the claims this
repository makes are **testable the way a customer will meet them**: two processes, one
bucket, no shared memory. Every durability proof so far has been two `Api`s in one test
binary — which is honest about the memtable and says nothing about a process boundary.

So the deliverable is the image **and** `scripts/byoc.sh`, which runs **two server containers
against one bucket**, writes and folds through the first, and queries the second. Nothing in
this repository has done that.

## ⚠️ Why S3, and why this milestone ships no Azure backend at all

An earlier draft of this spec chose Azurite, on the matrix's CAS rows. **That was wrong, and
the review caught it.** Every segment open goes through `Segment::open`'s
`get_suffix_as(key, SUFFIX_FETCH, Class::Meta)`, and **C-14** records suffix reads as absent
on Azure three ways: the client refuses `GetRange::Suffix` before building a request, Azurite
answers `bytes=-1` with a **500**, and the Blob REST API has no suffix form. Container B would
therefore fail *every query* — which is the milestone. C-14 also says the fix (an absolute
footer offset, the object length in the manifest, or accepting 3 `Rseq` on Azure) is a design
decision with its own spec. It is not this one, so `PSTORE_BACKEND` **has no `azure` value**:
an operator cannot reach the broken path, because it is not offered.

S3 is what is left, and MinIO measures `cas`, `create_if_absent` **and** `suffix_read` as
`Supported`. Its `aba_resistance` row is `Divergent` — content-derived ETags, so `v1 → v2 →
v1` returns the first tag — and that hazard is **already answered in the engine**:
`nonce_for(epoch, lane)` makes two commits byte-different even when everything else about them
matches, so a HEAD's content never returns to a prior value and the content-derived tag never
repeats. ⚠️ It is also not a `Capabilities` field, so `PSTORE_PROFILE=conforming` asserts
exactly the two things the matrix measured `Supported` and claims nothing about ABA.

⚠️ **The nonce is not the whole argument, and the second half is different in kind.** The
server CASes exactly two objects: HEAD, which carries the nonce, and the **lane registry**
(`lanes::register`, on the write path), which carries none. That one is ABA-safe for another
reason — its content is a `BTreeSet<LaneId>` that only ever grows, and nothing in the tree
deregisters a lane, so it can never return to a prior value. That is a monotonicity argument,
recorded here rather than left to a reader who would otherwise assume the nonce covers it.
⚠️ Criterion 2 exercises that CAS only because **both** containers write: `lanes::register`
is called from the flush path, so a container that only queries never registers its lane, and
a first draft of `byoc.sh` left the second of this system's two CAS'd objects completely
untested by the milestone that exists to test two writers against one bucket.

## ⚠️ The refusal that comes with it, and it is a feature

`ObjectStoreBackend::unprobed` marks `cas` and `create_if_absent` **Divergent**, and M7a's door
guard refuses to serve a profile that cannot fence. So a server pointed at a real bucket
**refuses to start** unless an operator states which recorded profile applies. That is correct
and it is the point: the capability matrix is a measurement, and a deployment that has not made
it is a deployment that must not accept a `durable` write.

## Delta

**Adds**
- `PSTORE_BACKEND` = `memory` (the default, and what M7c shipped) | `s3`, with endpoint,
  bucket and credentials from the environment, built the way `pstore-node::blob_store` builds
  one. `PSTORE_PROFILE` = `unprobed` (the default, which refuses) | `conforming`, naming a
  profile the operator has verified with `scripts/conformance.sh`.
- `pstore-server` takes `pstore-blob = { features = ["object_store"] }` — `ObjectStoreBackend`
  is behind that feature — **plus** its own `object_store = { features = ["aws"] }`, which is
  exactly `pstore-node`'s pair. ⚠️ **Not** `pstore-blob`'s `compat` feature, which is the
  emulator-integration feature for `examples/probe.rs` and would drag the GCS client the matrix
  says must never be a CAS target into a shipped image.
- `main.rs` constructs that backend with `ObjectStoreBackend::unprobed("s3")` unless
  `PSTORE_PROFILE=conforming`, which is what turns `Api::new`'s existing `CannotFence` refusal
  into criterion 3's non-zero exit. The refusal itself is M7c's; only the wiring is new.
- `dev/Dockerfile.server` — multi-stage, the runtime pinned to the same Debian as the build
  stage, because a newer build image links a newer glibc and every container dies with
  `GLIBC_2.38 not found`, which looks like a crash loop and is a packaging mistake.
  `dev/Dockerfile.node` already carries that scar; this one inherits the lesson.
  ⚠️ It sets `PSTORE_BIND=0.0.0.0:8080`: `Config::from_vars` defaults to `127.0.0.1:8080`,
  which inside a container is reachable by nothing.
- `pstore_server::UNSCHEDULED` — the duties the server does **not** perform, each a stable
  lower-case id (`fold`, `reap`, `tls`, `auth`) and one line of what an operator must do
  instead — served at `GET /v1/admin/duties`. See criterion 7 for why this is a constant in
  code and not a paragraph in a document.
- `scripts/byoc.sh` — builds the image, runs **two** containers on **different lanes** against
  one bucket, and asserts across the process boundary. Outside `cargo test`, for the same
  reason `conformance.sh` is: it needs Docker. ⚠️ It follows `scripts/cluster.sh`, not
  `docker-compose.yml`: a plain `docker run` is not on the compose project's network, so
  `http://minio:9000` would not resolve. Its own `minio` on `--network host`, and its own
  `mc mb -p`, because **nothing in this repository creates a bucket** except
  `conformance.sh`'s `make_s3_bucket`, which reaches MinIO through `compose exec`.
- `docs/deploy.md` — what a BYOC operator must decide, each item being something this system
  will otherwise do wrong: run the conformance suite **first**, and give every process its own
  lane. Plus one section, `## Unscheduled duties`, holding a `### ` per duty — the half
  criterion 7 checks, and the reason the other two are `## ` sections beside it rather than
  inside it.
- `deny.toml` gains `pstore-server` to `object_store`'s wrapper list, on the argument already
  recorded there for `pstore-node` — a binary must construct a real backend, and everything
  after that goes through `BlobStore` — **and a new `axum` ban wrappered to `pstore-server`
  alone**, which today is a comment in `Cargo.toml` and therefore not a gate at all.

**Does not add** — an Azure or GCS backend (above); a Helm chart or an operator; TLS
termination (a reverse proxy's job, and saying so beats shipping a half one); credential
rotation; an autoscaler; or multi-node coordination, which `pstore-node` has and the server
does not use.

## Acceptance criteria

1. **The image builds and serves**: `scripts/byoc.sh` builds `dev/Dockerfile.server`, runs it
   against MinIO, and a write and a query round-trip over HTTP against the container.
2. ⚠️ **Two containers, one bucket, and the write crosses the process boundary**: container A
   writes `durable` and folds; container B — a different process, a different lane, an empty
   memtable — returns the document. **Before** A's fold, B returns `404`. Then the same in
   reverse: B writes and folds on lane 2 and A returns **both** documents, which is what makes
   the lane registry's CAS a thing this milestone tests rather than a thing it assumes. This
   is the first time anything in this repository has asserted any of it across processes
   rather than across two objects in one test binary.
3. **An unprobed profile refuses to serve**, with the backend and the primitive named, and the
   container **exits non-zero** rather than accepting writes it cannot fence. Asserted with
   `PSTORE_BACKEND=s3` and `PSTORE_PROFILE` unset. ⚠️ The backend matters: with the default
   `memory` the profile is `Supported` and the container serves correctly, so an arm that
   omitted it would pass while proving nothing.
4. **A lane is required in the image too**: a container started without `PSTORE_LANE` exits
   non-zero with the reason, so the operator error that silently overwrites another process's
   bundles cannot be made by omission.
5. `/metrics` is reachable from **outside** the container and reports the traffic the smoke
   test generated — an operator can see a deployment they did not build.
6. `cargo deny check` passes with `pstore-server` in `object_store`'s wrapper list and `axum`
   banned outside `pstore-server`, so both confinements are checked by the same gate rather
   than one of them by a comment.
7. **The deployment document cannot fall behind the code**: `byoc.sh` fetches
   `GET /v1/admin/duties` from the running container and asserts its set of ids equals the ids
   under `docs/deploy.md`'s `## Unscheduled duties` section. ⚠️ **The extraction rule is part
   of the criterion**, or it is not checkable: each duty is a heading of the form
   ``### `fold` — one line``, and the id is the backticked token, read by
   `grep -o '^### `[a-z-]\+`'`. Scoped to that one section deliberately — `deploy.md` also
   documents conformance-first and one-lane-per-process, which are not duties, and a predicate
   over every `## ` in the file would fail the day it was written.
   ⚠️ A grep for four fixed phrases would instead test that a document contains strings the
   same commit wrote — the author controls both sides. This predicate is derived: adding a duty
   the server does not perform fails the gate until the document says what to do instead.
8. Region coverage ≥95% on the changed crates; the binary's new wiring is a composition root
   and is excluded from coverage by the existing `main.rs` rule, which is why criteria 3 and 4
   are asserted **against the running container** rather than against a unit test.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1,2 | `scripts/byoc.sh` | a server that serves only what its own memtable holds — the image would pass every in-process test and lose every write on restart |
| 2 | the pre-fold `404` in the same script | ⚠️ **a harness error, not a system mutation.** `Engine::query` re-reads HEAD every time, so B provably cannot see A's unfolded bundle; what this arm catches is both requests being aimed at container A, whose memtable answers `200` and makes the whole criterion vacuous |
| 3 | `scripts/byoc.sh` unprobed arm | the door guard lost in the move to a real backend, which is exactly where it matters and exactly where no unit test looks |
| 4 | `scripts/byoc.sh` no-lane arm | a default lane in the image's entrypoint, which is the silent-overwrite failure wearing a container |
| 5 | the `/metrics` fetch in the script | `PSTORE_BIND` left at its loopback default — an endpoint nobody outside the container can scrape |
| 7 | the duties arm | a duty added to `UNSCHEDULED` with no deployment instruction, and a deployment document that has quietly gone stale |

⚠️ **Criterion 2 is the milestone.** Everything else is packaging; that one is the first
evidence that the architecture works the way the README describes it.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Everything | **unchanged** — this milestone adds no code on any serving path | unchanged | unchanged | 0 |
| `/v1/admin/duties` | 0 | 0 | 0 | 0 — a constant, no store access |
| The smoke test | a handful of requests against MinIO | — | — | 0 |

## Risks

- **MinIO is not S3**, and D-99 is explicit that an emulator tests plumbing. What criterion 2
  proves is that the *process boundary* is crossed, not that S3 behaves; the capability matrix
  already says which claims are emulator-shaped, and a real account is still M0b.
- **`PSTORE_PROFILE=conforming` is an operator's word.** Nothing at runtime re-probes. That is
  the same trade M7a made — `--check` is a gate someone must run — and the deployment document
  says to run it first.
- **Azure remains unreachable**, and a customer who wants it gets no answer from this
  milestone beyond "not offered". A backlog row carries C-14's three exits.
- **The image is not hardened.** No non-root user, no read-only filesystem, no seccomp profile.
  Named because a BYOC image implies a security posture it does not have, and a milestone that
  quietly implies one is worse than a line that says it does not.
- **Docker in a gate is a gate that does not always run.** `byoc.sh` joins `conformance.sh` in
  the "must be run" category, which `gate-design` calls the weakest rung. ⚠️ It therefore goes
  in **`dev/README.md`'s table, not `AGENTS.md`'s Gates table**: `scripts/build-index.py
  --check` requires that table and CI to be the same set in both directions, and CI has no
  Docker, so listing it there would fail the gate — which is exactly how `conformance.sh` is
  already handled.

## Tasks

| Id | Commit |
|---|---|
| M7g.1 | The S3 backend, the profile, the duties constant, and the refusals |
| M7g.2 | The image, and `byoc.sh` across two containers |
| M7g.3 | `deploy.md`, the ledger, and M7's close |
