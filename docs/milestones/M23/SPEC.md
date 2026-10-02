# M23 — A segment is created, never replaced

**Serves:** [BACKLOG](../BACKLOG.md) row 44, and Invariant I1 — no in-place mutation
([engineering-standards](../../research/09-rust-stack/engineering-standards.md) § 7), which an
overwritten segment breaks.

## What is true today

- A fold's L0 segment (`segment_key`), a compaction's L1 segment (`compacted_key`) and a delete
  vector (`head::dv_key`, written by `supersede` and by a branch's copies) are keyed by the
  writer's `(epoch, lane)`, at `HEAD.epoch + 1`, and written with an unconditional `put`.
- A segment's sidecars are named after it, and `seal` writes them **before** the segment: the
  sparse and text dictionaries, and the centroid table.
- Two processes on one lane can reach the same key. One pauses after its HEAD read; its restarted
  successor commits at that key; when the first wakes, its `put` replaces a segment HEAD names,
  with other rows. Those rows were acknowledged and folded, and are gone with no error.
- Within one process, a retry against an unchanged HEAD re-seals the same key, and an earlier
  attempt's PUT landing late replaces the retry's.
- Two comments justify the unconditional `put` by "create-if-absent is not honoured everywhere"
  (`head.rs` above `dv_key`; `compacted_key`). That stopped being true with `require_fencing`,
  which refuses any backend whose `create_if_absent` is not `Supported`. M17's bundles rely on it.

## Delta

**Every such object is created, never replaced.** `put_conditional(key, body,
Precondition::NotExists)`, through one engine helper that picks the object's **name**:

- **Name `n`:** `n = 0` is today's key, unchanged. Each `n ≥ 1` appends `_{n:x}` to the
  stem: `…/seg/L0/{epoch:020}-{lane:016x}_{n:x}.seg` and `{segment}.{epoch:020}-{lane:016x}_{n:x}.dv`.
  `_` because `carried` (replica.rs) reads a third `-` field as a source hash.
- **`Lost` or `Contended`:** the name is taken, or may be. The next name is tried at once, with no
  read. A refusal is never an error and never a reason to re-read HEAD.
- **The next name is carried across an operation's attempts** (spec review B1). Each fold,
  compaction or branch call keeps, per base key, the next `n` to try. A retry at an unchanged
  epoch starts past every name it has already created or been refused. It is never refused by
  its own object, and never replaces its own late PUT. A new call starts at 0, and a refusal by
  an earlier call's object costs one write.
- **`Io`:** the attempt fails, as a failed `put` does today.
- **After 16 refused names in one call** the operation fails with `EngineError::Contended`,
  having replaced nothing. Not `Lost`: `Lost` tells the caller to rebase, and no HEAD moved
  (BACKLOG row 11; `retry_ceiling.rs`).
- **Compaction re-seals only when the epoch moved.** Its retry compares the epoch it sealed at
  with the next one, not keys, which differ once a name carries a suffix.

**A segment claims its name before its sidecars.** `seal` creates the segment first, then each
sidecar with create-if-absent, under the segment's chosen name. HEAD still names nothing until
the commit, after all of them, so a reader still never opens a segment whose sidecar is missing.
- A refused sidecar abandons the name, and `seal` moves to the next: it can only be a stale
  orphan's, since the segment there was this attempt's.
- Today's order would let a process lose the segment's name after creating a centroid table under
  it. Another process's segment would then be read with that table.

**Burial.** Branch and compaction record a key before writing it, so a write that fails
partway is buried (M19). A name **refused** is another process's object, or an earlier call's.
It is removed from that record and never buried. An `Io` name stays recorded, as M19 decided:
it may be another process's, and `bury_abandoned`'s live filter is what keeps it once committed.
**A fold now buries too** (spec review m2). Today a retry at its own epoch overwrites its earlier
segment, and nothing sweeps engine keys (M6e's `sweep` is the catalog's). Under M23 that earlier
segment would leak. So a fold records each name it creates, and buries what it no longer needs.
This also buries what a lost-CAS retry leaves, which leaks today.
- ⚠️ **Amended at implementation: in its own commit, never a commit of its own.** As specified, a
  burial commit after the fold, through `bury_abandoned`, broke two tests. They pin that the epochs
  folds report have no gaps (`engine.rs` `concurrent_committers_lose_and_duplicate_nothing`,
  `linearizability.rs` `one_hundred_writers_produce_a_dense_epoch_sequence`), and a burial commit
  is an epoch no fold reports.
- The names go into the graveyard of the commit the fold makes anyway, each under its own key
  epoch, as compaction buries its stale keys. Before that, any name that commit names, or has
  already buried, is dropped.
- A fold that ends with nothing to commit, or fails, keeps its names in the engine. This
  process's next fold commit buries them. A process that never commits again leaks them, which
  every such name does today.

**Comments.** The two that cite "not honoured everywhere" are corrected to cite `require_fencing`.

**Existing tests** (spec review M1). None is weakened; each keeps the mutation it was written for.
- `abandon.rs` `sealing_one_key_twice_writes_the_same_bytes`: its premise, the loser's PUT landing
  on the winner's committed key, is what M23 removes. Rewritten to the stronger property: the
  twin commits another name, and no key's bytes ever change.
- `abandon.rs` `an_error_after_sealing_buries_the_seal`: its fault moves with the write, from
  `put` to `put_conditional` on `.seg`.
- `abandon.rs` `a_same_lane_winners_key_is_not_buried` and `a_key_already_buried_is_not_buried_twice`:
  "both derive the same key" stops being true. The case left where a recorded name can be live is
  an `Io` create that did not land, whose name the twin then won. Both move to that setup: the
  `Io` fails the call at once, so the twin's compaction runs inside the store's hook, before it
  answers `Io`.
- `retry_ceiling.rs` (both) and `pstore-testkit` `Gated` (`compaction.rs`'s races): they race or
  refuse **HEAD's** CAS, and every conditional write now includes segment creates first. They are
  narrowed to the HEAD key, which keeps the commit loop the thing under test.
  - ⚠️ **`retry_ceiling` asserts an exact count** (spec review round 2). For the compaction it is
    its commit's attempts plus its burial's, 2 × `MAX_COMMIT_ATTEMPTS`. For the fold it is
    `MAX_COMMIT_ATTEMPTS`, since a fold makes no burial commit (amended above). `refused() > 1`
    passes a commit loop that never retried once the burial's attempts are refused too. The
    compaction test had the same hole before M23, and this closes it.
  - `Gated` gains `Gated::only(n, suffix)`, and `compaction.rs` uses it with `"/HEAD"`. Its other
    users gate catalog keys, so narrowing `Gated::new` itself would break them.

**Not changed:**
- **Bundles:** M17's.
- **Replication's copies:** a copy's name is a hash of its source key. Since this milestone
  makes source keys unreplaceable, the same name always carries the same bytes, so an overwrite
  changes nothing.
- **HEAD, the lanes registry, and the job register:** these are CAS writes already.
- **The format, HEAD's encoding, and any API.**
- **Mixed versions:** a pre-M23 binary still writes unconditionally. The guarantee needs every
  process to run M23.

## Acceptance criteria

1. **A paused fold never replaces a live segment.** Fold A reads HEAD and is held at its
   segment write. Fold B, on the same lane, commits at that key. A is released and commits.
   B's segment bytes are unchanged, and every row both folds folded is served.
2. **A paused compaction never replaces a live segment.** Compaction A fixes its inputs, loses its
   CAS, and is held at its re-seal at the next epoch. Compaction B, on the same lane, merges the
   newer HEAD and commits that key. B's segment bytes are unchanged.
3. **A paused fold never replaces a live delete vector.** The same, with two folds deleting
   from one segment.
4. **A branch's delete-vector copy never replaces a live one.** `src2` is branched from `src1` and
   deletes more rows. Both are branched to `dest` by two same-lane branches, so both copy a vector
   to one key with different rows. The first committed vector's bytes are unchanged.
5. **A retry at its own epoch takes the next name.** A fold whose first HEAD CAS answers
   `Contended` commits on its retry, at the `_1` name, and its earlier object is not replaced. A
   compaction retried at its own epoch, its seal already at a suffixed name, does not re-seal.
6. **An uncontended operation is unchanged on the wire.** A fold, a compaction and a branch
   each issue the same requests as before, with conditional PUTs in place of PUTs, and commit
   today's key names. Each refused name costs exactly one more write request.
7. **The segment claims its name first.** When A's fold, with a centroid table, is refused the
   name B's centroid-less segment holds, no centroid table exists at B's name, and B's queries
   scan exactly.
8. **A refused name is never buried.** A compaction is refused at a name holding a planted object
   HEAD does not name, then abandoned. That name is not in the graveyard. With the refused name
   recorded, the test fails; `bury_abandoned`'s live filter cannot save it.
9. **Bounded.** With every create refused, a fold and a compaction each fail with
   `EngineError::Contended` after 16 names, and replace nothing.
10. **Suffixed names are read like any other.** A `_1` segment and delete vector are resolved by
    `as_of`, reaped by GC once buried past retention, and not mistaken for a replica copy.
11. **A fold buries what its discarded attempts created.** A fold retried at its own epoch, and one
   retried after a lost CAS, each leave every created name either in its committed HEAD or in the
   graveyard, with no commit of their own, and GC reaps the rest. A fold left with nothing to
   commit buries its names at this process's next fold commit.
12. **Gates.** `./scripts/gates.sh` is green, and the mutation sweep over M23's source diff
    misses 0, every miss closed by a test or named as equivalent.

## Test plan

Engine tests in `crates/pstore-engine/tests/segment_names.rs`. A store wrapper holds one
engine's next write under `/seg/` until released. That is how a pause between the HEAD read
and the PUT is made, deterministically. Two engines share one lane and one store. The rows they
fold are written by a third engine on another lane, so M17's `LaneTaken` never enters.

| # | Test | Must fail first because / mutation it catches |
|---|---|---|
| 1 | `a_paused_fold_does_not_replace_a_live_segment` | `put` in place of the create: B's rows vanish |
| 2 | `a_paused_compaction_does_not_replace_a_live_segment` | the same, in `compact` |
| 3 | `a_paused_fold_does_not_replace_a_live_delete_vector` | the same, in `supersede` |
| 4 | `a_branch_does_not_replace_a_live_delete_vector` | the same, in the branch's copy |
| 5 | `a_fold_retried_at_its_own_epoch_takes_the_next_name` | a refusal answered as an error; `n` not advanced |
| 6 | `an_uncontended_fold_compaction_and_branch_are_unchanged` | a suffix at `n = 0`; an extra request per create |
| 7 | `a_segment_claims_its_name_before_its_sidecars` | sidecars first: B's segment gains A's centroids |
| 8 | `a_refused_name_is_never_buried` | the name left in the burial record |
| 9 | `sixteen_refusals_fail_the_operation` | an unbounded loop (times out); `Lost` reported |
| 10 | `a_suffixed_name_is_resolved_reaped_and_not_a_copy` | `-` in place of `_`; a parser reading `n` as an epoch |
| 11 | `a_fold_buries_what_its_discarded_attempts_created` | no fold burial; the committed name buried |
| 5′ | `a_compaction_retried_at_its_own_epoch_does_not_reseal` | keys compared in place of epochs (its seal refused once first, so the key carries `_1`) |

Each is observed red on today's code, or for 6, 9 and 10, which today's code passes, against
the mutation named. Setups: 2 needs A's re-seal after a lost CAS, since two compactions of one HEAD
seal identical bytes. 4 needs two sources sharing a segment with different vectors.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Fold, compaction, branch, uncontended | unchanged: each PUT becomes a conditional PUT | unchanged | unchanged | 0 | unchanged |
| Each refused name | **+1**, plus any sidecar already created under it | 0 | 0 | 0 | +1 write round |
| A fold with a discarded attempt | unchanged: buried in its own, or the next, commit | 0 | 0 | 0 | unchanged |

Queries are untouched. HEAD's bytes grow only by the 2+ characters of a suffix, and only after a
refusal.

## Risks

- **A stale sidecar under a name this process wins.** GC reaps a buried segment with its
  sidecars. If anything ever reaped a segment and left a sidecar, a later segment at that name
  with no sidecar of its own would be read with the stale one. Test 7's setup is where it shows.
- **Segment writes change fault class** (m3). `Faulty`'s `write_error` covers `put` only, and
  `Congested` retries `put` on a 503 but never `put_conditional`. Segment writes now get CAS faults
  and no 503 retry, as bundles have since M17. `pstore-testkit`'s sweep axes run engine ops under
  CAS faults, and will say if 16 names is too few. The bound is not raised to pass a test.
- **`Faulty`'s `cas_lost` and `cas_contended` now refuse segment and delete-vector creates.**
  A test injecting them at a high rate around folds could exhaust 16 names. The suite says so;
  the bound is not raised to pass it.
- **A `Contended` that did write.** It is taken as maybe-written, and the next name is used. The
  object it might have left is an orphan, never a replacement.

## Tasks

- **M23.1** — The naming helper, carried per call. Segment-first `seal`. Every site: fold,
  `supersede`, compaction, branch. Exhaustion as `Contended`, compaction's epoch compare, the
  burial rule, the fold's burial, the two comments, the existing tests above, and tests 1–11.
- **M23.2** — The ledger, `BACKLOG.md` row 44 closed, and the roadmap row.
