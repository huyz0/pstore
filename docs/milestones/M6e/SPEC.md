# M6e — The orphan run a graveyard cannot reach

**Serves:** backlog item 13, which [M6d](../M6d/VERIFIED.md) opened by name — *"a fold that
writes its run and loses the head CAS leaves an object **no head and no graveyard names**, and
its key cannot be derived from anything that survives. Reachable only by **LIST**."*

**Depends on** [M6d](../M6d/SPEC.md)'s graveyard, which reaps everything a head *does* name and
is why the remainder is a small bounded set rather than every run ever written.

## ⚠️ Why LIST, and why it is allowed here

`a_refused_pointer_cas_leaves_an_orphan_run_and_no_loss` pins the half-done fold: the run
landed and the pointer did not. That is the **safe** half of the ordering — the other order
leaves a pointer naming an object that was never written, which is a bucket lost — and its
price is an object nothing names. The key is `run_key(bucket, run_epoch, digest)` and the
digest is content-derived, so it cannot be computed from anything that survives.

`BlobStore::list_unrestricted` states the cost: *"LIST is priced like a PUT, returns at most
1000 keys, is inherently serial, and tells you what objects exist rather than what is
committed"* — and it is **permitted in GC**, which is what this is. The corpus forbids LIST on
read, write and startup paths. A sweeper is none of those.

## ⚠️ The failure this milestone must not cause, which is worse than the one it fixes

An orphan is garbage. A run a fold is **about to** commit is a bucket's worth of tenants. They
sit in the same prefix, and a sweeper that cannot tell them apart deletes the second — after
which the winning CAS publishes a head naming an object that no longer exists, and
`a_pointer_to_a_run_that_is_gone_is_an_error` turns every enumeration of that bucket into
`MissingRun`.

⚠️ **The epoch in the key settles it without a clock.** A run in flight was written against the
head its writer read, so its `run_epoch` is `head.run_epoch + 1` — strictly **greater** than
the head's. An orphan is one the head has already moved past:

> **An unnamed run whose `run_epoch` is strictly less than the head's is garbage.
> Everything else is left alone.**

No timestamp, no grace period, no `head` request per object. ⚠️ Two folders racing at one epoch
produce two runs at `head.run_epoch + 1` with different digests; after the winner commits the
loser's epoch **equals** the head's rather than being less, so it survives this sweep and the
next one collects it. Late, and never early.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| Prefix | **`{bucket:04x}/cat/b/`** | One LIST per bucket. ⚠️ It returns the bucket's `HEAD` too — the head key shares the prefix — so `HEAD` is skipped by name. `{run_epoch:020}` is fixed width and all digits, which is `key-layout.md` naming rule 3, and is why a listing comes back in epoch order. |
| Cost | **1 LIST + 1 batched DELETE per bucket** | ⚠️ Scales with **buckets**, not tenants: a deployment-wide sweep at `DEFAULT_WIDTH` is 16,384 LISTs, and 1M tenants does not change that. The rule bounds requests by nodes and bytes; a sweeper bounded by shard count is the same shape as `fold`. |
| Keys per bucket | `MAX_GRAVEYARD` + orphans + 1 | A LIST returns at most 1000 keys. Past that a sweep is **incomplete, not wrong**: it reaps what it saw and the next one sees the rest. Stated rather than paginated, because `list_unrestricted` has no cursor. |

## Delta

**`pstore-catalog`**
- `sweep(store, bucket) -> Result<usize, CatalogError>` — LISTs the bucket's prefix, keeps the
  head, the run it names and every graveyard entry, and deletes every remaining run whose
  epoch is strictly below the head's. Returns how many it reaped.
- ⚠️ A **guarded door**: `require_fencing` first, like `fold` and `reap`. A sweeper on a backend
  that cannot fence reads a head it cannot trust and deletes on the strength of it.
- ⚠️ **Anything it cannot positively identify is left alone.** The decision is made on a key
  parsed back into `(run_epoch, digest)`; a key under this prefix that does not parse — the
  `HEAD`, or any sibling object a later milestone puts there — is **kept**. The opposite
  default, "delete what I do not recognise", is how a sweeper deletes the bucket's pointer, and
  it is one edit away in either direction.
- ⚠️ **A bucket with no head reaps nothing.** `read_head` reads an absent pointer as
  `Epoch::ZERO`, and nothing is strictly below zero. A first fold that lost its
  create-if-absent therefore leaves an orphan this cannot collect until some fold succeeds —
  safe, and stated rather than discovered.
- ⚠️ **No CAS and no head write.** `sweep` only deletes; there is nothing to record, because an
  orphan is defined by *absence* from the head. That also makes it idempotent and safe beside a
  concurrent fold: the worst case is a head one epoch stale and one run fewer reaped.

**Does not add** — **pagination.** `list_unrestricted` returns a `Vec<Key>` with no cursor, so a
bucket holding more than 1000 objects is swept across several runs. **A schedule.** Nothing
calls `fold`, `reap` or `sweep` on a timer; the caller is whoever composes a serving stack.
**Sweeping anything but runs** — bundles, segments and the root have their own owners.

## Acceptance criteria

1. **An orphan is reaped and the live run is not.** A fold whose head CAS fails leaves a run
   nothing names; once the head has advanced, `sweep` deletes it, the run the head names
   survives, and `enumerate` still returns every record.
2. ⚠️ **A run in flight is never touched.** An unnamed run at `head.run_epoch + 1` — exactly
   what a fold that has written its run and not yet committed looks like — survives. Observed
   red by comparing on `<=`, which deletes the object a fold is about to commit.
3. ⚠️ **The graveyard's runs are not touched either.** M6d keeps the newest `retention`
   superseded runs so a reader mid-enumeration does not lose the one it is on; a sweeper
   ignoring the graveyard reaps exactly those.
4. ⚠️ **Anything unrecognised survives, the head included.** The head shares the prefix, and a
   key that does not parse as a run is kept — asserted with the head **and** a foreign object
   written under the same prefix, because "delete what I do not recognise" passes a test that
   only checks the head is special-cased by name.
5. **The sweep records nothing and is idempotent** — a second `sweep` returns 0 and the head is
   byte-identical to before the first.
6. ⚠️ **A backend that cannot fence is refused**, and sweeps nothing.
7. Region coverage ≥95% on `pstore-catalog`, mutation ≥80% on the changed module, gates green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `an_orphan_run_is_swept_and_the_live_one_is_not` | the epoch comparison inverted, or the head's own run not excluded — either loses a bucket |
| 2 | `a_run_in_flight_survives_the_sweep` | `<` widened to `<=`, which deletes the object a winning fold is about to name. **The failure this milestone must not cause.** |
| 3 | `the_graveyards_runs_survive_the_sweep` | the graveyard not consulted, which undoes M6d's retention window |
| 4 | `an_unrecognised_object_under_the_prefix_survives` | the default flipped to "delete what does not parse", which deletes the bucket's pointer and anything a later milestone puts beside it |
| 5 | `a_second_sweep_finds_nothing_and_changes_nothing` | a head write introduced, which turns a read-only sweeper into a CAS contender |
| 6 | `a_catalog_write_on_a_divergent_backend_is_refused`, with `sweep` added | `require_fencing` placed after the delete |

⚠️ Criterion 2 is the milestone. Criterion 1 passes over a sweeper that deletes everything it
does not recognise, and so does criterion 3 if the fixture has no graveyard.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `sweep`, per bucket | 1 batched DELETE | **2** — the head, then the LIST and the delete | 0 | **1** |
| `fold` / `reap` / `enumerate` | unchanged | unchanged | unchanged | 0 |

⚠️ The only LIST in the catalog, on the only path where the corpus permits one. Per bucket,
never per tenant.

## Risks

- **A sweep is only as fresh as the head it read.** A concurrent fold makes it reap one run
  fewer, never one more — the safe direction, and why the comparison is strict rather than a
  grace period.
- **1000 keys.** A bucket accumulating more orphans than that between sweeps is swept
  incrementally, and nothing reports how far behind it is.
- ⚠️ **The orphan rate is not measured.** M6d said these accumulate under contention; this
  collects them without saying how many there were. `sweep`'s return count is the only signal,
  and nobody reads it.

## Tasks

| Id | Commit |
|---|---|
| **M6e.1** | `sweep`, guarded, deleting only what the head has moved past |
