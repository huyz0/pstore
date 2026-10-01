# M22 — Replication jobs: pull-based index replication, listed, paused, resumed, cancelled

**Serves:** OQ-135 (async replication of immutable objects), for one index at a time; the
copy row of [`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md),
which M16 answered only within a tenant; Design rule 12 of
[`ownership-and-leases.md`](../../research/04-cluster/ownership-and-leases.md).

## What is true today

- `copy_from_namespace` (M16) is one-shot and same-tenant: `dest` borrows `src`'s keys, and
  nothing follows `src` afterwards. No path reads another tenant's HEAD or another bucket.
- A segment's bytes name no key, tenant or index. Its sidecars are derived from its key
  (`.cen`, `.sdict`, `.tdict`). A delete vector's row count only grows while its segment lives.
- Nodes find work in state they already hold. Nothing finds work across tenants without a
  LIST, and the server does not use the catalog.

## Delta

**HEAD.** A trailing `replicas` section, after `branched`: `dest -> Replica { source:
{store, tenant, index}, state: running|paused, run: u64, applied: Option<(src_epoch,
fingerprint)> }`. `fingerprint` hashes the source index's keys, delete counts and schema. A
HEAD with no replica is byte for byte what M21 wrote.

**Sync, `Engine::replicate(source, dest, run, known)`.** Pull-based, run by the destination.
1. Read the source tenant's HEAD from `source`, the local store or a named remote one. If
   the source index's fingerprint equals `known`, stop: **1 request**.
2. Read the dest HEAD. Stop unless `replicas[dest]` is `running` with this `run`. If the
   source index does not exist, stop with an error.
3. Copy each source segment that has no dest segment yet, with its sidecars that exist, to
   `…/idx/{dest}/seg/R/{E:020}-{lane:016x}-{h:016x}.seg`. `E` is the dest epoch read plus
   one, and `h` is FNV-1a 64 of the source key. A dest segment's `h` names its source, so
   no map is stored. Copy each delete vector whose source count exceeds the dest's, to
   `dv_key(dest_seg, E, lane)`.
4. Commit by CAS: `indexes[dest]` becomes the mapped source refs in source order;
   `schemas[dest]` becomes the source schema; and `applied` is recorded. Dest segments and
   vectors the source no longer names are buried. A lost CAS re-reads the dest HEAD and
   retries steps 2–4, reusing every copy. A sync that gives up buries what it wrote (M19).

A replica's state is the source's **folded** state: unfolded source rows arrive after the
source folds.

**Replica rules.** While `replicas` names `dest`:
- Writes, patches, deletes and a branch into `dest` are refused. The write door's check
  uses the cached HEAD, and the fold counts rows that slipped past a stale door as rejects.
- `delete_index(dest)` and `compact(dest)` are refused. `as_of` on `dest` is refused,
  because copy keys carry `E`, not their commit epoch.
- Cancel writes `branched[dest] = epoch`, so `dest`'s history starts there (M16's rule). It
  is a normal index afterwards.

**Queue, new crate `pstore-jobs`.** A sharded job queue over any `BlobStore`.
- A create-once `{spread}/jobs/{name}/CONFIG` fixes the shard count `S`. Its default is
  64, set by `PSTORE_REPLICATION_SHARDS` before first use; resharding is not supported.
- Each shard is one JSON object, `{spread}/jobs/{name}/{shard:04x}/QUEUE`, written only by
  CAS: `id -> Entry { run, enqueued_ms, claim: Option<{owner, expires_ms}>, note:
  {last_ok_ms, last_error} }`. A job's shard is FNV-1a(`id`) mod `S`.
- Operations, each one CAS loop on one shard: `enqueue`, which upserts, and replaces only
  a different `run`; `dequeue(id, run)`, which leaves another run's entry alone; `claim`
  (owner, max), which takes entries unclaimed, expired or already this owner's; `renew`
  (owner, notes), which drops entries removed, re-run or taken; and `read`.
- **Only runnable jobs are queued.** Invariant: a `running` replica has an entry with its
  `run`. Create and resume enqueue, then CAS HEAD, then enqueue again. Pause and cancel CAS
  HEAD first, then `dequeue(id, run)`. A worker dequeues an entry whose HEAD replica is
  gone, paused or re-run, but only if the entry is older than `grace` (300 s).
- **Claims are advisory (Design rule 12).** Two workers syncing one job waste copies and
  never corrupt: each commit is a CAS on a HEAD that states `running` and `run`. A pause is
  therefore effective when its CAS lands, whoever holds the claim.

**Worker** in `pstore-server`, beside the fold and reap loops, owned by this process's lane:
- At start, one parallel read of every shard, reclaiming this lane's claims.
- Then, every `scan` (10 s), it reads one random shard and claims up to `max_jobs` (64).
- It renews each shard it holds a claim in every `ttl/3` (TTL 120 s), and syncs each held
  job, at most 4 at a time.
- A job's interval is `period` (1 s), doubling while unchanged up to `idle` (60 s).
- On create, the serving process claims the job when under `max_jobs`.
- Env: `PSTORE_REPLICATION=off`, `_SCAN_S`, `_TTL_S`, `_MAX_JOBS`, `_IDLE_S`.

**Remote sources** are read-only S3-compatible stores (GCS via its S3 interoperability
endpoint). A source named `n` is configured by `PSTORE_SOURCE_<N>_ENDPOINT`, `_BUCKET`,
`_ACCESS_KEY`, `_SECRET_KEY` and `_REGION`, all listed in `PSTORE_SOURCES=n,…`.

**API**, tenant header as usual. `{dest}` names the job, so one job per dest index, and a
source may feed many.
- `PUT /v1/indexes/{dest}/replication` with `{"source": {"index", "tenant"?, "store"?}}`
  → `201`.
- `GET /v1/indexes/{dest}/replication` → status: state, source, applied source epoch,
  segments, rows, claim owner and expiry, last success, last error.
- `POST …/replication/pause` and `…/resume` → `200`, both idempotent. Resume also restores
  a missing entry.
- `DELETE …/replication` → `200 {epoch}`.
- `GET /v1/replications` → the tenant's jobs, from HEAD alone.

Refusals, by name:
- `400 bad_request`: a name outside the pattern; the source is the dest itself; an unknown
  store.
- `409 index_exists`, `409 replication_exists`, and `404 source_not_found`.
- `404 replication_not_found`.
- `409 replica_read_only`: a write, delete or branch into a replica.
- `409 replication_active`: dropping a replica.
- `409 replica_no_history`: `as_of` on a replica.

**Does not change:** queries of any index, a replica's included; folds, GC and branches of
non-replicas; the write path's requests. Nothing routes; there is no LIST on any path.

## Acceptance criteria

1. **Follows.** Same-tenant, cross-tenant and remote-store: after any source write, delete,
   compaction, drop-and-recreate or branch-borrowed segment, plus a source fold and one sync,
   every query kind on `dest` equals it on the source, deleted rows included.
2. **Incremental.** A sync after one new source segment copies exactly that segment and its
   existing sidecars. A sync after deletes in one segment copies one vector and no segment.
   With no change, a sync costs **1** read and nothing else.
3. **GC-safe.** Segments the source compacted away are buried in the dest, and `gc(0)` reaps
   them. Source GC does not break the dest. A lost dest CAS reuses its copies, and an
   abandoned sync leaves nothing unburied.
4. **Read-only.** Each replica refusal above holds, including a row past a stale door, which
   is counted as a reject. After cancel, writes succeed and `as_of` below the cancel is `404`.
5. **Pause fences.** A sync whose commit races a pause (interference hook) does not commit,
   and none commits after pause returns. Resume continues from `applied`. Cancel likewise.
6. **Queue.** `enqueue`, `dequeue(run)`, `claim`, expiry takeover, own-lane reclaim and
   `renew` dropping lost entries each hold. Two workers on one shard, with injected CAS
   contention, claim each entry once. `CONFIG` is created once and its `S` honoured.
7. **Invariant.** Across every interleaving of create, pause, resume and cancel steps (a
   bounded exhaustive test), a `running` replica ends with an entry of its `run`. A paused
   or cancelled one ends without one, or with one the grace cleanup removes.
8. **Workers.** Three workers with ten jobs: every job converges. Each job is held by at
   most one live worker after claims settle. A killed worker's jobs move within `ttl` plus
   `S·scan/N`.
9. **API.** Each endpoint's success and refusal codes. Status fields match HEAD and queue.
   List returns every job after one HEAD read.
10. **Cost.** Steady state per worker: one shard read per `scan`, one CAS per held shard per
    `ttl/3`, and one read per job per interval, by the request counter.
11. **Gates.** `./scripts/mutants.sh` over the diff misses 0; `./scripts/gates.sh` green.

## Test plan

| # | File, test that fails first | Mutation it catches |
|---|---|---|
| 1 | `engine/tests/replicate.rs` — `follows_*` per source kind | a source dv read by plain key; schema not copied; refs out of source order |
| 2 | same — `a_sync_copies_only_what_changed`, `an_idle_sync_reads_once` | known fingerprint ignored; sidecars copied unconditionally; dv count compared `>=` |
| 3 | same — `compacted_sources_are_buried`, `a_lost_cas_reuses_copies` | removed refs not buried; copies re-made per attempt; abandoned copies leaked |
| 4 | same, and `server/tests/replication.rs` — `replica_*_refused` | each refusal dropped; the fold folding a replica's rows |
| 5 | engine — `a_pause_fences_a_racing_sync` | commit without checking state or `run` |
| 6 | `jobs/tests/queue.rs` | expiry `<` as `<=`; claim ignoring max; dequeue ignoring run; renew keeping lost |
| 7 | `jobs/tests/interleavings.rs` | resume without the trailing enqueue; cleanup without grace |
| 8 | `server/tests/replication.rs` — `workers_converge`, `a_dead_workers_jobs_move` | no expiry takeover; renew skipped |
| 9 | server — one test per endpoint | each status code and field |
| 10 | server — `worker_steady_state_cost` | a HEAD read per idle job per tick; scan of every shard |

## RA budget

- **Sync:** 1 GET when unchanged. Otherwise 2 GETs (both HEADs), then in parallel per new
  segment 4 GETs (the segment and three sidecars, 404s included) and up to 4 PUTs, and a GET
  and PUT per changed vector, then 1 CAS. Depth 4. Background work, never a user path.
- **Control:** create is 1 source read, 2 shard CAS and 1 HEAD CAS. Pause is 1 HEAD CAS and
  1 shard CAS. Status is 2 GETs in parallel. List is 1 GET. No LIST anywhere.
- **⚠️ Polling is per job per time.** This is the "never scales with elapsed time per index"
  rule, broken on purpose: the user asked for pull-based replication. The cost is bounded,
  opt-in and billed to the dest tenant: an idle job is 1,440 reads a day at `idle` = 60 s.

## Risks

- **Clock skew** only delays a claim's takeover, by Design rule 12. The CAS keeps data correct.
- **A lost queue entry** stalls a running job until a resume, without data loss. Status
  shows `claimed_by: null`, and resume repairs it.
- **Memory:** a segment is copied whole (GET then PUT), so a worker holds up to 4 segments.
- **Same-tenant replicas copy bytes** that M16 could share. That is accepted for one code
  path; the dest owns its keys, so the source's GC never reaches them.
- **No auth exists**, so any tenant header can replicate any source. That is today's posture
  for every read, recorded so the auth duty covers it.
- **Shard size:** about 150 bytes per entry, so 64 shards hold about 100k jobs at about
  240 KB each. More needs a larger `S` at first use.

## Tasks

- **M22.1** — `pstore-jobs`: the queue, with tests 6 and 7.
- **M22.2** — engine: `replicas`, `replicate`, and the replica rules, with tests 1–5.
- **M22.3** — server: the worker, remote sources and the API, with tests 8–10, plus docs.
