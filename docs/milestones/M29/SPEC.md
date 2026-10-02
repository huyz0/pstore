# M29 — A committing compaction buries everything it wrote and abandoned

**Serves:** [BACKLOG](../BACKLOG.md) row 49, which [M23](../M23/VERIFIED.md)'s code review opened,
and row 43, which [M19](../M19/VERIFIED.md)'s did.

## What is true today

- **Row 49.** `seal` creates a segment, then each of its sidecars, create-if-absent (M23).
  - When a sidecar's name is taken, which can only be a stale orphan's, `seal` abandons that
    segment name and takes the next.
  - The segment it created at the abandoned name stays in the call's record (`names.written`).
    So do any sidecars created there before the refusal, as derived keys of that segment.
  - A compaction buries its record only when it does **not** commit (`compact_inner`).
  - When it commits, it buries `stale`, the segments its earlier lost attempts sealed, and
    nothing else. The abandoned segment is in no HEAD and no graveyard, and nothing ever
    deletes it.
  - The fold does not have this hole: its commit buries its whole record through `bury_into`
    (M23). A branch's commit names every copy it keeps, and its abandoned copies are its
    lost attempts' `stale`.
- **Row 43.** It said a `Contended` retry on a branch's success path could bury a live copy
  key: an earlier attempt's copy, at the same key the retry wrote again.
  - Since M23, a retry takes the next name, so no two attempts share a key, and the case no
    longer arises.
  - Nothing pins that.

## Delta

**A compaction's commit buries its whole record through `bury_into`, as the fold's does.** It
replaces the `stale` loop: every key the call created, or may have, that the committing HEAD
neither names nor has buried, is buried under its own key epoch.
- `stale` is a subset of the record, since every seal records what it creates. So nothing
  buried today stops being buried.
- `bury_into` never buries a key `next` names, so the merged segment, live in `next`, is never
  buried.
- A segment's sidecars die with it: GC deletes a buried segment's derived keys, as it does for
  every buried segment (`a_fold_buries_what_its_discarded_attempts_created`).

**Row 43 is closed by M23, and pinned.** A test makes a branch's first commit answer `Contended`
without landing. The retry lands, and no key is both live and buried. No branch code changes:
a test that the change could fail does not exist, because M23's names already make the keys
distinct (`gate-design`: no change without a test that would see it).

**Not changed:** the fold, the branch, the graveyard's meaning, GC, the format, and every
request count.

## Acceptance criteria

1. **The abandoned segment is buried.** An object is planted at the centroid table's name for a
   compaction's first segment name. The compaction creates the segment there, is refused the
   table, seals at the next name, and commits. The abandoned segment is in the graveyard,
   under its own key epoch. On the parent commit it is in neither HEAD nor the graveyard.
2. **And reaped.** After `gc(0)`, the abandoned segment is gone from the store, and the merged
   segment and its rows remain.
3. **Nothing live is buried.** No key the committing HEAD names is in its graveyard. The planted
   table is untouched before GC, and gone after it, with the segment it derives from.
4. **Lost attempts are still buried.** `a_paused_compaction_does_not_replace_a_live_segment`,
   `a_compaction_retried_at_its_own_epoch_does_not_reseal` and the M19 compaction tests pass
   unchanged.
   - ⚠️ Amended at implementation: with the burial call removed, no existing test failed. So
     `a_committed_compaction_buries_its_lost_attempts_seal` pins it: a lost CAS, a re-seal,
     a commit, and the first seal buried and reaped.
5. **Row 43 pinned.** A branch of a source with a delete vector, whose first commit answers
   `Contended` without landing, commits on its retry. The first attempt's copy key differs from
   the committed one, and is buried, then reaped. No key is both live and buried.
6. **Gates.** `./scripts/gates.sh` is green, and the mutation sweep over M29's source diff misses
   0. Every miss is closed by a test or named as equivalent.

## Test plan

In `crates/pstore-engine/tests/segment_names.rs` (1–3) and `abandon.rs` (5).
- Test 1's compaction runs with `exact_scan_threshold: 1`, so its merge writes a centroid table
  and the planted object is refused (spec review m4).
- ⚠️ **Test 5 passes on the parent** (spec review M1). It is seen red by restoring M23's two
  halves together (round 2):
  - `Names::take` ignores its counter, so a retry reuses its first name;
  - a create replaces, so that name is written again rather than refused.
  - The copy key then repeats, and is expected to be live and buried at once. With `take`'s
    fault alone, the create refuses the retry 16 times and the branch fails `Contended`.
  - The ledger records which red was observed, not this expectation.
- Criterion 4's coverage of a lost attempt's burial is checked by mutation at implementation:
  the burial call removed must fail a test that reseals and commits, which the ledger names
  (spec review m5).

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1, 2 | `a_committed_compaction_buries_a_name_it_abandoned` | not buried; the record not passed to the commit |
| 3 | the same test, and `a_refused_name_is_never_buried` (existing) | a live key buried (the `live` filter dropped is caught by M23's tests) |
| 4 | existing compaction tests | the burial call removed |
| 5 | `a_contended_branch_retry_buries_no_live_copy` | a guard, red under M23 reverted (both halves) |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| A compaction that commits | 0 | 0 | 0 | 0 | unchanged: the burial rides its commit |
| GC | the abandoned segment's deletes, once | unchanged | unchanged | 0 | unchanged |

HEAD grows by one graveyard entry per abandoned name, until GC reaps it: bytes, not requests.

## Risks

- **A key `names.written` records that another process won.** An `Io` create that did not land
  records its name, and a same-lane twin may then win that name and commit it. `bury_into`
  skips keys the committing HEAD names. A twin's key that a later HEAD names, but this one does
  not, cannot exist: this commit is the newer one. That is M23's argument for the fold, and it
  holds unchanged here.

- **GC now deletes a planted sidecar** (spec review m3). It derives the sidecar keys of every
  buried segment, so the stale orphan that refused this compaction is deleted with the segment
  it derives from. That is safe: a sidecar whose segment is buried can only be an orphan.
  Before M29 both leaked.

## Tasks

- **M29.1** — The compaction's commit buries its record, and the tests.
- **M29.2** — The ledger, `BACKLOG.md` rows 49 and 43 closed, and the roadmap row.
