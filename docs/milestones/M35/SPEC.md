# M35 — A wrong row is refused at the door, and never blocks a lane

**Serves:** [BACKLOG](../BACKLOG.md) row 27, which [M7d](../M7d/VERIFIED.md) narrowed. A
write-only process that has never read HEAD accepts a wrong width at its door and is refused at
its flush. Measured while planning this milestone, that refusal is worse than "one API call
later": it blocks every write the process makes to that index, until it restarts.

## What is true today

- **The door** (`Engine::write_*`) checks a batch against the schema this process last read,
  else against the rows it holds unfolded, else against the batch's first row. A process that
  has never read HEAD has no schema to compare against.
- **The flush** reads HEAD once per process (`schemas_unseen`) and refuses a pending batch that
  contradicts a recorded schema (`SchemaConflict`). The refused rows **stay in `pending`**.
- **Measured** (a probe that was not committed). One engine folds an index of width 4. A second
  engine, which has never read HEAD:
  1. writes a width-3 row: accepted;
  2. writes a width-4 row: **refused** (`DimensionMismatch { expected: 3 }`), because the door
     now compares against the pending wrong row;
  3. flushes: `SchemaConflict`; the wrong row is still pending;
  4. writes again (width 4): refused; flushes: refused.
  - That index is blocked on that engine until a restart, and every later correct write to it
    is refused.
  - In the server a `batched` write answered 200 at step 1. M7d's own rule says a contradicting
    row must never stop later work ("drop and count, never stop", tests/schema.rs). This stops
    it.

## Delta

**Nothing is removed from `pending`** (spec review, round 1). Removal broke M17's uncertain-bundle
accounting, M9c.1's fresh-view generation, and M7d's "drop and count". Instead:
1. **A process's first write reads HEAD, once.** `refuse_replica`, which every write path calls
   first, reads HEAD when the index is a known replica, **or** when the process has never
   read one. So the door knows every recorded schema, and the replica check sees what the
   read learned.
2. **A cached schema is authoritative at the door.** When this process holds one for the index,
   the door checks a batch against it alone, and no longer falls back to the width of rows it
   holds unfolded. A stale wrong row then refuses no correct write.
3. **A flush refuses a schema conflict once, then waives.** The refusal is returned as today. The
   next flush skips the schema check, so the rows accepted before the schema was known become
   durable, and the fold's reject pass sets them aside in the index's quarantine (M25). There
   they are counted and exportable, never dropped.
   - Any wrong row written after the refusal meets (2) at the door, since the schema is then
     known.
   - **The waiver is one flag** (spec review, round 2). A refusal sets it, and only a flush
     that writes its bundle clears it. A flush that fails (`Io`, `Contended`) keeps it for the
     retry. It skips the schema check for **every** index in that flush, not only the one the
     refusal named: `find_map` reports the first conflict and stops.
   - Two comments become false and are corrected: `flush_inner`'s "Nothing wrong may become
     durable", and the schema test's "A row that never becomes durable can never reach a
     fold".

**Not changed:** the fold's reject pass and quarantine; M17's uncertain bundles; the fresh view;
the flush's own HEAD read and its fresh-watermark resume (M9j); every request after a
process's first write.

## Acceptance criteria

1. **The door refuses a recorded schema's wrong width.** The measured sequence: step 1's write is
   refused with `SchemaConflict` (parent: accepted), and step 2's is accepted and becomes
   durable.
2. **A refused flush blocks nothing.** The race: an engine whose first write read HEAD with no
   schema for the index buffers two width-3 rows; another process then folds the index at
   width 4. The flush refuses. A width-4 write is then accepted, and the next flush succeeds.
   After a fold, the width-4 row is in the index and both width-3 rows are in its quarantine.
   Parent: refused at every write and flush.
3. **One waiver, every index, until a bundle lands.** As 2, with a wrong row in each of two
   indexes: one refusal (naming one), then a flush that fails on an injected `Io` and keeps the
   waiver, then one that writes both. Both are quarantined.
4. **The read is once per engine.** A fresh engine's first write costs 1 HEAD read and its next
   10 writes 0; its first flush still costs its resume's read (counted on `Accounted`).
5. **Gates.** `./scripts/gates.sh` is green, and the sweep over M35's source diff misses 0.

## Test plan

In `crates/pstore-engine/tests/schema.rs`.

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1 | `a_first_write_learns_the_recorded_schema` | accepted on the parent; the first read dropped |
| 2 | `a_refused_flush_blocks_no_later_write` | refused forever on the parent; the waiver dropped; the door's fallback kept |
| 3 | `one_waiver_covers_every_refused_index` | a waiver for the named index only; a waiver cleared by a failed flush |
| — | `a_text_field_that_contradicts_the_schema_is_refused` (existing) | ⚠️ **amended, not weakened**: its write is now refused at the door (the first read knows the schema), where it was accepted and refused at the flush |
| 4 | `the_first_write_reads_head_once` | a read per write; the `schemas_unseen` guard dropped |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| An engine's first write | 0 | +1 (HEAD) | 0 | 0 | +1, once per engine build |
| Every later write | 0 | 0 | 0 | 0 | unchanged |
| Its first flush | 0 | unchanged: 1, its resume's fresh watermark (M9j) | 0 | 0 | unchanged |

**Added, not moved** (spec review M2): +1 GET per engine build, and since M26 an evicted engine is
rebuilt.

## Risks

- **A row refused at a flush becomes durable at the next**, and the fold quarantines it. The flush
  that refused it returned the error. That error reaches a `durable` caller, and the server logs
  a `batched` one's background flush. The quarantine is how the row stays visible rather than
  lost, against M7d's "nothing wrong may become durable": that rule holds at the door, which
  now sees HEAD, and the flush only waives what the door could not have seen.
- **A first write can fail on a store error**, or wait a round trip, where it buffered for free.
  Two first writes in parallel both read HEAD, which is harmless.
- **A drop and re-create** still recovers through M9f.2's re-read at the door. The first read
  remembers schemas only and prunes nothing; the flush prunes as it does today.

## Tasks

- **M35.1** — The first read, the door's authority, the flush's waiver, and tests 1–4.
- **M35.2** — The ledger, `BACKLOG.md` row 27 closed, and the roadmap row.
