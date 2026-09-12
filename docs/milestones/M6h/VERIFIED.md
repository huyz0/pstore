# M6h — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. Command: `cargo test -p pstore-catalog --test repartition`.

1. **Every bucket owns what it holds** —
   `every_bucket_owns_what_it_holds_after_a_repartition`, checked against `bucket_of` at the
   current width for every record in every bucket. Observed red by inverting the ownership
   predicate, which empties each bucket into its sibling.
2. **No tenant is lost or duplicated** — `a_repartition_loses_no_tenant`, the census before and
   after.
3. ⚠️ **A stale-width writer's record is moved, not dropped** —
   `a_stale_width_writers_record_is_moved_not_dropped`, and it is the criterion that makes this
   a re-partition. Observed red by not recording the arrival, which turns the operation into a
   prune and **deletes the only copy of a live record**.
   ⚠️ **The first fixture asserted nothing.** It hardcoded tenant 9,999, and `h % 2w` equals
   `h % w` for half of all tenants — that one did not move, so the test would have passed over
   a prune. It now searches for a tenant the doubling actually moves.
4. **An older copy does not come back** — `an_older_copy_does_not_come_back`: an older-epoch
   copy in the wrong bucket and a tombstone in the right one, and the tenant stays deleted.
   Observed red by concatenating instead of merging, which resurrects the superseded record —
   and since a tombstone is only a newer record, that undoes a delete.
5. **Idempotent** — `a_second_repartition_moves_nothing`: the second run returns 0 and every
   bucket's contents are byte-for-byte what they were.
6. **The split's duplicates are reclaimed** — `a_repartition_reclaims_the_splits_duplicates`:
   the total records held falls to the tenant count, having first asserted it was larger.
   ⚠️ This and criterion 3 pull opposite ways and both hold: deleting everything a bucket does
   not own satisfies 6 and breaks 3.
7. **Zero LIST** — `repartitioning_does_not_list`.
8. **A backend that cannot fence is refused** —
   `a_repartition_on_a_divergent_backend_is_refused` asserts the error **and zero writes**, and
   `repartition` is added to `refusal.rs`'s existing loop over every guarded door.
9. **Gates** — `./scripts/gates.sh` green. Mutation over `repartition` and its rewrite helper,
   in the `dev` container: **18 mutants, 14 caught, 4 missed** — the four are the engine's
   stale-commit test helper, one in `pstore-node` and two in `pstore-testkit`, all of which
   predate this milestone and survive every sweep in this session. Nothing in `pstore-catalog`
   survives.
   ⚠️ **Read carefully, because I read it wrong once.** `cargo mutants` prints only the
   **missed** mutants by file, so grepping its output for a crate and finding nothing means
   nothing was *missed* — not that nothing was tested. Confirming a sweep covered what it was
   meant to needs `cargo mutants --list` with the same filter, which is how the false finding
   recorded against [M6g](../M6g/VERIFIED.md) was caught.

⚠️ **Two passes, and one mutation showed why.** Collapsing them into a single pass fails four
of the eight tests: a bucket can both gain and shed, so there is no ordering of single writes
that never leaves a record in neither place — a cycle of gains and sheds has none at all.

## What M6g got wrong about this, and it is worth recording

M6g called this blocked, needing "a bound on how long a stale-width process may live". Most of
that was already gone when it wrote it:

- ⚠️ **The stale-width reader was already refused** by M6g's own closing census check.
- ⚠️ **Nothing looks a tenant up by derived key.** `bucket_of` has exactly **two** shipped
  callers — the split's predicate and the appender — so every read is a full census and a
  record in the "wrong" bucket is still found.

What was left is the writer, and it is not a lifetime bound: a stale-width appender's copy may
be the **only** one or the **newest**, so a record may leave a bucket only when its owner holds
a copy at least as new. That is a rule about *content*, checkable from the census this
operation already does — not a rule about time, which nothing could have provided.

## What is not built, and named rather than omitted

- **Atomicity.** A crash between the passes leaves the duplication this operation starts from,
  so a re-run finishes it. ⚠️ Safe **only** because every read is a census; adding a per-tenant
  read path would invalidate that argument, and this is the place that would have to change.
- ⚠️ **A bucket emptied of records keeps its head object.** After shrinking that is dead
  weight, and reclaiming it needs a rule about when a derived key may stop existing — a head
  that 404s already means "empty bucket", so deleting one is not obviously safe.
- **A schedule.** Nothing calls `repartition`, `split`, `fold`, `reap` or `sweep`. The caller
  is whoever composes a serving stack, and nothing does.
- **Concurrency limits.** On a freshly split deployment this rewrites every bucket: `width`
  runs and `width` head CASes in one operation, and nothing bounds it against concurrent
  folds. A loser retries; a deployment folding hard could starve it.
