# M202: recovery point and recovery time, measured under node loss and store loss

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "DR release
requires explicit RPO/RTO measured with node/store loss, not only a successful copy call.
Replication following source deletions is not an independent recovery point." It is the last
piece of H's export and restore item still pending in [DELIVERY](../../comparisons/DELIVERY.md).

## What holds today
- **A node owns nothing.** A `durable` or `applied` write is acknowledged after its bundle is
  in the blob store, and any process can fold any tenant (`Engine::fold`), so a lost node's
  durable writes are reachable by every other node. A `batched` write is acknowledged from
  this process's memory. It reaches the store when the tenant's unflushed rows pass 8 MiB or
  when a durable write comes (M141), and it is lost if the process dies first.
- **The store is the only durable tier.** Losing it loses everything in it. The two
  independent recovery points are an index replica in another bucket (M22, pull-based, read
  only until cancelled, then a normal index) and an export (M167, read-only, one epoch).
- A replica shows its source's folded state: a source write reaches it after the source
  folds (up to `PSTORE_FOLD_AGE_S`, default 3600 s), then within one sync interval
  (`PSTORE_REPLICATION_IDLE_S`, default 60 s, when the source is idle). M192's bound applies
  only to a source in the replica's own bucket, which is no protection against losing that
  bucket.
- Nothing states an RPO or RTO, and no test loses a node or a store and counts what is left.

## Delta
- **Tests that lose a node and a store** and count what survives and what recovery costs, in
  writes and in requests (below). They run on the in-memory store, so any wall-clock time they
  print is `provisional`. Counts are not.
- **`docs/deploy.md` gains "Recovery objectives"**, stating for each loss what is kept, what
  may be lost, and what recovery is:
  - **Node loss.** RPO is 0 for `durable` and `applied` writes. For `batched` writes it is
    that process's unflushed rows: usually up to `tenant_flush_bytes` (8 MiB a tenant by
    default, M141), a soft bound that writes racing a flush can pass, and never more than
    `tenant_buffered_bytes` (64 MiB). RTO is the next request on any node. A durable write that was not yet folded becomes visible to other nodes at the next
    fold, which any node runs.
  - **Store loss, with a replica in another bucket.** RPO is the source writes the replica's
    last sync did not reach: those not folded at the source, plus at most one sync interval.
    At the defaults that is up to about `PSTORE_FOLD_AGE_S + PSTORE_REPLICATION_IDLE_S`. The
    replica keeps answering throughout. RTO is a cancel of its replication (one HEAD read and
    one conditional write, uncontended), after which it takes writes, plus pointing clients at
    its bucket. If the server holding the replication is lost too, the job's takeover
    (`TTL + SHARDS × SCAN`, about 13 minutes) adds to the RPO.
  - **Store loss, with an export.** RPO is everything after the export's epoch. RTO is a copy
    of the export into a writable index, whose requests scale with its bytes (M167).
  - **Store loss with neither:** everything is lost. A replica in the same bucket, or one
    that follows source deletions, is not a recovery point for it.
- **`docs/research/11-design/slos.md` gains "Recovery objectives"**: each count above is
  `enforced` by the test that measures it. RPO and RTO as durations are `blocked`, named on
  a real fleet's fold and sync settings and its backend's latency, because the in-memory
  store measures neither.

Not changed: the engine and the server. If a test finds a defect, it is fixed here and named
in VERIFIED.

## Acceptance criteria
1. **Node loss** (engine, `recovery::a_lost_node_loses_no_durable_write`): node A (lane 1)
   writes 10 rows durably (each write flushed) and folds, writes 1 more row durably and does
   not fold, then writes 5 rows it never flushes. A is dropped with no shutdown, flush or fold.
   Node B (lane 2, the same store), never having served the tenant, scans, folds and scans
   again. After its fold it answers exactly the 11 durable rows. The 5 unflushed rows are
   absent: that count is the batched RPO. B issues no LIST, and the requests it costs are
   recorded in VERIFIED as B's recovery cost.
2. **A durable write not yet folded is not lost** (engine, same test): B's scan before its
   fold answers the 10 folded rows, and the one unfolded durable row appears at B's fold. This
   is the "visible at the next fold" clause.
3. **Store loss with a replica** (engine,
   `replicate::a_lost_store_leaves_the_replica_its_last_sync`): a source in store A, a replica
   in store B (`Kind::Remote`). 30 rows are written, folded and synced. Then 10 more rows are
   written and folded at the source but not synced. Store A then fails every read. A sync
   fails and changes nothing in B, and the replica answers exactly the first 30 rows: the RPO
   is those 10. Cancelling the replication costs exactly 1 HEAD read and 1 conditional write
   in B. The index then takes a write and answers 31 rows, with store A still failing.
4. **Docs:** the deploy section and the SLO rows exist. `scripts/check-slos.py` passes with
   every new row `enforced` by a named test that exists, or `blocked` with a blocker named.
5. **Gates:** fmt, clippy, suite, coverage 95/95, check-slos, check-links, check-verified.
   Mutants run on CI. The diff adds no shipping code unless a defect is found.

## Test plan
| AC | Test | What it pins |
|---|---|---|
| 1, 2 | engine `recovery::a_lost_node_loses_no_durable_write` | durable writes survive a node with no shutdown; batched loss is the unflushed rows only; no LIST in takeover |
| 3 | engine `replicate::a_lost_store_leaves_the_replica_its_last_sync` | a failed sync changes nothing; the replica holds its last sync; a cancel is one commit and makes it writable |

**Red first.** These tests pin behaviour that already exists, so they pass on the parent. Each
is shown to test something by a mutation of the code it covers that turns it red, applied by
script and recorded in VERIFIED: a fold that reads only its own lane (AC 1 and 2), and a
cancel that keeps the replication (AC 3).

## Review
Round 1 (a read-only subagent given the spec, tests and docs): FAIL, two majors, both taken.
The batched RPO is now the soft `tenant_flush_bytes` bound and the hard
`tenant_buffered_bytes` one, as `reserve_buffer` says; and VERIFIED.md records the recovery
cost. Minors taken: AC 3 asserts the sync failed on the source, the cancel is said to be
uncontended, and the replication worker's takeover is named in the RPO. Not covered, and said
so: a node and its bucket lost together is the "neither" row.

## RA budget
None. Tests and docs only.

## Risks
- The numbers are counts on one host. The durations a customer will ask for need a real
  fleet's settings and backend, which is why they are `blocked` rows rather than guesses.
- A replica's RPO depends on the source folding. A source tenant that stops folding (its
  nodes gone too) leaves unfolded durable writes in store A, which a replica never sees.

## Out of scope
Automatic failover or promotion, a lag bound for a remote-bucket replica (its credentials are
read-only), and time-based RPO/RTO targets.
