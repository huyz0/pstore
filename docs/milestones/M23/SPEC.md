# M23 — A segment is created, never replaced

**Serves:** [BACKLOG](../BACKLOG.md) row 44, and Invariant I1 — no in-place mutation
([engineering-standards](../../research/09-rust-stack/engineering-standards.md) § 7), which an
overwritten segment breaks.

## What is true today

- Four kinds of object are keyed by the writer's `(epoch, lane)` and written with an
  unconditional `put`:
  - a fold's L0 segment (`segment_key`);
  - a compaction's L1 segment (`compacted_key`);
  - a delete vector (`head::dv_key`), written by a fold's `supersede` and by a branch's copies.
- A segment's sidecars are named after it, and `seal` writes them **before** the segment: the
  sparse and text dictionaries, and the centroid table.
- The epoch is `HEAD.epoch + 1` at the HEAD the attempt read.
- Two processes on one lane can reach the same key:
  - a process pauses after its HEAD read;
  - its restarted successor commits at that key;
  - when the first one wakes, its `put` replaces a segment HEAD names, with other rows;
  - those rows are acknowledged and folded, and are gone with no error.
- The same happens within one process. A retry against an unchanged HEAD (after `Contended`,
  or a later fold after an `Io`) re-seals the same key, and an earlier attempt's PUT that lands
  late replaces the retry's.
- Two comments justify the unconditional `put` by "create-if-absent is not honoured everywhere"
  (`head.rs` above `dv_key`; `compacted_key`). That stopped being true with `require_fencing`,
  which refuses any backend whose `create_if_absent` is not `Supported`. M17's bundles rely on it.

## Delta

**Every such object is created, never replaced.** `put_conditional(key, body,
Precondition::NotExists)`, through one engine helper that picks the object's **name**:

- **Name `n`:** `n = 0` is today's key, unchanged. Each `n ≥ 1` appends `_{n:x}` to the
  stem: `…/seg/L0/{epoch:020}-{lane:016x}_{n:x}.seg` and `{segment}.{epoch:020}-{lane:016x}_{n:x}.dv`.
  `_` because `carried` (replica.rs) reads a third `-` field as a source hash.
- **Created:** the name is this attempt's.
- **`Lost` or `Contended`:** the name is taken, or may be. The next name is tried at once, with no
  read. A refusal is never an error and never a reason to re-read HEAD.
  - The process's own earlier object at that name refuses it too, and that is the point: a
    retry never replaces its own late PUT either. No memory of attempted names is kept.
- **`Io`:** the attempt fails, as a failed `put` does today.
- **After 16 refused names** the operation fails with `EngineError::Lost`, having replaced
  nothing.

**A segment claims its name before its sidecars.** `seal` creates the segment first, then each
sidecar with create-if-absent, under the segment's chosen name. HEAD still names nothing until
the commit, after all of them, so a reader still never opens a segment whose sidecar is missing.
- A refused sidecar abandons the name, and `seal` moves to the next: it can only be a stale
  orphan's, since the segment there was this attempt's.
- Today's order would let a process lose the segment's name after creating a centroid table under
  it. Another process's segment would then be read with that table.

**Burial.** Branch and compaction record a key before writing it, so a write that fails
partway is buried (M19). A name **refused** is another process's object, or this one's earlier
attempt. It is removed from that record and never buried. An `Io` name stays recorded, as M19
decided. The fold buries nothing today; what its discarded attempts leave stays the orphan
sweeper's (M6e).

**Comments.** The two that cite "not honoured everywhere" are corrected to cite `require_fencing`.

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
2. **A paused compaction never replaces a live segment.** The same, with two compactions.
3. **A paused fold never replaces a live delete vector.** The same, with two folds deleting
   from one segment.
4. **A branch's delete-vector copy never replaces a live one.** The same, with two branches.
5. **A retry at its own epoch takes the next name.** A fold whose first HEAD CAS answers
   `Contended` commits on its retry, at the `_1` name, and its earlier object is not replaced.
6. **An uncontended operation is unchanged on the wire.** A fold, a compaction and a branch
   each issue the same requests as before, with conditional PUTs in place of PUTs, and commit
   today's key names. Each refused name costs exactly one more write request.
7. **The segment claims its name first.** When A's fold, with a centroid table, is refused the
   name B's centroid-less segment holds, no centroid table exists at B's name, and B's queries
   scan exactly.
8. **A refused name is never buried.** A compaction refused at a name, then abandoned, leaves
   that name out of the graveyard. With the refused name recorded, the test fails.
9. **Bounded.** With every create refused, an operation fails with `EngineError::Lost` after
   16 names, and replaces nothing.
10. **Suffixed names are read like any other.** A `_1` segment and delete vector are resolved by
    `as_of`, reaped by GC once buried past retention, and not mistaken for a replica copy.
11. **Gates.** `./scripts/gates.sh` is green, and the mutation sweep over M23's source diff
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
| 9 | `sixteen_refusals_fail_the_operation` | an unbounded loop (times out); a refusal past 16 retried |
| 10 | `a_suffixed_name_is_resolved_reaped_and_not_a_copy` | `-` in place of `_`; a parser reading `n` as an epoch |

Each is observed red on today's code, or for 6, 9 and 10, which today's code passes, against
the mutation named.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Fold, compaction, branch, uncontended | unchanged: each PUT becomes a conditional PUT | unchanged | unchanged | 0 | unchanged |
| Each refused name | **+1** | 0 | 0 | 0 | +1 write round |

Queries are untouched. HEAD's bytes grow only by the 2+ characters of a suffix, and only after a
refusal.

## Risks

- **A stale sidecar under a name this process wins.** Suppose the orphan sweeper reaps a
  segment but not its sidecar. A later segment created at that name, with no sidecar of its own,
  would then be read with the stale one. Claiming the name first makes this need a sweep that
  splits a segment from its sidecars. Test 7's setup is the place it would show.
- **`Faulty`'s `cas_lost` and `cas_contended` now refuse segment and delete-vector creates.**
  A test injecting them at a high rate around folds could exhaust 16 names. The suite says so;
  the bound is not raised to pass it.
- **A `Contended` that did write.** It is taken as maybe-written, and the next name is used. The
  object it might have left is an orphan, never a replacement.

## Tasks

- **M23.1** — The naming helper. Segment-first `seal`. Every site: fold, `supersede`, compaction,
  branch. The burial rule, the two comments, and tests 1–10.
- **M23.2** — The ledger, `BACKLOG.md` row 44 closed, and the roadmap row.
