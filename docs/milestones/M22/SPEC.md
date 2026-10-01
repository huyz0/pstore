# M22 — Replication jobs: pull-based index replication, listed, paused, resumed, cancelled

**Serves:** OQ-135 (async replication of immutable objects), one index at a time; the copy row
of [`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md), which M16
answered within a tenant only; Design rule 12 of [`ownership-and-leases.md`](../../research/04-cluster/ownership-and-leases.md).

## What is true today
- M16's copy is one-shot and same-tenant. Nothing reads another tenant's HEAD or bucket.
- Segment bytes name no key, tenant or index; sidecars derive from the key; the footer names
  the dictionaries. `pstore-catalog`'s create-once root and registers are the precedent here.

## Delta

**HEAD** gains a trailing `replications` section, after `branched`: `dest ->
{source: {store, tenant, index}, state: running|paused, run, applied: (src_epoch,
fingerprint)?, rejected}`. `run` is the epoch that last set `running`. `fingerprint` is
FNV-1a 64 over the source index's segment keys, its delete-vector keys and its schema.
`rejected` counts rows refused (below). A HEAD with none is byte for byte M21's.

**Sync**, `Engine::replicate(sources, known)`, of all of one tenant's running replications.
It is pull-based and run by the destination.
1. Read each distinct source HEAD, in parallel. A replication whose fingerprint equals
   `known[(dest, run)]` is skipped; when all are, stop: **1 read per distinct source**.
2. Read the dest HEAD. Keep the replications `running` in **this** HEAD whose source index
   exists. One whose source fingerprint equals `applied`'s is current: seed `known` and keep
   it out of the commit.
3. Copy, at most 4 segments in flight, each source segment that no dest segment maps.
   - To `…/idx/{dest}/seg/R/{E:020}-{lane:016x}-{h:016x}.seg`: `E` the dest epoch read plus
     one, `h` FNV-1a of the source key. The footer's dictionaries go with it, and a `.cen`
     if present (D-10: absent means scan exactly).
   - A source delete vector whose key's hash `g` differs from the one the dest's vector key
     carries is copied to `{dest_seg}.{E:020}-{lane:016x}-{g:016x}.dv`, which `dv_of` still
     parses.
   - Any 404 re-reads that source's HEAD and remaps, keeping every copy. A sidecar 404 stands
     only if that fresh HEAD still names the segment.
4. Commit the rest in one CAS. **Per replication**, it is included only if it is still
   `running` and its `src_epoch` is **greater** than `applied`'s, so a slower worker never
   rolls it back. Each included one sets `indexes[dest]` to the mapped refs in source order,
   sets `schemas[dest]`, records `applied`, and buries the dest segments and vectors it
   replaced or dropped, including a vector whose source segment no longer has one.
5. A lost CAS goes back to 2 and reuses the copies. **A replication that fails** is left out
   and its copies are buried, while the rest commit. Failures include a source read error, an
   unknown store, or 24 consecutive remaps that copied nothing new. Copies unused, or mapped by a rival's
   commit, are buried under their own epochs (M19).

A replica shows the source's **folded** state.
**Replica rules.** While `replications` names `dest`:
- Every write into `dest`, tombstones and patches included, is refused at the door (cached
  HEAD) and at the flush. The fold drops any that slipped past a stale door and counts them in
  `rejected`. A query of `dest` ignores unfolded rows.
- Refused: `delete_index(dest)`, `compact(dest)`, a branch into `dest`, and `as_of` on `dest`,
  since copies carry `E`, not their commit epoch. Each is checked on every commit attempt's
  HEAD.
- Allowed: a branch from `dest`, by M16's rules.
- Cancel sets `branched[dest] = epoch` (history starts there) and leaves a normal index.
- Create refuses `dest` by branch's rule: exists, unfolded rows or rejects, or `dropped`.

**Queue**, new crate `pstore-jobs`: a sharded register of **tenants with runnable
replications**, over any `BlobStore`. A create-once `{spread}/jobs/{name}/CONFIG` fixes `S`
(default 64; `PSTORE_REPLICATION_SHARDS` applies only on first use).
- Each shard is a JSON object at `{spread}/jobs/{name}/{shard:04x}/QUEUE`, written only by CAS.
  It maps `{tenant:032x}` to `{gen, claim: {owner, expires_ms}?}`, about 70 B per entry.
  `gen` is FNV-1a of HEAD's running `(dest, run)` set, and a holder that sees it change
  re-reads HEAD. A tenant's shard is FNV-1a(`id`) mod `S`.
- Operations: `claim(shard, owner, max)` takes entries unclaimed, expired or already this
  owner's; `renew(shard, owner)` returns what it still holds; and `reconcile(id, wanted)`.
- **`reconcile` is the only path that adds or removes an entry.** It reads the shard, then the
  dest HEAD, then CASes the shard on the tag it read. There is an entry iff HEAD has a
  running replication, carrying HEAD's `gen` and keeping its claim. A new entry is claimed
  for the caller only if its worker runs and is under `max`. A lost CAS repeats. `claim` and `renew` never add
  an entry, and keep `gen`.
  - A HEAD change after the shard read changes its tag before its own reconcile writes, so
    the last reconcile to commit read the latest HEAD. No grace period, no clock.
  - Create, pause, resume and cancel each CAS HEAD and then reconcile. A crash in between
    leaves the entry stale until the next control call or **status**, which reconciles
    whenever `queued` disagrees with HEAD.
- **Claims are advisory (Design rule 12).** Each commit is a CAS on a HEAD that states
  `running`, `run` and `applied`, so two holders waste copies and never corrupt. A pause is
  effective when its CAS lands.

**Worker** in `pstore-server`, beside fold and reap, owned by this process's lane.
- At start, one parallel read of every shard reclaims its lane's claims.
- Then one shard per `scan` (10 s), rotating, offset by the lane, claiming up to `max` (64).
- It renews each held shard every `ttl/3` (TTL 120 s); a tenant lost, or with nothing
  running (then reconciled), is dropped.
- Per held tenant it runs a sync every `period` (1 s), doubling up to `idle` (60 s) while
  nothing changes or a replication keeps failing. At most 4 segments in flight per worker.
- Notes go to `{spread}/tnt/{t}/REPLSTATUS`, `dest -> {last_commit_ms, last_error ≤256 B}`,
  PUT only after a commit or a changed error, at most once per `ttl/3`, never once HEAD
  shows none. Deleted with the last replication; a racing worker may leave one orphan.
- Env: `PSTORE_REPLICATION=off`, `_SCAN_S`, `_TTL_S`, `_MAX`, `_IDLE_S`.

**Remote sources** are read-only S3-compatible stores (GCS through its S3 interoperability
endpoint). A source named `n` is configured by `PSTORE_SOURCE_<N>_ENDPOINT`, `_BUCKET`,
`_ACCESS_KEY`, `_SECRET_KEY` and `_REGION`, all listed in `PSTORE_SOURCES`. Every worker must
configure the same sources; one without `n` notes `unknown_store` for that replication.

**API**, tenant header as usual. A job is `(tenant, dest)`; a source may feed many.
- `PUT /v1/indexes/{dest}/replication` with `{"source": {"index", "tenant"?, "store"?}}` →
  `201`.
- `GET …/replication` (reconciles a stale entry, so it may write) → state, source, applied epoch, segments, rows, rejected, `queued`,
  claim owner and expiry, last success, and last error.
- `POST …/replication/pause` and `…/resume`, both idempotent → `200`.
- `DELETE …/replication` → `200 {epoch}`.
- `GET /v1/replications` → the tenant's jobs, after one HEAD read.

Refusals: `400 bad_request` (name, self-source, unknown store), `409 index_exists`,
`409 replication_exists`, `404 source_not_found`, `404 replication_not_found`,
`409 replica_read_only` (write or branch into), `409 replication_active` (drop),
`409 replica_no_history` (`as_of`).

**Docs.** A correction banner on `ownership-and-leases.md` § Work scheduling: opt-in
cross-tenant work derives from no manifest a node holds, so M22 adds a register; not a
broker (no address, master or liveness), claims advisory. Plus `deploy.md`, the parity row.

**Unchanged:** non-replica queries, folds, GC and branches; write-path requests. No LIST.

## Acceptance criteria

1. **Follows.** Same-tenant, cross-tenant and remote sources: after writes, deletes,
   compaction, drop-and-recreate, re-branch of its parent, and a borrowed segment, then a
   source fold and a sync, every query kind on `dest` equals the source's.
2. **Incremental.** One new source segment copies exactly it and its expected sidecars. A
   delete in one segment copies one vector and no segment. With no change, a sync costs **1**
   read per distinct source.
3. **GC-safe.** Source compaction then `gc(0)` on both tenants: dest answers unchanged, and
   the replaced dest segments are buried and reaped. A re-branch from a parent with no deletes
   buries the dest's vector. A source segment reaped mid-copy
   remaps without losing copies. A branch from `dest`, then a sync that drops a shared
   segment, then `gc(0)`: the branch answers unchanged. A lost CAS reuses copies; a rival's
   duplicate and an abandoned sync's copies are buried.
4. **No regress.** A commit from a source HEAD older than `applied` is refused
   (interference hook).
5. **Read-only.** Each replica refusal holds. A row past a stale door is not served and is
   counted in `rejected`. After cancel, writes succeed and `as_of` below the cancel is `404`.
6. **Pause fences.** A sync racing a pause does not commit. After a resume with an idle
   source, the first sync commits nothing (one dest HEAD read more), the next costs criterion 2's. Cancel likewise.
   One failing replication does not stop its sibling's commit. A replication created on a
   held tenant syncs within `ttl/3 + period`.
7. **Queue.** `claim` takes unclaimed, expired and own entries, honours `max`, and skips live
   foreign ones. `renew` drops a lost entry. `CONFIG` is created once, and its `S` is
   honoured.
8. **Reconcile.** Across every interleaving of control steps (HEAD CAS, shard read, HEAD read,
   shard CAS) for two controllers and a worker (bounded, exhaustive), the shard ends with an
   entry iff HEAD has a running replication.
9. **Workers.** Three workers and ten tenants under paused tokio time: all converge, and a
   tenant is held by one live worker after one `ttl`. A killed worker's tenants are reclaimed
   within `ttl + S·scan`. A new tenant is claimed by its creator at once.
10. **API.** Each endpoint's success and refusal codes. Status matches HEAD, the shard and
    the notes. List costs one read.
11. **Cost.** By the request counter, a worker's steady state is one shard read per `scan`,
    one CAS per held shard per `ttl/3`, and criterion 2's reads per tenant per interval.
12. **Gates.** `./scripts/mutants.sh` over the diff misses 0; `./scripts/gates.sh` is green.

## Test plan

| # | Test that fails first | Mutation it catches |
|---|---|---|
| 1 | `engine/tests/replicate.rs` `follows_*` | dv by plain key; schema not copied; order lost; dv decided by count |
| 2 | same, `copies_only_what_changed`, `idle_sync_reads_once` | `known` ignored; dictionaries unconditional |
| 3 | same, `gc_*`, `branch_from_replica_survives`, `lost_cas_reuses_copies` | no burial; recopy per attempt; leak |
| 4 | same, `an_older_source_never_commits` | the `>` check dropped or made `>=` |
| 5 | same and `server/tests/replication.rs`, `replica_*_refused` | each refusal dropped; the fold keeping rows |
| 6 | engine, `a_pause_fences_a_racing_sync`, `resume_after_idle`, `one_failure_isolated`; server, `a_new_replication_on_a_held_tenant` | no state check; `>` all-or-nothing; `gen` unobserved |
| 7 | `jobs/tests/queue.rs` | expiry `<` as `<=`; `max` ignored; foreign claim taken |
| 8 | `jobs/tests/reconcile.rs` | HEAD read before the shard read; an unconditional shard write; renew re-adding |
| 9 | server, `workers_converge`, `a_dead_workers_tenants_move` | no takeover; renew skipped |
| 10 | server, one per endpoint | each code and field |
| 11 | server, `worker_steady_state_cost` | a read per replication instead of per source; every shard scanned |

## RA budget

- **Sync:** idle costs 1 GET per distinct source. Otherwise the source reads come first, then
  1 dest HEAD GET, then per new segment 1 GET and ≤3 sidecar GETs followed by their PUTs,
  then per changed vector a GET and a PUT, then 1 CAS. Depth 5, in the background.
- **Control:** create = source GET, HEAD GET+CAS, reconcile (2 GETs+CAS); pause, resume,
  cancel = HEAD GET+CAS, reconcile. Status 3 parallel GETs; list 1 GET. No LIST.
- **Worker:** one GET per `scan` even with no jobs (1,000 GET/s across 10,000 nodes: nodes).
  One `REPLSTATUS` PUT per tenant per `ttl/3` at most, and only while commits or errors
  change: zero when idle.
- **⚠️ Polling is per tenant per source per time**, which breaks "never per elapsed time"
  on purpose: the user asked for pull-based replication. It is opt-in and billed to the dest
  tenant, at ≤1,440 reads a day per idle source at `idle` = 60 s. Dest commits are at most 1
  CAS/s per tenant, below the per-key ceiling.

## Risks

- **Takeover** after a crash: ≤ `ttl + S·scan`, ~13 min at defaults with one survivor.
- **Ceiling:** 64 shards × ~2,000 tenants ≈ 140 KB a shard; beyond, choose `S` at first use.
- **A stale HEAD cache** on an eventual read may serve a row that slipped a stale door.
- **A regressed source HEAD** (disaster recovery) stops at the `>` check: cancel, recreate.
- **No auth exists**: any tenant header may name any source, as for every read today.
- **Same-tenant replicas copy bytes** M16 could share, so dest GC never depends on source GC.

## Tasks

- **M22.1** — `pstore-jobs`: register, claims, reconcile (tests 7, 8).
- **M22.2** — engine: `replications`, `replicate`, replica rules (tests 1–6).
- **M22.3** — server: worker, remote sources, API, docs (tests 9–11).
