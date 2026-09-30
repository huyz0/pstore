# Deploying pstore

What an operator must decide, and what this server will otherwise do wrong. Every section
here is something the process does **not** do for you.

⚠️ **This is a BYOC image, not a service.** It is not hardened: it runs as root, the
filesystem is writable, and there is no seccomp profile. Treat it as a process you place
behind your own perimeter.

## Measure the bucket first

`PSTORE_PROFILE` defaults to `unprobed`, and an unprobed profile **refuses to start**:
`ObjectStoreBackend::unprobed` reports both fencing primitives as `Divergent`, and a backend
that cannot fence must not accept a write it will call durable. CAS on a blob is the only
fencing mechanism in this system — there is no lock and no lease — so a bucket whose CAS is
wrong loses acknowledged writes with no error anywhere.

Run `scripts/conformance.sh` against your endpoint, read
[`docs/profiles/capability-matrix.md`](profiles/capability-matrix.md), and set
`PSTORE_PROFILE=conforming` only if `cas` and `create_if_absent` both measured `Supported`.
⚠️ Nothing re-probes at runtime. `conforming` is your word.

⚠️ **`PSTORE_BACKEND` has two values, `memory` and `s3`.** Azure and GCS are deliberately
absent. Every segment open is a suffix read, and C-14 records suffix ranges as absent on Azure
three ways; `fake-gcs-server` accepts `ifGenerationMatch` and ignores it, which is the worst
shape a precondition can have. An Azure backend is a design decision with its own spec.

## One lane per process

`PSTORE_LANE` is required and never defaulted. WAL lanes are single-writer and dense: two
processes writing one tenant on the same lane **at the same time** overwrite each other's
bundles — acknowledged, durable writes, gone, with no error. Nothing detects the collision.
Give every server process a distinct lane and keep it stable across restarts.

A restart on the same lane is safe, provided the old process has stopped (M9j). A process's
first flush to a tenant resumes the lane where it ended: it reads HEAD's watermark for the
lane, then probes forward from it, with no LIST. Those probes are the first flush's only
extra cost, 8 parallel reads.

## Unscheduled duties

⚠️ **The ids below are checked against `pstore_server::UNSCHEDULED`**, served at
`GET /v1/admin/duties`: `scripts/byoc.sh` fails if the two sets differ, so a duty added in
code cannot go undocumented. ⚠️ The *prose* under each heading is hand-written and nothing
compares it to the constant's one-line summary — this list cannot fall behind, but it can
disagree in detail.

### `fold` — a scheduled fold runs, for the tenants each server wrote

Each server folds, on its own, every tenant it wrote whose unfolded bundles reach 1 MiB or
are an hour old (M9i.1, D-39; `PSTORE_FOLD_*` below). Until a fold commits, a durable write is
visible only to the process that wrote it: queries read HEAD and the segments it names, never
a bundle, so another process simply does not see the write yet.

⚠️ **What is still yours:** a tenant whose only writer died, or restarted, keeps its unfolded
bundles until some process writes that tenant again or you call `POST /v1/admin/fold` for it.
A fold is tenant-wide, so any live writer of the tenant folds a dead writer's bundles too.

### `reap` — a scheduled reap runs, for the tenants each server committed to

Each server reaps, on its own, every tenant it committed to once that commit is an hour old
([M18](milestones/M18/SPEC.md); `PSTORE_GC_*` below). It reaps through the highest epoch it
committed that long ago, so anything a reader of the last hour could still need is kept.
⚠️ The reap is also what bounds time travel: a query before the reap horizon is refused rather
than answered wrongly, so `PSTORE_GC_AGE_S` **is** your retention policy.

⚠️ **What is still yours:** a process remembers only the commits it made since it started. A
tenant nothing commits to after a restart keeps its buried segments, billed, until something
commits there again or you call `POST /v1/admin/gc` for it.

### `tls` — the server speaks HTTP in the clear

Terminate TLS in a reverse proxy in front of this process. The server will not be given a
certificate loader; there is one transport crate in this workspace and it is confined to
`pstore-server` by `cargo deny`.

### `auth` — the tenant is whatever the header says

Authenticate and authorize before the request reaches this process. `X-Pstore-Tenant` is
taken at face value, and there is no quota: `pstore-meter` exists and is not wired. **Do not
expose this port to anyone you would not give the whole bucket to.**

## Environment

| Variable | Default | Notes |
|---|---|---|
| `PSTORE_LANE` | — | **Required.** See above. |
| `PSTORE_BIND` | `127.0.0.1:8080` | ⚠️ The image sets `0.0.0.0:8080`; the library default is loopback, which inside a container is reachable by nothing. |
| `PSTORE_BACKEND` | `memory` | `memory` or `s3`. `memory` is durable for exactly as long as the process. |
| `PSTORE_PROFILE` | `unprobed` | `unprobed` refuses to serve. |
| `PSTORE_S3_ENDPOINT` | — | e.g. `http://rustfs:9000`. |
| `PSTORE_BUCKET` | `pstore` | Must exist; the server does not create it. |
| `PSTORE_ACCESS_KEY` / `PSTORE_SECRET_KEY` | the provider's credential chain | ⚠️ **Unset is the deployed case**: an instance profile or a service-account role. Both halves or neither — half a pair is ignored. |
| `PSTORE_REGION` | `us-east-1` | |
| `PSTORE_FOLD` | on | Exactly `off` disables the scheduled fold; anything else is refused. |
| `PSTORE_FOLD_PERIOD_MS` | `1000` | How often the fold loop looks. Looking costs no request. |
| `PSTORE_FOLD_AGE_S` | `3600` | A tenant is folded once its oldest unfolded write is this old. Shorter means faster visibility to other processes, and more folds: D-39 prices it. |
| `PSTORE_FOLD_BYTES` | `1048576` | ...or once its unfolded bundles total this many bytes. |
| `PSTORE_GC` | on | Exactly `off` disables the scheduled reap; anything else is refused. |
| `PSTORE_GC_PERIOD_MS` | `1000` | How often the reap loop looks. Looking costs no request. |
| `PSTORE_GC_AGE_S` | `3600` | How long a buried object is kept, and so how far back `as_of` reaches. Below an hour, a `bounded` read's cached HEAD can name a reaped object; the read retries fresh, at the cost of a request. |

## Observability

`GET /metrics` serves Prometheus text: blob requests and bytes by class, HTTP responses by
route and status, and every refusal code at zero until it happens — a series that appears only
when something breaks is a series no alert can be written against.
[`docs/research/11-design/slos.md`](research/11-design/slos.md) records which objectives are
enforced by a gate and which are blocked, and on what.
