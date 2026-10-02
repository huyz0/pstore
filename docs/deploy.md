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

## A read cache that survives a restart

Set `PSTORE_CACHE_DIR` to a local directory and the server reads segments through a memory
tier and a disk tier behind it ([M20](milestones/M20/SPEC.md)). A restart keeps the disk tier,
so a deploy is not a fleet-wide cold start. Unset, the server is uncached.

- **One directory per lane.** The tier lives in `<dir>/lane-<n>`, so several server processes
  on one host may share `PSTORE_CACHE_DIR`: their lanes differ, so their files do too.
- **Size it as the budget it is.** The disk tier preallocates `PSTORE_CACHE_DISK_BYTES` as files
  at start, in 4 MiB blocks, with at least two blocks per class: 24 MiB is the floor, whatever
  the variable says. Endurance throttling is not configured yet.
- **Stop it with SIGTERM or Ctrl-C.** Either flushes the disk tier's writes in flight; a
  SIGKILL loses them, and costs only those.
- **It records which store filled it.** Each start reads the bucket's store id at
  `_pstore/store-id`, creating it the first time: one GET, or one conditional PUT and a GET.
  A directory recorded for any other store, or a recreated bucket, is emptied before it serves
  anything. ⚠️ **A bucket restored from a backup keeps its old id.** Empty the cache directory
  by hand when you restore one, or the tier may serve objects the restore removed.
- **A broken disk degrades.** A directory the server cannot use leaves it memory-only, which it
  prints at start as `Bypassed`. It never refuses to serve over it.
- **Not with `PSTORE_BACKEND=memory`,** which is refused. A memory store is new at every start
  and its keys repeat, with other bytes.

### Warming before a burst

`POST /v1/indexes/{index}/warm` fetches one index's metadata into the read cache: segment
footers, delete vectors, centroid tables and dictionaries, never the vectors or postings
themselves ([M21](milestones/M21/SPEC.md), D-44). It is billed like a query and returns when
done, three sequential requests deep for any segment the server wrote. A second warm costs
one read.

- ⚠️ **It warms the process that serves it.** Nothing routes a tenant to a particular server,
  so warm every process the burst will reach: for example, once per server behind the load
  balancer, addressed directly.
- Without `PSTORE_CACHE_DIR` it is refused with `409 no_read_cache` and costs nothing.

## Replication

An index can follow another index, in the same tenant, another tenant, or another bucket
([M22](milestones/M22/SPEC.md)). `PUT /v1/indexes/{dest}/replication` with
`{"source": {"index": "src", "tenant": "71", "store": "eu"}}` creates the job; `tenant`
defaults to the caller's and `store` to this server's own bucket. `GET` reads its status,
`POST …/pause` and `…/resume` stop and restart it, `DELETE` cancels it, and
`GET /v1/replications` lists a tenant's jobs.

- **Pull-based.** A worker in each server copies what the replica lacks from its source and
  commits it to the replica's HEAD. The replica shows the source's **folded** state, so a
  source write reaches it after the source folds, then within one sync interval.
- **Read-only.** A replica refuses writes, a drop, compaction and `as_of` (`409`) until its
  replication is cancelled; it then stays as a normal index.
- **Work is found in a register** kept in this bucket. A new job is claimed by the server that
  created it; a crashed server's jobs are taken over within `TTL + SHARDS × SCAN` (about
  13 minutes at the defaults with one survivor). Claims are advisory: two servers on one job
  waste copies and never corrupt the replica.
- ⚠️ **Polling costs requests over time.** Each held tenant reads each of its distinct sources'
  HEADs once per sync interval -- one GET a minute when idle -- billed to the replica's tenant.
  Every worker also reads one register shard per `SCAN`, with or without work.
- ⚠️ **Every server must name the same sources.** A worker without the store a job names
  records `unknown_store` in that job's `last_error` and keeps the claim.
- ⚠️ **No auth**: any tenant header may name any source index in any configured store (see
  `auth` below).

A source is any S3-compatible bucket: GCS works through its S3 interoperability endpoint with
HMAC keys. It is only ever read.

## The engine registry

A server keeps one engine per tenant it has served. Since M26, `PSTORE_ENGINES` caps them: 10 000
by default, `0` for no cap. Once a second, while the registry is over the cap, the server drops
the least recently used engines that hold nothing a restart would lose, down to 90% of the cap.

What it never drops:
- an engine holding unflushed or unfolded rows, an unresolved bundle write, objects to bury, or a
  lane another process has taken;
- an engine with commits the scheduled reap has yet to collect, while `PSTORE_GC` is on: a tenant
  that commits stays for about the reap age after;
- an engine a `strong` read has asked a fold of, or one a request is using.

The cap is soft. A server whose busy tenants alone exceed it stays over, and scans at most once
a minute. `pstore_engines` and `pstore_engines_evicted_total` in `/metrics` show the size and the
churn. A dropped tenant's next request costs what a restart costs it: a HEAD read, and on its
first flush the lane probes.

## Quarantined rows

A fold refuses a row that contradicts its index's schema, and keeps going: a race between two
processes that have both never read HEAD can produce one. Since M25 the row is **set aside, not
dropped**: `GET /v1/indexes/{index}` reports it in `quarantined_rows`, and it stays until you
act on it, through GC and across restarts.

- `GET /v1/indexes/{index}/quarantine` exports every such row: the document as a client writes
  one, the reserved attributes that say how it was written, and why the schema refuses it.
  Cosine vectors come back as the stored unit vector; their magnitude is gone.
- Fix and rewrite what you want to keep, then `DELETE /v1/indexes/{index}/quarantine?through=E`,
  with `E` the `epoch` the export reported. It removes only what that export showed.
- ⚠️ **Upgrade every process before relying on it.** A process older than M25 commits HEAD
  without the quarantine, and the rows it named become unreachable.

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
| `PSTORE_CACHE_DIR` | — | Turns on the read cache (above). Refused with `PSTORE_BACKEND=memory`. |
| `PSTORE_CACHE_RAM_BYTES` | `268435456` | The memory tier, split by class: a tenth each for centroid tables and segment metadata, the rest bulk. Refused without `PSTORE_CACHE_DIR`. |
| `PSTORE_CACHE_DISK_BYTES` | `4294967296` | The disk tier, split the same way and preallocated. Refused without `PSTORE_CACHE_DIR`. |
| `PSTORE_REPLICATION` | on | Exactly `off` runs no replication worker; this server still serves the job API. |
| `PSTORE_REPLICATION_SCAN_S` | `10` | How often a worker reads one more register shard. |
| `PSTORE_REPLICATION_TTL_S` | `120` | How long a claim lasts unrenewed; renewed every third of it. |
| `PSTORE_REPLICATION_MAX` | `64` | Tenants one worker holds at most. |
| `PSTORE_REPLICATION_IDLE_S` | `60` | The sync interval of a tenant whose sources do not change. |
| `PSTORE_REPLICATION_SHARDS` | `64` | The register's shard count, **fixed when the register is first created**: about 2,000 tenants with jobs per shard. |
| `PSTORE_SOURCES` | — | Comma-separated names of remote source stores, each configured by the variables below. |
| `PSTORE_SOURCE_<NAME>_ENDPOINT` / `_BUCKET` | — | Required for each name. |
| `PSTORE_SOURCE_<NAME>_ACCESS_KEY` / `_SECRET_KEY` | the provider's credential chain | Both or neither; half a pair is refused. |
| `PSTORE_SOURCE_<NAME>_REGION` | `us-east-1` | |

## Observability

`GET /metrics` serves Prometheus text: blob requests and bytes by class, HTTP responses by
route and status, and every refusal code at zero until it happens — a series that appears only
when something breaks is a series no alert can be written against.
[`docs/research/11-design/slos.md`](research/11-design/slos.md) records which objectives are
enforced by a gate and which are blocked, and on what.
