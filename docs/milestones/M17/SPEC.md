# M17 — Two writers on one lane fail loudly

**Serves:** [BACKLOG](../BACKLOG.md) row 24, and OQ-91's density invariant
([C-2](../../research/00-plan/open-questions.md)), which the fix must not break.

## What is true today

- A lane is single-writer and **dense**, and a bundle is an unconditional `put`.
- M9j resumes a restarted writer at its lane's tail, so a lone restart no longer collides.
- Two **live** processes on one lane still do, and nothing notices:
  - they reach the same sequence number;
  - the second `put` overwrites the first;
  - that bundle's acknowledged `durable` rows are gone, with no error anywhere.
- `PSTORE_LANE` makes the lane an operator's choice. It does not make a collision an error.

## Delta

**A bundle is created, never replaced.** `flush` writes it with
`put_conditional(key, body, Precondition::NotExists)`. The engine already refuses a backend
whose `create_if_absent` is not `Supported` (`require_fencing`), so every backend it runs on
can honour this. A failed write still never consumes a sequence (OQ-91).

**What a first attempt's outcome means:**

- **Created:** as today. The rows move to `durable`, and the next sequence is taken.
- **`Lost`** (an object is already there): this process has never attempted that sequence, so
  another process writes this lane. The flush fails with `EngineError::LaneTaken { lane, seq }`
  and consumes nothing. It makes no read.
- **`Contended`** (the backend could not evaluate the condition): nothing is written or
  consumed. The flush fails with `EngineError::Contended`, which the API already answers
  **409**. The rows stay pending for the next flush, at the same sequence.
- **`Io`**: the write may have landed, and the flush resolves that before returning (below).

**Resolving an uncertain write.** The engine records the attempt: its sequence, its exact
bytes, and how many rows of each index it took from the front of `pending`. While a record
exists, it is resolved before anything else is written, under the flush lock:

- **One GET of the bundle, in the same flush.**
  - **Present, and byte-for-byte the recorded bytes:** it landed. The recorded rows move to
    `durable`, and the sequence is taken. The comparison is on bytes, never decoded rows.
  - **Present, with other bytes:** another process wrote it, so the flush answers `LaneTaken`.
  - **Absent:** HEAD is read for this lane's watermark.
    - **At or below the sequence:** nothing landed. The record is dropped, and the flush writes
      the current `pending` there as a first attempt. A flush makes **at most one** first
      attempt: one that finds its own `Io` did not land fails with that `Io`.
    - **Past it:** something landed there, was folded and has been reaped, and whose it was
      cannot be known. The flush answers `LaneTaken`, rather than guess.
  - **The GET fails:** the record stays, and the flush fails.
- **A drop of an index**, which removes that index's pending rows, also removes that index's
  count from the record. The rows it counted are gone either way.
- The API's first flush after a lost acknowledgement therefore succeeds. A client never sees
  one unless the read that resolves it also fails.

**Any HEAD read also checks the lane.** Every HEAD this engine reads is already passed to
`prune_to`. If its watermark for this lane is past the next sequence this engine would write,
and no record accounts for it, another process has written this lane. Every later flush
answers `LaneTaken` without a request. This covers the case where the other writer's bundle
was folded and reaped before this process flushed again, so creating at a free key below
the watermark would lose rows.

**`LaneTaken` is loud and stays loud.** It consumes nothing, and the rows stay pending and
visible to this process's reads. The API answers **500** with code `lane_taken`, and its
message names the lane and `PSTORE_LANE`. The operator restarts the process: M9j's resume
starts it past the other writer.

**Does not change:** a bundle's format and key; the fold's recovery probe; M9j's resume; the
success path's cost; `batched` writes, which were never promised to survive a process.

## Acceptance criteria

1. **A second writer never erases a first's durable writes.**
   - Engines A and B share one tenant, lane and store. A flushes 0; B resumes and flushes 1.
   - A's next flush fails with `LaneTaken { lane, seq: 1 }`, and so does the one after it.
   - A's rows stay visible to A, and B's bundle is byte-for-byte unchanged.
   - A fold serves every row either acknowledged as durable.
2. **A HEAD read finds a lane taken, even after GC.** A flushes 0. B flushes 1 and 2, then a
   fold and `gc(0)` reap them. A queries, then writes. A's flush fails with `LaneTaken` and
   issues no PUT.
3. **A lost acknowledgement costs one GET, and nothing else.** The store lands sequence 1 and
   answers `Io`. That flush returns `Some(1)` after 1 PUT and 1 GET. The next writes 2. A fold
   serves each row once, and a new process resumes past 2.
4. **An unresolved write is resolved first.**
   - The acknowledgement is lost and the GET fails, so the flush fails. Then index `X` is
     dropped, and more rows are written to `Y` and to a new `X`.
   - The next flush resolves the landed bundle and writes the rest at the next sequence.
   - A fold serves `Y`'s rows each once, the new `X`'s rows, and none of the dropped `X`'s.
5. **A write that never landed is written again, at the same sequence.** The store fails
   before writing, and then fails the GET. The next flush reads the bundle as absent and the
   watermark as below it, and writes at that sequence. The lane stays dense.
6. **Absent past the watermark is `LaneTaken`**, not a guess. An uncertain record's bundle
   is folded and reaped before its resolution.
7. **`Contended` consumes nothing.** The flush fails with `Contended`, the next writes at the
   same sequence, and the API answers 409.
8. **The API names it:** a durable write meeting `LaneTaken` answers `500 lane_taken`, naming
   the lane and `PSTORE_LANE`.
9. **Cost:** a flush that creates its bundle issues exactly 1 PUT and no read, and `LaneTaken`
   from a collision issues 1 PUT and no read.
10. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | B's rows lost (today's overwrite) | an unconditional `put`; `Lost` read as this process's own; a refusal that consumes the sequence |
| 2 | A creates below the watermark | the HEAD check removed, or comparing `>=` |
| 3 | the flush answers `Io` | no resolution; a comparison that is not byte-exact; the sequence not taken |
| 4 | the dropped rows return, or the new `X`'s are lost | a drop that leaves its count in the record; a resolution after the write |
| 5 | a gap in the lane | an absent bundle taken as landed |
| 6 | the rows are taken as landed | the watermark check removed |
| 7 | `Contended` is not produced | `Contended` read as `Lost` or as `Io` |
| 8 | no `lane_taken` code | the mapping dropped |
| 9 | as 1 | a read on the success path, or on `Lost` |

Every existing test of lanes, the fold's recovery and OQ-91's scenario passes unchanged.

## RA budget

A successful flush is unchanged: **1 W**, depth 1. A collision is 1 W. A lost acknowledgement
adds 1 GET, which is depth 2. Resolving an unresolved write adds 1 GET, and 1 HEAD read when the
bundle is absent. All of this is on the write path only, and only after a failure.

## Risks

- **A process that only writes stays blind until it flushes.** Say its lane is written by
  another process, folded and reaped past retention, and it reads no HEAD in all that time.
  Its next flush then creates below the watermark, and the rows are never folded. The window
  needs a fold, a GC past retention and no HEAD read, all between two of its flushes. It is
  stated, not closed, and it stays open in BACKLOG row 24.
- **The resolution trusts bytes.** Another process writing byte-identical bytes at the same
  sequence is taken as this one. The rows are identical, so nothing is lost. A repeated patch
  or condition applies once, not twice, which changes nothing.
- A backend that says `Supported` and ignores the condition would overwrite as before
  (`fake-gcs-server`, OQ-153). `require_fencing` trusts the capability record, as it does
  for HEAD.

## Tasks

- **M17.1** — conditional bundles, the uncertain-write record and its resolution, the HEAD check, `LaneTaken`, and the API code.
