# M6a — The sharded catalog: derived buckets, one parallel round, zero LIST

**Serves:** **Q9** ([catalog-without-master](../../research/03-metadata-consistency/catalog-without-master.md))
and the §8 revision in [tenancy-scale-model](../../research/10-benchmarks-cost/tenancy-scale-model.md)
making the catalog **tenant-scoped**. No new id space; **OQ-8** is touched and left open.
**Depends on** [M2](../M2/SPEC.md)'s HEAD — a record describes a tenant HEAD epoch, which is
what makes the catalog derived rather than authoritative.

## ⚠️ Why this is its own milestone, and the failure it exists to prevent

The roadmap's M6 is three things sharing a subject and nothing else: the catalog, per-tenant
quotas and metering (a `pstore-blob` decorator with no catalog in it — **M6b**, Design rule 13,
[load-and-hotspots](../../research/04-cluster/load-and-hotspots.md) § "Isolation between
tenants"), and a synthetic 1M-index workload, which needs the catalog to exist first and lands
here as criterion 4's fixture. Split on M5's evidence: fusion rode along with sparse in the
plan and came back from spec review with four blocking findings.

A catalog is the natural home of a LIST, and a LIST here would be invisible — enumeration is a
cold admin path, so nothing a user waits on gets slower, every functional test passes, and the
bill arrives a month later. C1 prices a LIST like a PUT, caps it at 1000 keys, and makes it
serial: 50M objects is 50,000 serial LISTs. The second failure is why the criteria are
**counted rather than timed**: a catalog whose request count scales with tenants rather than
buckets is correct, is fast at test scale, and does not survive 1M tenants.

## ⚠️ C-12 — the change log is unnecessary, and its absence is what keeps the root cold

`catalog-without-master.md` §2 gives each bucket a per-lane change log
(`{h}/cat/b/{bucket}/log/{lane}/{seq}`, "lanes again") drained by a folder, because "an index
creation must not rewrite a whole catalog bucket". The premise is right; a separate object
space is not the only way to honour it. Pending records carried inside the bucket's own CAS'd
pointer bound the write by `MAX_PENDING` rather than by occupancy, and delete a lane id space,
sequence allocation, forward probing, and a probe window.

**The trade, stated plainly, because it is not free.** The log makes an append one small
unconditional PUT; the pointer makes it a read plus a CAS of `pending`. What the log costs in
exchange is paid on *every enumeration, forever*: a reader that cannot know which lanes hold
pending entries must probe `LOG_LANES × window` derived keys per bucket — 524,288 requests at
`DEFAULT_WIDTH`, four lanes and a window of eight, against 16,384. Bytes on a
tenant-lifecycle write are the cheap side; requests on the enumeration path are the expensive
one, and **criterion 14 measures that byte cost** rather than asserting it away.

The reuse would not have carried anyway: the engine's lanes are **node**-scoped, so their id
space is unbounded and a reader cannot learn which exist without a registry object — a second
mutable object per bucket and a third sequential round in every enumeration.

What it costs is a CAS on the write path, which lanes exist to avoid. Affordable here and
nowhere else, because a catalog append is a **tenant-lifecycle** event and not a commit —
criterion 7 is what holds that true rather than an instruction saying so.

Recorded as **C-12** on `catalog-without-master.md` and on `key-layout.md`, whose key tree
carries the log keys and whose census bills the per-bucket pointer at fold rate, not append
rate.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| `TenantRecord` | **~1.2 KB** | 16-byte id, 8-byte epoch, a state byte, and up to 50 names at ~24 bytes. ⚠️ **Not** tenancy-scale-model §8's ~10 KB, which sizes a record carrying per-index *config*; this one carries names, and every byte figure below is derived from 1.2 KB. |
| `DEFAULT_WIDTH` | **16,384** buckets | tenancy-scale-model §8. At 1M tenants that is ~61 records per bucket, so a run is **~73 KB**. Carried **in the root**, which supplies `DEFAULT_WIDTH` when it has never been written; a reader takes it as a parameter and never reads the constant itself. |
| `MAX_PENDING` | **8** records | ~10 KB of pending against a ~73 KB run — the append is bounded by the cap, not by occupancy, which is the whole of C-12's cheap side. Bounded **structurally**: the append that would exceed it folds inline, so a folder that never runs is a cost problem and never a correctness one. |
| Enumeration depth | **2** given the width, **3** from a cold root | `catalog-without-master.md` promises one parallel round after the root. The heads are that round; the runs are the second, because a run key is not derivable until its head is read. |
| `cat/root` CAS rate | **width change only** | Matches `key-layout.md`'s mutable-object census exactly. Per-bucket pointers are the ~1/min registers, `width` of them, which is the corpus's own answer to root contention. |
| Catalog requests on a tenant open | **0** | The hot path has no catalog in it — enforced by there being no dependency edge to it (criterion 12). |

## Delta

**Adds**
- **`pstore-catalog`** (layer 3), depending on `pstore-blob` and `pstore-types` only. Not on
  `pstore-engine`: a catalog that *could* read HEAD would be tempted to, and this is derived
  state that must be rebuildable without the engine.
- `TenantRecord { tenant, epoch, indexes: Vec<String>, state: Live | Deleted }` — a tenant as of
  one of its HEAD epochs. `epoch` is what orders two records for one tenant without a clock.
- `Root { epoch, width }` at `cat/root`, CAS'd. Read to learn the width, and **absent means
  never widened** — the reader gets `DEFAULT_WIDTH` and the request that found nothing still
  counts against the cold-enumeration depth. Nothing in this milestone writes it.
- `BucketHead { run_epoch, digest, pending: Vec<TenantRecord> }` at `{bucket:04x}/cat/b/HEAD`,
  CAS'd — `width` independent registers, one per bucket.
- `{bucket:04x}/cat/b/{run_epoch:020}-{digest:016x}` — the immutable sorted run. ⚠️ **The
  digest in the key is load-bearing.** Two folders reading the same head but different
  `pending` — an append landed between them — produce different runs at the same `run_epoch`;
  an epoch-only key lets the CAS loser's PUT land second and leaves the head pointing at a run
  missing a record that is no longer pending either. Different content, different key, and the
  PUT is conditional on absence.
- `Appender::observe(tenant, epoch, &[String]) -> bool` — records only when this tenant's index
  set or liveness changed since this appender last recorded it. Returns whether it wrote.
- `fold(store, bucket, width)` — drains `pending` into a new immutable run and CASes the head.
  Optimistic and leaderless; a loser rebases onto the winner and refolds.
- `enumerate(store, width)` / `enumerate_since(store, width, prior)`.

**Does not add** — **bucket splitting.** The root carries `width` and every reader takes it as
a parameter, so the mechanism a split needs is in place; the split needs a protocol keeping an
old-width reader *stale rather than wrong*, which is its own set of criteria. OQ-8 stays open. Also not added: **run reaping** (a superseded run is garbage and stays;
reaping without a retention window is how a reader mid-enumeration loses the run it is
reading); **billing aggregation**, **quotas**, **metering** (all M6b); **any wiring into the
commit path** — nothing calls `observe` yet, which is what criterion 12 asserts and why; **any
HTTP or admin surface**.

## Acceptance criteria

1. A bucket is a pure function of `(tenant_id, width)`, and hashing defeats structured ids:
   100,000 ids in each of three families — sequential, high-64-bits-only, stride 65,536 — at
   width 256 leave the busiest bucket below **1.5×** the mean.
2. An uncontended `observe` that changes something costs **1 read + 1 conditional write**, and
   **no** request in the whole lifecycle — observe, fold, enumerate — is an `OpClass::List`.
3. Enumeration's sequential depth is **2** given the width and **3** from a cold root, on the
   depth-counting store.
4. Enumeration's request count is a function of **width, not tenants**: at width 16, a
   deployment of 100 tenants and one of 2,000 issue **identical** counts. The test asserts both
   fixtures occupy all 16 buckets, so `r` is 16 in both and the claim is not luck.
5. `enumerate_since` re-reads only the runs whose `run_epoch` moved: after a fold touching 1 of
   16 buckets, exactly **1** run object is read.
6. A record that has been recorded but not folded is enumerated, and after folding it is
   enumerated **once**.
7. `observe` over an unchanged index set writes **nothing**: 100 calls after the first cost
   exactly the first one's requests.
8. Two appenders racing on one bucket both land — under the barrier store, not a hopeful
   `spawn`.
9. Two folders racing on one bucket lose no record **when an append lands between their head
   reads**, so the two produce different runs: one CAS wins, the loser rebases, and every
   recorded record is in the run that results. Two folders drained from an identical head write
   an identical run and could not observe this.
10. `pending.len() ≤ MAX_PENDING` after **any** sequence of appends: the append that would
    exceed it folds inline first.
11. The newest `epoch` wins for a tenant, and a `Deleted` tombstone hides it from enumeration
    without being dropped from the run — a tombstone dropped at fold time un-deletes the tenant
    at the next enumeration.
12. Nothing depends on `pstore-catalog`: `grep -l pstore-catalog crates/*/Cargo.toml` names only
    its own manifest, so no hot path can reach it.
13. A read refused for any reason **other than absence** surfaces as an error: enumeration over
    a store that refuses one read returns `Err`, never a shorter set. Derived head keys 404 by
    design, so the branch that swallows absence is the one that would swallow a throttle.
14. C-12's cheap side, measured rather than asserted: in a bucket holding 64 folded records, an
    `observe` moves **under a quarter** of the bytes a `fold` of that bucket moves.
15. Region coverage ≥95% on `pstore-catalog`, mutation ≥80%, full gate set green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `bucket_is_derived_and_hashing_defeats_structured_ids` | truncation instead of a hash — `id as u32 % width` is perfectly uniform over sequential ids and puts a whole high-bits id family in bucket 0 |
| 2 | `an_observe_costs_one_read_and_one_write`, `the_whole_lifecycle_issues_no_list` | read-modify-write turned into two reads; a `list_unrestricted` anywhere in `fold` or `enumerate` |
| 3 | `enumeration_is_two_rounds_deep` | heads awaited before runs in a loop — same answer, depth `1 + width` |
| 4 | `enumeration_requests_do_not_scale_with_tenants` | a per-record request anywhere in the read path |
| 5 | `incremental_enumeration_skips_unchanged_buckets` | the `run_epoch` comparison inverted or dropped |
| 6 | `a_pending_record_is_visible`, `a_folded_record_appears_once` | reading only the run (invisible until folded); reading run and pending without letting pending win |
| 7 | `an_unchanged_index_set_writes_nothing` | the change check dropped, which turns a lifecycle-rate append into a commit-rate one |
| 8 | `racing_appenders_both_land` | `put` for `put_conditional`; a rebase that overwrites instead of re-adding |
| 9 | `racing_folders_lose_no_record` | the loser publishing its own run over the winner's, dropping what the winner had drained |
| 10 | `pending_is_bounded_by_an_inline_fold` | the cap checked after the push, or not at all |
| 11 | `the_newest_epoch_wins`, `a_tombstone_hides_a_tenant_without_being_dropped` | `>` for `>=` on the epoch merge; the tombstone filtered at fold time rather than at read time |
| 13 | `a_refused_read_is_an_error_not_a_shorter_answer` | `Err(_) => None` where only `NotFound` should be, which is how a throttled enumeration under-reports and returns `Ok` |
| 14 | `an_observe_moves_fewer_bytes_than_a_fold` | `MAX_PENDING` raised to the point where the pointer costs what the run costs, which is C-12's argument quietly deleted |

⚠️ Criterion 4's fixture is the OQ-72 workload in miniature — 2,000 tenants × 50 index names —
so what it supports at 1M is **arithmetic on a measured invariant**, and the ledger says so.

## RA budget

`width` = buckets, `r` = buckets holding a run. Enumeration and folding are **cold paths** —
the ≤3 budget is for user-facing paths — but only `fold` uses that licence.

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `observe`, nothing changed | 0 | **0** | 0 | 0 |
| `observe`, uncontended change | **1** | 2 | 1 | 0 |
| `observe` that overflows `MAX_PENDING` | 2 | 4 | 2 | 0 |
| Full enumeration, width in hand | 0 | **2** | `width` + `r` | 0 |
| Full enumeration, cold | 0 | **3** | 1 + `width` + `r` | 0 |
| Incremental, `c` runs moved | 0 | 2 | `width` + `c` | 0 |
| Fold one bucket | 2 | 4 | 2 | 0 |
| **Open / write / query a tenant** | — | **unchanged** | **unchanged** | **0** |

## Risks

- **A creation burst into one bucket meets the per-key CAS ceiling** (~5 writes/s,
  [manifest-and-cas](../../research/03-metadata-consistency/manifest-and-cas.md)). Lifecycle
  rates make this remote — 1M tenants created over a year is 0.03/s across 16,384 buckets — but
  a bulk import is exactly the shape that concentrates. It degrades to retry, never to loss,
  and **creation is already effective via HEAD**, so a contended append delays enumeration and
  nothing else. Creations/s into one bucket is the number to watch; it is not measured here.
- **The 1M number is not measured.** Criterion 4 measures the invariant at 2,000 tenants; the
  rest is arithmetic. A cost genuinely superlinear only above 2,000 would not be caught here,
  and nothing in this repository could catch it.
- **`observe`'s change check is per appender instance**, so the rate is lifecycle events ×
  appender instances: a fleet-wide restart re-records every tenant each surviving appender
  touches once. Harmless — the merge is idempotent by `(tenant, epoch)` — but it is the same
  concentration shape as the bulk import above, arriving at the same registers.
- **Splitting is unbuilt and `width` is a parameter**, so the shape invites someone to pass a
  different number and expect it to work. It reassigns every tenant, and nothing refuses it.
  `{bucket:04x}` is fixed-width up to 65,536 buckets; past that the split changes the key
  format as well as the assignment.

## Tasks

| Id | Commit |
|---|---|
| **M6a.1** | `pstore-catalog`: the record, the root, derived keys, and the bucket pointer that carries pending changes |
| **M6a.2** | `observe` — one CAS, nothing written when nothing changed, and the inline fold that bounds the pointer |
| **M6a.3** | `fold` — an immutable run, leaderless, losing nothing to a race |
| **M6a.4** | `enumerate` / `enumerate_since` — two rounds, no LIST, and a count that does not scale with tenants |
