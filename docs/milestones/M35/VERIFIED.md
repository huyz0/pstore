# M35 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `MemoryStore` and `Accounted`. Every
number is a request count or a test outcome, so none is provisional.

Command: `cargo test -p pstore-engine --test schema`.

⚠️ **A test that fails to compile is not counted as red.** Each criterion names a red on the
parent, or the hand mutation that was seen to fail it.

1. **The door refuses a recorded schema's wrong width.** `a_first_write_learns_the_recorded_schema`:
   a cold engine's width-3 write is refused with `SchemaConflict`, and its width-4 write is
   accepted and folds.
   - Red on the parent: the width-3 write was accepted.
   - Killed: the first read dropped.
   - `the_width_survives_a_flush_and_a_fold` (`tests/dimensions.rs`) asserted the old
     acceptance, and now asserts the refusal.
2. **A refused flush blocks nothing.** `a_refused_flush_blocks_no_later_write`: after the race
   and the refusal, a width-4 write is accepted and the next flush succeeds. After a fold the
   index holds `a` and `right`, and the quarantine holds `w1` and `w2`.
   - Red on the parent: `DimensionMismatch { expected: 3, got: 4 }` on the correct write.
   - Its tail pins the waiver as spent once a bundle lands.
   - Killed: the waiver dropped, the door's fallback kept, and the waiver never cleared.
   - `a_stale_row_of_another_metric_refuses_no_write` does the same for a metric (code review).
3. **One waiver, every index, until a bundle lands.** `one_waiver_covers_every_refused_index`:
   one refusal naming `a`; a flush failing on an injected `Io` returns `Blob`; the retry writes
   both. Both rows are quarantined.
   - Red on the parent: the second flush refused.
   - Killed: a waiver for the named index only, and a waiver cleared by any flush.
   - ⚠️ **Never a text-field conflict** (code review): `a_waiver_never_covers_a_text_field`.
     Killed: the waiver covering it, which wrote the row unquarantined under a foreign fold.
4. **The read is once per engine.** `the_first_write_reads_head_once`: 1 HEAD read on the first
   write, 0 over the next 10, and more than 0 on the first flush (its resume).
   - Red on the parent: 0 reads on the first write.
   - Killed: a read on every write.
   - `a_failed_first_read_refuses_no_write` (found in implementation, from the server's chaos
     suite): a read fault refuses no write, and the next write retries the read. Killed: the
     fault propagated.
   - The server's pins move by the spec's RA budget: a batched first write 0 → 1 read, a durable
     first write 10 → 11. A second batched write is now asserted at 0 in every class.
5. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on M35.1, and on this ledger's commit.
   - The sweep over M35's source diff: 12 mutants over `6a281a4..6e5488c`, 9 caught, 1 unviable, 2 timed out (the flush answering `Ok` without writing leaves a test waiting on a bundle that never lands), **0 missed**.
   - Hand mutations: 10, all killed.

**Residue (recorded, BACKLOG row 54):**
- A text-field conflict accepted before the schema was known still refuses every flush of that
  engine, on every index, until a restart: as before M35.
- A stale cached schema after a drop and re-create can admit a row the fold then quarantines
  (code review, minor).
- `quarantinable` names the text-field conflict by its message; `a_waiver_never_covers_a_text_field`
  is what catches a rewording.
