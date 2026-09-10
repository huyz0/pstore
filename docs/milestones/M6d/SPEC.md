# M6d — Reaping a superseded catalog run

**Serves:** the half of backlog item 10 that has a proven pattern —
[M6a](../M6a/VERIFIED.md): *"A superseded run is garbage and stays;
`a_refused_pointer_cas_leaves_an_orphan_run_and_no_loss` pins the half-done fold that makes
one. Reaping needs a retention window, or a reader mid-enumeration loses the run it is on."*

**Depends on** [M2](../M2/SPEC.md)'s `Engine::gc`, which solved exactly this for segments: a
graveyard keyed by epoch, reaped once the tenant has committed `retention` further epochs, and
**zero LIST**.

## ⚠️ Why a graveyard is required rather than convenient

A segment's key is remembered in HEAD; a run's key is **derived** — `run_key(bucket,
run_epoch, digest)`. The digest is content-derived, so **an old run's key cannot be computed
from the current head**: the head carries only the digest of the run it names. Reaping without
recording what was superseded means the objects are unreachable, and unreachable garbage is
permanent garbage.

## ⚠️ What premature reaping does, and the one piece of good news

`a_pointer_to_a_run_that_is_gone_is_an_error` already pins it: a run key that 404s raises
`CatalogError::MissingRun`, deliberately, because *"a derived pointer key that 404s means
'empty bucket', and a run key that 404s means a bucket's worth of tenants has vanished"*.

So reaping too early **fails loudly** rather than reporting fewer tenants than exist. That is
what makes a retention window a safe knob rather than a silent correctness risk — and it is
why the window is a number in the caller's hands, not a constant here.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| Graveyard entries | **`retention`, capped at `MAX_GRAVEYARD = 8`** | 16 bytes an entry, in an object CAS'd on **every** append and fold. Unbounded, this grows the hottest small object in the catalog forever — which is the failure `MAX_PENDING = 8` already exists to prevent one field over. |
| Retention | **a caller's argument**, `0..=MAX_GRAVEYARD` | **How many superseded runs are kept.** ⚠️ Stated as a count rather than as epoch arithmetic, and that is a review finding: `run_epoch + retention <= head.run_epoch` invites an off-by-one whose failure mode is reaping a run a reader is on, and the graveyard is already newest-first, so "keep the newest `retention`" needs no arithmetic at all. `0` reaps a run the moment it is superseded; `2` keeps it for two further folds. The window is counted in **folds of that bucket**, not seconds. `Engine::gc(retention)` takes it the same way and for the same reason: this layer has no clock. ⚠️ **Refused above `MAX_GRAVEYARD`**, and that is the whole interaction between the two numbers: a window of 20 against a record of 8 evicts runs 9 through 20 from the graveyard **while they are still inside the window they were promised**, turning them into permanently unreachable garbage. A promise larger than the record is refused rather than silently broken. |
| Delete then CAS | **delete first** | ⚠️ The mirror of the engine's ordering argument. Deleting and then failing the head CAS leaves graveyard entries naming absent objects, and the next reap re-deletes them — `delete_batch` on a missing key is not an error. CAS-then-delete leaves objects with **no record**, which is the unreachable case this milestone exists to remove. |

## Delta

**`pstore-catalog`**
- `BucketHead` gains `graveyard: Vec<(Epoch, u64)>` — `(run_epoch, digest)` of runs this
  bucket has superseded, newest first, truncated to `MAX_GRAVEYARD`. ⚠️ Appended to the
  encoding **after** `pending`, so a head written before this milestone decodes with an empty
  graveyard rather than failing — the same additive rule `Section::TextFields` follows.
- ⚠️ **`Appender::record` must keep the graveyard**, and today it does *because* it mutates
  the head it read (`head.pending = next`) rather than constructing a fresh one. A refactor to
  `BucketHead { run_epoch, digest, pending: next }` would silently erase the record and orphan
  every run in it — permanently, since the keys are derived. Pinned by a test.
- `publish` pushes the head's current `(run_epoch, digest)` onto the graveyard before writing
  the new head. ⚠️ Only when there **is** one: a bucket at `Epoch::ZERO` has never folded.
  ⚠️ **Deduplicated**, because `publish` is retried: two attempts against the same head each
  supersede the same run, and a duplicate entry spends a slot inside a bound whose whole job
  is to keep this object small.
- `reap(store, bucket, retention) -> Result<usize, CatalogError>` — deletes every graveyard
  entry past the newest `retention`, then CASes the head keeping only those.
  Returns how many objects it reaped, the shape `Engine::gc` already returns. ⚠️ **Retries on
  a contended head**, up to `MAX_CAS_ATTEMPTS`, exactly as `fold` does — and recomputes the
  doomed set from the head it re-read, because the graveyard may have shifted under it.
- ⚠️ **AMENDED after implementation: `read_root` returns the root's tag.** It returned only
  the `Root`, so `write_root`'s `Some(previous)` arm had **no reachable caller** — every caller
  could pass only `None`, which is create-if-absent and fails the moment the root exists. The
  width could therefore be set once and **never changed**, which is a concrete blocker under
  OQ-8 one layer below its protocol question. Found by the region floor while closing this
  milestone's criterion 10, which is what a coverage floor is for; included rather than
  deferred because it is three lines and a shape `read_head` already has.
- ⚠️ `reap` is a **guarded door**: `require_fencing` first, like `fold`. M7a made this a rule
  and the reason applies harder here — a reaper on a backend that cannot fence deletes objects
  and then fails to record that it did.

**Does not add** — **reaping an orphan run.** A fold that writes the run and loses the head CAS
leaves an object no head and no graveyard ever names, and its key cannot be derived from
anything that survives. ⚠️ Reachable only by **LIST**, and the corpus forbids LIST on read,
write and startup paths — a reaper is none of those, so a LIST-based sweeper is *permitted*
and is a separate decision with a cost model. Two cheaper half-measures were considered and
rejected: carrying the orphan into the retry's graveyard only helps when the writer survives
to retry, and writing a tombstone object needs its own reaper. **Bucket splitting (OQ-8)** —
still the other half of item 10, still blocked on a protocol that keeps an old-width reader
stale rather than wrong, and on `{bucket:04x}` capping at 65,536. **A caller.** Nothing folds
the catalog on a schedule; `reap` is a mechanism, exactly as `fold` is.

## Acceptance criteria

1. **A superseded run is reaped, and the one in use is not.** Two folds with `retention = 0`:
   the run from the first fold is gone, the run the head names is present, and `enumerate`
   returns every record. ⚠️ `0`, because `retention` is a count of kept superseded runs — the
   live run is never a graveyard entry, so keeping none of the dead ones cannot touch it.
2. ⚠️ **The window is honoured.** With `retention = 2`, a superseded run survives the next
   **two** folds and is reaped on the third — asserted by the object existing at each step,
   not by the return count alone. Spelled out because "retention = 2" and "survives two more
   folds" is exactly the arithmetic an off-by-one hides in, and getting it wrong reaps a run a
   reader is on.
3. ⚠️ **Reaping deletes before it commits.** Against a store that refuses every head CAS,
   `reap` errors, **the objects are gone**, and the head's graveyard still names them — so a
   later `reap` against a working store returns `Ok` rather than erroring on absent keys. The
   other order leaves objects with no record, which is the unreachable case this milestone
   exists to remove. ⚠️ Asserted on the objects and the error, not on a delete count: `reap`
   retries a contended head like `fold` does, so it deletes once per attempt.
4. **Zero LIST**, asserted on the request-class counter across fold, reap and enumerate.
5. ⚠️ **An append does not erase the graveyard.** Fold, append, then reap: the reap still
   finds and deletes the superseded run. Without this, an append between a fold and a reap
   orphans every run recorded so far, unreachably.
6. ⚠️ **A retention larger than the record is refused.** `reap(store, bucket,
   MAX_GRAVEYARD + 1)` returns an error and deletes nothing. Without it the window is a
   promise the graveyard cannot keep, and the runs it evicts are unreachable forever.
7. ⚠️ **A pre-M6d head decodes with an empty graveyard**, and reaping it is a no-op returning
   0 rather than an error. Constructed from bytes without the new field.
8. **The graveyard is bounded.** Ten folds leave at most `MAX_GRAVEYARD` entries, and the head
   stays under the size the append path assumes.
9. ⚠️ **A backend that cannot fence is refused**, added to `refusal.rs`'s existing loop over
   every guarded door rather than as a second fixture.
10. ⚠️ **A width change is conditioned on the root it read**, and a stale tag loses. Added by
    the amendment above; without it the root is write-once.
11. Region coverage ≥95% on `pstore-catalog`, mutation ≥80% on the changed module, gates green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_superseded_run_is_reaped_and_the_live_one_is_not` | the comparison reaping the head's own run, which is `MissingRun` on the next enumeration |
| 2 | `the_retention_window_is_counted_in_folds` | `retention` ignored, or the split taken from the wrong end of a newest-first list — either reaps a run a reader may still be on |
| 3 | `a_reap_that_loses_the_head_cas_still_deleted_and_is_idempotent` | the CAS moved before the delete, which leaks objects with no record; and an absent key treated as an error |
| 4 | `reaping_does_not_list` | a LIST introduced to find runs |
| 5 | `an_append_between_a_fold_and_a_reap_keeps_the_graveyard` | the head rebuilt rather than mutated on the append path, which orphans every recorded run and no other test would notice |
| 6 | `a_retention_wider_than_the_graveyard_is_refused` | the cap applied silently, which evicts runs still inside their promised window and makes them unreachable |
| 7 | `a_head_written_before_the_graveyard_decodes_and_reaps_nothing` | a decoder that requires the field, which makes every existing head unreadable |
| 8 | `the_graveyard_is_bounded_by_max_graveyard` | the truncation dropped, which grows the object CAS'd on every append forever |
| 9 | `refusal.rs`'s guarded-door loop, with `reap` added | `require_fencing` placed after the delete, which is the M7a finding one door over |
| 10 | `a_width_change_is_conditioned_on_the_root_it_read`, `a_root_write_that_fails_for_io_is_not_reported_as_a_lost_race` | the tag dropped again, which makes the root write-once; and an I/O failure reported as `RootContended`, which tells a caller someone else won when nobody did |

⚠️ Criteria 1–3 are the milestone. Criterion 3 is the one no functional test can see: both
orders reap the right objects on a store that never fails.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `reap` | 1 CAS + 1 batched delete | **3** — read the head, delete, then CAS | 0 | 0 |
| `fold` | unchanged | unchanged | unchanged | 0 |
| `enumerate` | 0 | unchanged | unchanged | 0 |

⚠️ **3, not 2**, and the delete cannot join the CAS's round: the ordering is the correctness
argument, so the CAS must not be issued until the delete has returned. A background job is the
one place in this system where depth is allowed to cost something.

⚠️ Per **bucket**, and a deployment-wide reap is 16,384 of these. That is the same shape as
`fold` and it is a background job; the point is that it never scales with **tenants**.

## Risks

- **The head grows by up to 128 bytes**, in the object every append CASes. `MAX_GRAVEYARD`
  bounds it; the risk is that a later change raises the bound for reaping's convenience and
  pays for it on the write path.
- **`retention` is a promise nobody enforces.** A reader that pauses longer than `retention`
  folds gets `MissingRun`. Loud, and still a failure — the same contract `Engine::gc` has.
- **Orphan runs still accumulate**, one per fold that loses its head CAS. Under contention that
  is not rare, and this milestone does not reduce it. Named above rather than implied.

## Tasks

| Id | Commit |
|---|---|
| **M6d.1** | `BucketHead` records what it superseded, additively |
| **M6d.2** | `reap`, guarded, deleting before it commits |
