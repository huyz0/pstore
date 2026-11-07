# M202 verification

Implemented in (tests and docs only, no shipping code):
- `crates/pstore-engine/tests/it/recovery.rs`: node loss.
- `crates/pstore-engine/tests/it/replicate.rs`: bucket loss with a replica in another bucket.
- `docs/deploy.md` "Recovery objectives", `docs/research/11-design/slos.md` "Recovery
  objectives", and DELIVERY row H.

All runs were in a 4-core cloud container on the in-memory store, so every count of requests
is this build's and every duration would be `provisional`. No duration is claimed.

How to run:
- `cargo test -p pstore-engine --test it recovery::`
- `cargo test -p pstore-engine --test it replicate::a_lost_store_leaves_the_replica_its_last_sync`

**Red first.** These tests pin behaviour that already exists, so they pass on the parent. Each
was turned red by a mutation of the code it covers, applied and reverted by script
(.harness/m202-mutations.log):
- a fold that reads only its own lane turns `a_lost_node_loses_no_durable_write` red: B never
  sees A's unfolded durable row;
- a cancel that keeps the replication turns `a_lost_store_leaves_the_replica_its_last_sync`
  red: the recovered index refuses its first write.

**Suite:** 2,740 passed and 0 failed, with the two IPv6 tests skipped because the container has
no IPv6 (.harness/m202-suite.log); as M201 recorded, those two fail alone only with `Address
family not supported by protocol`. fmt and clippy pass.

**Coverage:** lines 97.32%, regions 96.00%, functions 97.06% (.harness/m202-coverage.log).

**Acceptance criteria.**
1. PASS: `a_lost_node_loses_no_durable_write`. After A is dropped with no shutdown, B (another
   lane, never having served the tenant) answers exactly the 11 durable rows after its fold,
   and none of the 5 never flushed: the batched RPO is those 5. B issued no LIST. B's two scans
   and its fold cost 21 reads and 2 writes on this build (printed by the test).
2. PASS: the same test. B's scan before its fold answers the 10 folded rows, and the unfolded
   durable row appears after B's fold.
3. PASS: `a_lost_store_leaves_the_replica_its_last_sync`. With the source's bucket failing every
   object read, the sync names `dst` failed and commits nothing, the replica's refs are
   unchanged and it answers the 30 synced rows; the 10 rows folded after that sync are the
   RPO. Cancelling costs exactly 1 HEAD read and 1 conditional write, and the index then takes
   a write and answers 31 rows.
4. PASS: `scripts/check-slos.py` (every new row enforced by a test that exists, or blocked
   with its blocker named), `scripts/check-links.sh`.
5. PASS for `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the suite and
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95`, run here.
   `scripts/mutants.sh` is pending CI's shards; the diff adds no shipping code.

Review (spec, tests and docs, one round by a read-only subagent): FAIL on two majors, both
taken. The batched RPO in `docs/deploy.md` claimed 8 MiB as a bound, but `reserve_buffer`'s
flush bound is soft and racing writes can stack to `tenant_buffered_bytes` (64 MiB); the docs
now say so. This file did not yet exist. Minors taken: AC 3 asserts the sync failed on the
source, the cancel's cost is stated uncontended, and the replication worker's takeover time is
named in the replica's RPO. Not covered by a test: a node lost together with the replication
worker's takeover, which the docs state as a formula.
