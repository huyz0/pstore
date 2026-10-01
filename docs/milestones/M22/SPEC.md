# M22 — Replication jobs: pull-based index replication, listed, paused, resumed, cancelled

**Serves:** OQ-135 (async replication of immutable objects), one index at a time; the copy row
of [`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md), which M16
answered within a tenant only; Design rule 12 of [`ownership-and-leases.md`](../../research/04-cluster/ownership-and-leases.md).

## What is true today

- M16's copy is one-shot and same-tenant. Nothing reads another tenant's HEAD or bucket.
- A segment's bytes name no key, tenant or index. Its sidecars are derived from its key, and
  its footer says which dictionaries it has (`Engine::warm` reads that).
- No node can find cross-tenant work without a LIST. The catalog's create-once root and
  shard registers (`pstore-catalog`) are the precedent for a sharded register.

## Delta

**HEAD** gains a trailing `replications` section, after `branched`: `dest ->
{source: {store, tenant, index}, state: running|paused, run, applied: (src_epoch,
fingerprint)?, rejected}`. `run` is the epoch that last set `running`. `fingerprint` is
FNV-1a 64 over the source index's segment keys, its delete-vector keys and its schema.
`rejected` counts rows refused (below). A HEAD with none is byte for byte M21's.

**Sync**, `Engine::replicate(sources, known)`, of all of one tenant's running replications.
It is pull-based and run by the destination.
1. Read each distinct source HEAD, in parallel. A replication whose fingerprint equals
   `known[(dest, run)]` is skipped; when all are, stop. This costs **1 read per distinct
   source**.
2. Read the dest HEAD. Keep the replications that are `running` with the expected `run`, and
   whose source index exists.
3. Copy, at most 4 segments in flight, each source segment that no dest segment maps.
   - It goes to `…/idx/{dest}/seg/R/{E:020}-{lane:016x}-{h:016x}.seg`, where `E` is the dest
     epoch read plus one and `h` is FNV-1a of the source key. The footer's expected
     dictionaries go with it. A `.cen` goes with it if present (D-10: absent means scan
     exactly).
   - A source delete vector whose key's hash `g` differs from the one the dest's vector key
     carries is copied to `{dest_seg}.{E:020}-{lane:016x}-{g:016x}.dv`, which `dv_of` still
     parses.
   - Any 404 re-reads that source's HEAD and remaps, keeping every copy. A sidecar 404 stands
     only if that fresh HEAD still names the segment.
4. Commit everything in one CAS. Each replication must still be `running` with the same
   `run`, and its `src_epoch` must be **greater** than `applied`'s, so a slower worker never
   rolls a replica back. Each one sets `indexes[dest]` to the mapped refs in source order,
   sets `schemas[dest]`, records `applied`, and buries the dest segments and vectors it
   replaced or dropped.
5. A lost CAS goes back to 2 and reuses the copies. Copies the commit does not use, or that a
   rival's commit already mapped, are buried under their own epochs. Giving up buries
   everything written (M19).

A replica shows the source's **folded** state.

**Replica rules.** While `replications` names `dest`:
- Every write into `dest`, tombstones and patches included, is refused at the door (cached
  HEAD) and at the flush. The fold drops any that slipped past a stale door and counts them in
  `rejected`. A query of `dest` ignores unfolded rows.
- Refused: `delete_index(dest)`, `compact(dest)`, a branch into `dest`, and `as_of` on `dest`,
  since copies carry `E`, not their commit epoch. Each is checked on every commit attempt's
  HEAD.
- Allowed: a branch from `dest`, by M16's rules.
- Cancel sets `branched[dest] = epoch`, so `dest`'s history starts there, and leaves a normal
  index.
- Create refuses `dest` by branch's rule: it exists, has unfolded rows or rejects, or is in
  `dropped`.

**Queue**, a new crate `pstore-jobs`: a sharded register of **tenants with runnable
replications**, over any `BlobStore`.
- A create-once `{spread}/jobs/{name}/CONFIG` fixes `S`. The default is 64, and
  `PSTORE_REPLICATION_SHARDS` applies only on first use.
- Each shard is a JSON object at `{spread}/jobs/{name}/{shard:04x}/QUEUE`, written only by CAS.
  It maps `{tenant:032x}` to `{claim: {owner, expires_ms}?}`, about 60 B per entry. A
  tenant's shard is FNV-1a(`id`) mod `S`.
- Operations: `claim(shard, owner, max)` takes entries unclaimed, expired or already this
  owner's; `renew(shard, owner)` returns what it still holds; and `reconcile(id, wanted)`.
- **`reconcile` is the only path that adds or removes an entry.** It reads the shard, then the
  dest HEAD, then CASes the shard on the tag it read, with an entry iff the HEAD has a
  running replication. A lost CAS repeats.
  - A HEAD change that lands after the shard read changes the shard's tag before its own
    reconcile writes, so the last reconcile to commit read the latest HEAD. No grace period
    and no clock are involved.
  - Create, pause, resume and cancel each CAS HEAD and then reconcile. A crash in between is a
    failed request: the client retries, and every control call is idempotent and
    reconciles.
- **Claims are advisory (Design rule 12).** Each commit is a CAS on a HEAD that states
  `running`, `run` and `applied`, so two holders waste copies and never corrupt. A pause is
  effective when its CAS lands.

**Worker** in `pstore-server`, beside fold and reap, owned by this process's lane.
- At start it reads every shard in one parallel round, reclaiming this lane's claims.
- Then it reads one shard per `scan` (10 s), in a rotating order offset by the lane, claiming
  up to `max` tenants (64).
- It renews each held shard every `ttl/3` (TTL 120 s), and drops a tenant it lost or that has
  no running replication (and reconciles it).
- Per held tenant it runs a sync every `period` (1 s), doubling up to `idle` (60 s) while
  nothing changes.
- Status notes go to `{spread}/tnt/{t}/REPLSTATUS`: `dest -> {last_ok_ms, last_error ≤256 B}`,
  PUT on change at most every `ttl/3`.
- Env: `PSTORE_REPLICATION=off`, `_SCAN_S`, `_TTL_S`, `_MAX`, `_IDLE_S`.

**Remote sources** are read-only S3-compatible stores (GCS through its S3 interoperability
endpoint). A source named `n` is configured by `PSTORE_SOURCE_<N>_ENDPOINT`, `_BUCKET`,
`_ACCESS_KEY`, `_SECRET_KEY` and `_REGION`, all listed in `PSTORE_SOURCES`. Every worker must
configure the same sources; one without `n` notes `unknown_store` for that replication.

**API**, tenant header as usual. A job is `(tenant, dest)`, so there is one per dest index,
and a source may feed many.
- `PUT /v1/indexes/{dest}/replication` with `{"source": {"index", "tenant"?, "store"?}}` →
  `201`.
- `GET …/replication` → state, source, applied epoch, segments, rows, rejected, `queued`,
  claim owner and expiry, last success, and last error.
- `POST …/replication/pause` and `…/resume`, both idempotent → `200`.
- `DELETE …/replication` → `200 {epoch}`.
- `GET /v1/replications` → the tenant's jobs, after one HEAD read.

Refusals, by name:
- `400 bad_request`: a name outside the pattern; the source is the dest itself; an unknown
  store.
- `409 index_exists`, `409 replication_exists`, `404 source_not_found`,
  `404 replication_not_found`.
- `409 replica_read_only`: a write or branch into a replica.
- `409 replication_active`: dropping a replica.
- `409 replica_no_history`: `as_of` on a replica.

**Docs.** A correction banner on `ownership-and-leases.md` § Work scheduling: opt-in
cross-tenant work is not derivable from any manifest a node holds, so M22 adds a register of
it. It is not a broker: it has no address, no master and no liveness protocol, and claims are
advisory. `deploy.md` and the parity row are updated too.

**Does not change:** any query of a non-replica, the write path's requests, and the fold, GC
and branch of non-replicas. There is no LIST anywhere.

## Acceptance criteria

1. **Follows.** For a same-tenant, cross-tenant and remote source: after source writes,
   deletes, compaction, a drop-and-recreate, a drop-and-re-branch of its parent, and a
   borrowed segment, then a source fold and a sync, every query kind on `dest` equals the
   source's, deleted rows included.
2. **Incremental.** One new source segment copies exactly it and its expected sidecars. A
   delete in one segment copies one vector and no segment. With no change, a sync costs **1**
   read per distinct source.
3. **GC-safe.** Source compaction then `gc(0)` on both tenants: dest answers unchanged, and
   the replaced dest segments are buried and reaped. A source segment reaped mid-copy
   remaps without losing copies. A branch from `dest`, then a sync that drops a shared
   segment, then `gc(0)`: the branch answers unchanged. A lost CAS reuses copies; a rival's
   duplicate and an abandoned sync's copies are buried.
4. **No regress.** A commit from a source HEAD older than `applied` is refused
   (interference hook).
5. **Read-only.** Each replica refusal holds. A row past a stale door is not served and is
   counted in `rejected`. After cancel, writes succeed and `as_of` below the cancel is `404`.
6. **Pause fences.** A sync racing a pause does not commit, and resume continues from
   `applied`. Cancel likewise.
7. **Queue.** `claim` takes unclaimed, expired and own entries, honours `max`, and skips live
   foreign ones. `renew` drops a lost entry. `CONFIG` is created once, and its `S` is
   honoured.
8. **Reconcile.** Across every interleaving of control steps (HEAD CAS, shard read, HEAD read,
   shard CAS) for two controllers and a worker (bounded, exhaustive), the shard ends with an
   entry iff HEAD has a running replication.
9. **Workers.** Three workers and ten tenants under paused tokio time: all converge, and a
   tenant is held by one live worker after one `ttl`. A killed worker's tenants are reclaimed
   within `ttl + S·scan`.
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
| 6 | engine, `a_pause_fences_a_racing_sync` | commit without the state or `run` check |
| 7 | `jobs/tests/queue.rs` | expiry `<` as `<=`; `max` ignored; foreign claim taken |
| 8 | `jobs/tests/reconcile.rs` | HEAD read before the shard read; an unconditional shard write |
| 9 | server, `workers_converge`, `a_dead_workers_tenants_move` | no takeover; renew skipped |
| 10 | server, one per endpoint | each code and field |
| 11 | server, `worker_steady_state_cost` | a read per replication instead of per source; every shard scanned |

## RA budget

- **Sync:** idle costs 1 GET per distinct source. Otherwise the source reads come first, then
  1 dest HEAD GET, then per new segment 1 GET and ≤3 sidecar GETs followed by their PUTs,
  then per changed vector a GET and a PUT, then 1 CAS. Depth 5, in the background.
- **Control:** create is 1 source GET, 1 HEAD GET plus CAS, and reconcile (2 GETs plus 1
  CAS). Pause, resume and cancel are a HEAD GET plus CAS, then reconcile. Status is 3 GETs
  in parallel. List is 1 GET. No LIST.
- **Worker:** per worker, one GET per `scan` even with no jobs, which is 1,000 GET/s across
  10,000 nodes. It scales with nodes.
- **⚠️ Polling is per tenant per source per time**, which breaks "never per elapsed time"
  on purpose: the user asked for pull-based replication. It is opt-in and billed to the dest
  tenant, at ≤1,440 reads a day per idle source at `idle` = 60 s. Dest commits are at most 1
  CAS/s per tenant, below the per-key ceiling.

## Risks

- **Takeover** after a crash is bounded by `ttl + S·scan`, about 13 min at the defaults with
  one survivor. A larger `S` needs a longer `scan` or more workers.
- **Ceiling:** 64 shards × about 2,000 tenants is about 120 KB per shard. More needs a larger
  `S` chosen at first use, because there is no resharding.
- **A source HEAD that regresses** (disaster recovery) stops its replicas at the `>` check.
  Recovery is cancel, then create again.
- **No auth exists**, so any tenant header may name any source. That is today's posture for
  every read, recorded for the auth duty.
- **Same-tenant replicas copy bytes** that M16 could share, so the dest's GC never depends on
  the source's.

## Tasks

- **M22.1** — `pstore-jobs`: register, claims and reconcile, with tests 7 and 8.
- **M22.2** — engine: `replications`, `replicate` and the replica rules, with tests 1–6.
- **M22.3** — server: the worker, remote sources, the API and docs, with tests 9–11.
