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
can honour this.

**What each outcome means**, keeping the rule that a failed write never consumes a sequence:

- **Created:** as today. The rows move to `durable`, and the next sequence is taken.
- **`Contended`** (the backend could not evaluate the condition): nothing is consumed. The flush
  fails with `EngineError::Contended`, and its rows stay pending for the next flush, at the same
  sequence.
- **`Io`:** as today. Nothing is consumed. The write may have landed, and the next case
  handles that.
- **`Lost`** (an object is already there): the flush reads it back with one GET.
  - **This process's own bundle, whose acknowledgement was lost.** It holds, for each index
    it names, exactly the first rows this flush's snapshot holds for that index, and at least
    one row. Those rows are durable: they move to `durable`, and the sequence is taken. Any
    remaining rows are written at the next sequence in the same call, by the same rules.
  - **Anything else.** Another process writes this lane. The flush fails with
    `EngineError::LaneTaken { lane, seq }`, consuming nothing. Every later flush fails the same
    way while the other writer holds the lane. The server answers **500**, with code
    `lane_taken` and a message naming the lane and `PSTORE_LANE`.
- A bundle identical to a prefix of this process's rows is treated as this process's own,
  whoever wrote it. That is safe: identical rows are durable either way.

**Does not change:**
- a bundle's format, its key, or the fold's recovery probe;
- M9j's resume;
- the success path's cost;
- `batched` writes, which were never promised to survive a process.

## Acceptance criteria

1. **A second writer never erases a first's durable writes.**
   - Engines A and B share one tenant, lane and store.
   - A flushes. Then B, which resumes past A, flushes. Then A flushes at the sequence B took.
   - A's flush fails with `LaneTaken` naming that lane and sequence.
   - A fold then serves every row both acknowledged as durable, and B's bundle is unchanged.
2. **Loud, and it stays loud.** A's next flush fails the same way, and its unflushed rows stay
   visible to A's own reads.
3. **A lost acknowledgement is not a second writer.**
   - A store lands a bundle and then returns `Io` for it. The flush fails, and more rows are
     written.
   - The next flush succeeds. The fold serves every row exactly once.
   - The lane is dense: the sequences written are 0, 1 and 2, with no gap. A new process
     resuming the lane finds all three.
4. **`Contended` consumes nothing.** A store answers one bundle write `Contended`. The flush
   fails with `Contended`, and the next flush writes at the same sequence.
5. **The API says so:** a durable write that meets `LaneTaken` answers `500 lane_taken`, and the
   message names the lane and `PSTORE_LANE`.
6. **Cost:** a flush that creates its bundle issues exactly 1 PUT and no read.
   `LaneTaken` costs 1 PUT and 1 GET.
7. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | B's rows are lost (today's overwrite) | an unconditional `put`; `Lost` read as this process's own bundle |
| 2 | as 1 | a refusal that consumes the sequence |
| 3 | the flush after the lost acknowledgement fails | the prefix check refusing its own bundle; the remainder not written; the sequence not taken |
| 4 | `Contended` is not produced | `Contended` read as `Lost` |
| 5 | no `lane_taken` code | the mapping dropped |
| 6 | as 1 | a read on the success path |

Every existing test of the lane, the fold's recovery and OQ-91's scenario keeps passing
unchanged.

## RA budget

A successful flush is unchanged: **1 W**, depth 1. A collision or a lost acknowledgement adds
1 GET. A lost acknowledgement with rows left over adds 1 more W. That is depth 3, on the write
path only, and only when something went wrong.

## Risks

- **A false "own bundle".** Another process's bundle equal to a prefix of this process's rows
  is taken as this process's. It holds the same rows, so nothing is lost.
- **A lane taken forever.** If the other process stops but its bundles stay, M9j's resume
  cannot help a process that already holds a sequence. The operator restarts it, and the new
  process resumes past the collision.
- A backend that says `Supported` and ignores the condition (`fake-gcs-server`, OQ-153) would
  overwrite as before. `require_fencing` trusts the capability record, as it does for HEAD.

## Tasks

- **M17.1** — conditional bundles, the lost-acknowledgement check, `LaneTaken`, and the API code.
