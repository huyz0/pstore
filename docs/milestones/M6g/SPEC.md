# M6g — Splitting a catalog bucket, stale rather than wrong

**Serves:** backlog item 10b and **OQ-8**, left open by [M6a](../M6a/VERIFIED.md): *"the split
needs a protocol keeping an old-width reader **stale rather than wrong**, and `{bucket:04x}`
stops at 65,536 buckets, so it is a key-format change too."*

**Depends on** [M6d](../M6d/SPEC.md), which removed the blocker underneath: `read_root`
returned no tag, so `write_root`'s conditional arm had no caller and the width was **write-once**.

## ⚠️ Doubling is the only split, and that is what makes it tractable

`bucket_of` is `hash(tenant) % width`. For a doubling, `h % 2w` is either `h % w` or
`h % w + w` — so bucket `b` splits into exactly `b` and `b + w`, and **no record ever moves
between two old buckets**. A split is therefore `w` independent partitions, not a reshuffle.

⚠️ **The key format is not blocking for the first two doublings.** `{bucket:04x}` holds
65,536; the default is 16,384, so 32,768 and 65,536 are reachable with no format change at all.
The fifth digit is a real change and this milestone stops short of it, refusing rather than
widening past `MAX_WIDTH` — which `Width::new` already does.

## ⚠️ Pruning is the only step that can make a reader wrong

Walk the states. `w` is the old width, `b` an old bucket, `b + w` its new sibling.

| after | old-width reader (`0..w`) | new-width reader (`0..2w`) |
|---|---|---|
| new buckets written, root not flipped | complete — old buckets untouched | nobody has the new width yet |
| root flipped, old buckets not pruned | **complete** — `b` still holds every record | complete; `b` and `b + w` both hold the moved records, and `merge` dedupes by tenant |
| old buckets pruned | ⚠️ **wrong** — the moved tenants are gone from `b` | complete |

So the split itself is safe in both directions, and **it is pruning that breaks an old reader**.
This milestone writes and flips; it does not prune.

⚠️ **The same argument covers a stale-width WRITER**, which is the worse case: an appender
holding `w` writes tenant `T` to `b` when the world says `b + w`. Unpruned, a new-width reader
reads `0..2w`, which includes `b`, so the record is found. Pruned, it is lost — a lost write,
not a stale read. Nothing here bounds how long a stale writer may live, which is exactly why
pruning is not built.

## ⚠️ And a reader must not be silently short

Between reading the root and finishing the enumeration, the width can change. Today
`enumerate` takes a width as an argument and would happily return a partial answer.

> **`enumerate` reads the root when it finishes and refuses if the width is not the one it
> gathered at.**

⚠️ **Amended twice.** The first draft compared only the width and I judged it would wrongly
refuse a deliberately-behind reader; the second added an opening read for a baseline, which
cost a fourth round trip **to learn what the caller had already told it** — the caller read the
root to know the width it passed. The width comparison is right after all, and the "stale
reader" objection was confused: the criterion below is about the **buckets** still holding
every record, which is what makes such a reader recoverable. `enumerate` refusing to hand back
a census it cannot vouch for is a different thing, and it is also what pruning will need.

One extra GET on a path that already costs `width + runs` requests, and it turns "a census
that is quietly short" into an error the caller retries. ⚠️ That is the whole of "stale rather
than wrong": the data is still there, and pruning — the one step that would remove it — is
deferred.

## Delta

**`pstore-catalog`**
- `split(store) -> Result<Width, CatalogError>` — doubles the deployment's width. For each old
  bucket it partitions the run and pending records, writes the new sibling's head and run, and
  then CASes the root. Returns the new width.
- ⚠️ **Refused past `MAX_WIDTH`**, and refused when the root moved under it — a split racing a
  split must not interleave two partitionings.
- `enumerate` and `enumerate_since` read the root at the **end** and return
  `CatalogError::WidthMoved { enumerated, current }` if it is not the width they gathered at.
  One extra GET, no baseline: the caller already read the root to learn the width it passed.
- ⚠️ **`enumerate` merges across buckets, not only within them.** `merge` was applied per
  bucket and the results concatenated, so one tenant appearing in two buckets appeared
  **twice** in the census — which is exactly the unpruned post-split state, and was already
  possible for any record written under a stale width. This spec's first draft claimed the
  deduplication already happened. It did not, and `every_tenant_survives_a_split` said so.
- A **guarded door**: `require_fencing` first, like `fold`, `reap` and `sweep`.

**Does not add** — ⚠️ **pruning the old buckets.** It is the only step that makes a reader or
writer wrong, and doing it safely needs a bound on how long a stale-width process may live,
which nothing provides. Until then a split **doubles the deployment's bucket objects** and
leaves the old contents in place: correct, and not free. **Shrinking.** **Anything past 65,536
buckets** — that is the fifth hex digit and a different key space. **Automatic splitting** —
nothing measures bucket occupancy, and nothing calls this.

## Acceptance criteria

1. **Every tenant survives a split.** A catalog of several hundred tenants enumerates to the
   same set before and after, at the new width.
2. ⚠️ **A record lands where the new width says.** After the split, each tenant's record is in
   `bucket_of(tenant, 2w)` — asserted by reading that bucket directly, not by enumerating,
   which would pass if everything stayed put.
3. ⚠️ **The old buckets still hold every record.** After a split, reading buckets `0..w`
   **directly** finds every tenant — which is what makes an old-width reader complete rather
   than short, and it is exactly the property pruning would break.
   ⚠️ Asserted against the buckets rather than through `enumerate`, because criterion 4 makes
   `enumerate` refuse once the root has moved: the two criteria are about different things and
   the first draft of this spec had them contradicting each other.
4. ⚠️ **A census at a width the deployment has left is refused, not shortened.** Both shapes of
   the same failure: a split landing mid-pass, and a caller already behind when it started.
5. ⚠️ **A tenant in two buckets is one tenant.** The unpruned post-split state puts a moved
   record in `b` and `b + w` both, and a census must not count it twice.
6. **A stale-width writer's record is still found.** Append through an appender built at `w`
   after a split, then enumerate at `2w`: the record is there.
7. **Splitting twice reaches 65,536 and then refuses.** From 16,384: 32,768, 65,536, then an
   error rather than a fifth hex digit.
8. ⚠️ **A split that loses the root CAS changes nothing observable** — the new buckets may
   exist, and the width has not moved, so every reader still reads the old shape and gets
   every tenant.
9. **Zero LIST**, and the request count is a function of the width, never of the tenant count.
10. ⚠️ **A backend that cannot fence is refused**, and splits nothing.
11. Region coverage ≥95% on `pstore-catalog`, mutation ≥80% on the changed module, gates green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `every_tenant_survives_a_split` | the partition dropping a side — half the catalog, silently |
| 2 | `a_record_lands_where_the_new_width_says` | the partition predicate inverted, which enumerate cannot see because it reads every bucket |
| 3 | `an_old_width_reader_is_complete_after_a_split` | the old bucket rewritten without the moved records — pruning by accident, which is the failure this protocol exists to avoid |
| 4 | `a_census_at_a_width_the_deployment_left_is_refused` | the check dropped, which returns a census that is quietly short |
| 5 | `every_tenant_survives_a_split` | the cross-bucket merge dropped, which counts a moved tenant twice for as long as the old buckets stand |
| 5 | `a_stale_width_writer_is_still_found` | as 3, from the write side and worse — a lost write rather than a short read |
| 6 | `splitting_stops_at_the_key_format` | `MAX_WIDTH` compared with `<=`, which writes a five-digit bucket into a four-digit key space |
| 7 | `a_split_that_loses_the_root_cas_changes_nothing` | the root written before the buckets, which publishes a width whose buckets do not exist yet |
| 8 | `splitting_does_not_list` | a LIST introduced to find buckets |
| 9 | `a_catalog_write_on_a_divergent_backend_is_refused`, with `split` added | `require_fencing` after the writes |

⚠️ Criteria 2 and 3 must both hold: 3 alone passes if the split moved nothing, and 2 alone
passes if it moved everything and deleted the originals.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `split` | `w` heads + `w` runs + 1 root CAS | **3** — root, then heads, then runs; writes after | `w` wide per round | 0 |
| `enumerate` | 0 | **3**, was 2 — the closing root read | unchanged | 0 |
| `fold` / `reap` / `sweep` / append | unchanged | unchanged | unchanged | 0 |

⚠️ `enumerate` gains one round trip and one request, and stays inside the budget of three. A
caller that reads the root itself first pays four in total — and the second read is what makes
the first trustworthy, because the shape can move between them.

## Risks

- ⚠️ **A split doubles the deployment's bucket objects and prunes nothing.** Two doublings from
  the default leaves 65,536 heads where 16,384 stood, with the old ones still holding records
  that have moved. Correct, and it is storage nobody reclaims.
- **`split` is not atomic and not resumable.** A crash between the bucket writes and the root
  CAS leaves the new buckets written and the width unmoved — criterion 7 says that is
  observably nothing, and a re-run redoes the partitioning from the same source.
- ⚠️ **Nothing bounds a stale-width process.** That is why pruning is absent, and it means the
  cost above is permanent until something does.

## Tasks

| Id | Commit |
|---|---|
| **M6g.1** | `enumerate` refuses a width that moved under it |
| **M6g.2** | `split`, doubling into siblings and flipping the root last |
