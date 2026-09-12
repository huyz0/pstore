# M6h — Re-partitioning a catalog that has drifted

**Serves:** backlog item 17, which [M6g](../M6g/VERIFIED.md) opened: *"pruning the old buckets
is the only step that makes anyone wrong … until it happens a split doubles the deployment's
bucket objects and reclaims nothing."*

**Depends on** [M6g](../M6g/SPEC.md)'s split, and on its census check — which turns out to have
removed most of the blocker M6g recorded.

## ⚠️ The blocker was smaller than M6g thought, and the reason is worth stating

M6g deferred pruning because it would make an old-width **reader** wrong and a stale-width
**writer** lose a record. Both need re-examining:

- **The reader is already refused.** M6g's own closing check makes `enumerate` reject a census
  gathered at a width the deployment has left. There is no other read path: `bucket_of` has
  exactly **two** shipped callers — the split's partition predicate and `Appender::record` —
  so nothing looks a tenant up by derived key. Every read is a full census over `0..width`.
- **The writer is not lost, but it is the real constraint.** A stale-width appender writes
  tenant `T` into `b` when the current width says `b + w`. A census reads every bucket, so the
  record is found. ⚠️ But it means **a record in the "wrong" bucket may be the only copy, or
  the newest one** — so a prune that drops everything a bucket does not own by the current
  width can delete a live record.

That second point is what makes this a re-partition rather than a prune.

> **A record may be removed from a bucket only when the bucket that owns it holds a copy at
> least as new.** Otherwise it is *moved*, not dropped.

Establishing that needs the owner's contents, which needs a census — so the operation reads
every bucket first and then rewrites, and "prune the split's leftovers" and "put a stale
writer's record where it belongs" are the same operation.

## Delta

**`pstore-catalog`**
- `repartition(store) -> Result<usize, CatalogError>` — reads every bucket at the current
  width and rewrites the ones that have drifted. Returns how many records moved.
- ⚠️ **Two passes, because one cannot be ordered.** A bucket can both gain and shed, so there
  is no order of single writes that never leaves a record in neither place — a cycle of
  gains and sheds has no valid ordering at all. So: **pass one writes every bucket that
  gains**, with its own records *and* the arrivals; **pass two writes every bucket that
  sheds**, with only what it owns. Between the passes a record is in two buckets, which is the
  state this operation starts from and which a census already tolerates.
  ⚠️ A crash between passes leaves exactly that duplication — the input state — so a re-run
  finishes it. Both passes skip buckets with nothing to do, so after a split, where the
  siblings already hold their arrivals, pass one is a no-op.
- ⚠️ **Run and pending together.** A record recorded and not yet folded is as much a tenant as
  one in a run, so a drifted bucket is rewritten as a fresh run holding everything it owns,
  with pending emptied — which is what `publish` does, and leaving pending alone would strand
  a moved record in the head of a bucket that no longer owns it.
- ⚠️ **Merged by the one rule.** Where a record appears in two buckets, `merge`'s
  newest-epoch-wins decides which survives, exactly as a census already does.
- A **guarded door**: `require_fencing` first, like `fold`, `reap`, `sweep` and `split`.

**Does not add** — ⚠️ **atomicity.** A crash mid-way leaves some buckets re-partitioned and
some not, which is the state this operation exists to clean up, so a re-run finishes it. That
is only safe because a census reads every bucket; it is not a general licence. **A schedule** —
nothing calls it. **Bucket deletion**: a bucket emptied of records keeps its head object, and
reclaiming those is a different question about key liveness.

## Acceptance criteria

1. **After a split and a re-partition, every bucket owns exactly what it holds.** For every
   bucket `b` and every record in it, `bucket_of(tenant, width) == b`.
2. **No tenant is lost or duplicated** — the census before and after is the same set.
3. ⚠️ **A stale-width writer's record is moved, not dropped.** Write through an appender built
   at the old width after a split, re-partition, and the record is still enumerable **and** now
   lives in the bucket the current width names. This is the criterion that makes it a
   re-partition rather than a prune, and a prune would delete the record.
4. ⚠️ **A record whose owner holds a newer copy is dropped, not resurrected.** Two copies at
   different epochs, one in the wrong bucket: the newer survives and the older does not come
   back. Otherwise re-partitioning would undo a tombstone.
5. **It is idempotent** — a second run moves nothing and changes no head.
6. **The old buckets shrink.** After a split, the sum of records held across all buckets falls
   to the number of tenants; before it is larger, because the moved records are in two places.
7. **Zero LIST**, and the request count is a function of the width, never of the tenant count.
8. ⚠️ **A backend that cannot fence is refused**, and moves nothing.
9. Region coverage ≥95% on `pstore-catalog`, mutation ≥80% on the changed module, gates green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `every_bucket_owns_what_it_holds_after_a_repartition` | the ownership predicate inverted, which empties every bucket into its sibling |
| 2 | `a_repartition_loses_no_tenant` | a record dropped from a non-owner without being written to its owner — the window this operation's ordering exists to close |
| 3 | `a_stale_width_writers_record_is_moved_not_dropped` | pruning instead of re-partitioning, which deletes the only copy of a live record |
| 4 | `an_older_copy_does_not_come_back` | the merge dropped or inverted, which resurrects a superseded record and can undo a tombstone |
| 5 | `a_second_repartition_moves_nothing` | writes issued unconditionally, which makes a no-op operation a CAS contender |
| 6 | `a_repartition_reclaims_the_splits_duplicates` | the drop half omitted, which leaves the duplication a split creates in place forever |
| 7 | `repartitioning_does_not_list` | a LIST introduced to find buckets |
| 8 | `a_catalog_write_on_a_divergent_backend_is_refused`, with `repartition` added | `require_fencing` after the writes |

⚠️ Criteria 3 and 6 pull in opposite directions and both must hold: 6 alone is satisfied by
deleting everything a bucket does not own, which is exactly what 3 forbids.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `repartition` | ≤ `width` runs + `width` head CASes | **3** — heads, runs, then the writes | `width` wide per round | 0 |
| Everything else | unchanged | unchanged | unchanged | 0 |

⚠️ A function of the **width**, never of the tenant count — the same shape as a census and as
the split.

## Risks

- ⚠️ **It rewrites every bucket that has drifted**, so on a freshly split deployment that is
  every bucket: `width` runs and `width` head CASes in one operation. A background job, and
  nothing bounds how long it holds those CASes against concurrent folds — a loser simply
  retries, but a deployment folding hard could starve it.
- **Not atomic**, and safe only because a census reads every bucket. If a per-tenant read path
  is ever added, that safety argument goes with it — which is worth knowing before adding one.
- ⚠️ **A bucket emptied of records keeps its head object.** After shrinking, that is dead
  weight; reclaiming it needs a rule about when a derived key may stop existing, and a head
  that 404s is already meaningful ("empty bucket"), so deleting one is not obviously safe.

## Tasks

| Id | Commit |
|---|---|
| **M6h.1** | `repartition`, moving before dropping |
