# M45 — HEAD records each segment's length; an open reads an absolute range

**Serves:** [BACKLOG](../BACKLOG.md) row 32's precondition. It takes exit 2 of
[C-14](../../research/02-object-storage/request-efficiency-patterns.md): "carrying object length
in the manifest (which already names every segment)". It does **not** add the Azure backend.

## Why this is decided now, without M0b's account

Row 32 says "choosing among them is a measurement … and needs M0b's real account". C-14, the
banner the row paraphrases, says "a design decision with its own spec", and asks for no
measurement. Where a row and a banner disagree, the banner wins. A design review held before
this spec judged that the account would measure nothing this choice depends on:

- All three exits use the same absolute range GET. Azurite answers it (`bytes=0-99 -> 206`,
  C-14, provisional).
- **Exit 3** (head, then range) adds a sequential round trip on every cold open, whatever
  Azure's latency is. D-34 prices the round trip, so it loses on any measurement.
- **Exit 1** (a footer at a known absolute offset) makes the same requests as exit 2, plus a
  segment format change.
- **Exit 2's** only cost is bytes in HEAD, and that is measured here, locally.

So row 32 gains a correction banner saying this. ⚠️ **Exit 2 alone does not make Azure
work** (spec review): `as_of` refs, HEADs from before M45, and HEADs an older node rewrote
still read by suffix. The Azure adapter, and what it does with those, stays open on row 32.

Two honest qualifications (spec review):
- "The banner wins" is AGENTS.md's rule within one document. Applying it between a backlog
  row and the corpus banner it cites stretches that rule. It is applied here because the row
  claims to restate C-14 and does not.
- Exit 1 is, if anything, worse than "the same requests": a header at offset 0 puts the index
  read behind it, one more sequential round.

## What is true today

- Every segment open is `Segment::open(store, key)`, which reads the last `SUFFIX_FETCH`
  (8 KiB) bytes with `get_suffix_as` (`pstore-format/src/reader.rs`). A segment whose index
  section is larger reads it with a second, absolute range.
- Its callers hold a HEAD `SegmentRef` (engine `prepare`, `compact_attempts`, `scan`, `warm`,
  and `segment_targets` for `pstore_query::Target`), or a fresh `mem/` segment the engine
  just PUT. In every one of them the length is knowable without a request.
- `seal` PUTs the segment's bytes, so it holds the length and drops it.
- Sidecars (dictionaries, centroids, deletes) are whole-object `get_immutable` reads. Only the
  segment open is a suffix read.

## Delta

1. **`SegmentRef` gains `len: Option<u64>`, the segment object's byte length.** `None` means
   unknown: a HEAD from before M45, a ref reconstructed for `as_of`, or a HEAD rewritten by a
   pre-M45 node.
   - `seal` returns the length it PUT.
   - The fold and compaction record it, and a replica copies its source ref's.
   - ⚠️ A compaction retry re-seals at a new key, and its length is that re-seal's, never the
     abandoned attempt's (spec review).
2. **HEAD carries it in an optional trailing section, after M32's dictionaries.** For each
   index in name order: its name, its segment count, and each length as an unsigned LEB128
   varint, with **0 meaning unknown** (no segment is shorter than its footer).
   - The section is written whenever any length is known. The dictionaries section is then
     written too, all `0xFF` if none is known, by M15.2's earlier-section rule.
   - The decoder refuses an index HEAD does not name, a count that disagrees, or a varint
     that runs past the buffer, past 10 bytes, overflows `u64`, or is not in its shortest
     form (so a decode and re-encode is byte for byte).
   - A pre-M45 decoder stops after the dictionaries and ignores the rest (`decode` returns
     there). So it reads a new HEAD and re-encodes it without lengths, and those refs fall
     back to the suffix read.
3. **`Segment::open_at(store, key, len: Option<u64>)`.**
   - With `Some(len)`, the first read is `get_range_as(key, len.saturating_sub(SUFFIX_FETCH)..len,
     Meta)`: the same bytes the suffix read returns, by absolute offsets.
   - With `None`, it is today's suffix read. `Segment::open(store, key)` is `open_at(…, None)`.
   - **A recorded length the footer disagrees with is refused**
     (`FormatError::Corrupt("recorded length disagrees")`), never read past.
4. **`pstore_query::Target` gains `segment_len: Option<u64>`**, filled from the ref. Every
   non-test open in the engine and in `pstore-query` passes the length it holds. A fresh
   segment passes its own length.
   - **Left on the suffix read, said so** (spec review): `pstore-index`'s `SparseIndex::open`,
     `TextIndex::open` and `VecIndex::open`/`warm`, reached only from examples and tests;
     `segment_rows_for_test`; `examples/id_round.rs`. The examples' `SegmentRef` and
     `Target` literals gain the field.
   - The server's `Backend` doc comment, which says every open is a suffix read, is corrected.

**Not changed:**
- the number and depth of requests;
- the bytes fetched;
- the segment format;
- the read cache's behaviour. Its key for the open becomes a `Range` rather than a `Suffix`,
  so the first open after the upgrade misses once.
- `as_of` refs still read by suffix.

## Acceptance criteria

1. **No suffix read where the length is known.** An engine over a store whose
   `get_suffix`/`get_suffix_as` fail does all of the following without one suffix read:
   - writes, folds, and queries a dense, a text and a sparse leg;
   - an ordered query;
   - selects, aggregates and scans;
   - compacts;
   - warms;
   - replicates an index to another tenant, and then queries the replica. The query
     succeeding over the failing-suffix store is the assertion that the replica's refs carry
     their source's lengths.

   The same run over the plain store gives the same results. Test: `pstore-engine`
   `tests/lengths.rs::every_open_reads_a_range_when_head_knows_the_length`.
2. **The same requests.** For the same writes and query, a per-key counting store the test
   adds (`Accounted` counts by tenant and class, not by key) records
   the same number of reads, and the same bytes for every key but HEAD's, with lengths as with
   lengths stripped from HEAD. HEAD's bytes differ by its section, and nothing else differs.
   Criterion 1 is what shows the reads are ranges; this counting store cannot tell a suffix
   from a range. Test: `tests/lengths.rs::a_known_length_costs_what_the_suffix_cost`.
3. **Unknown still works.** A HEAD with every length stripped, as a pre-M45 node writes it,
   answers the same query by suffix reads. The next fold records lengths only for the
   segments it seals. Test: `tests/lengths.rs::a_head_without_lengths_reads_by_suffix`.
4. **The section round-trips and refuses malformed input.**
   - Lengths 0 (unknown), 1, 127, 128, 2^32, and `u64::MAX` round-trip.
   - The decoder refuses a truncated varint, an 11-byte varint, a 10-byte varint whose last
     byte is above 1, a non-shortest one (`0x80 0x00`), a wrong count, and an unknown index.
   - A HEAD with no length known encodes byte for byte as before M45.
   - Test: `pstore-engine` `head::tests::the_lengths_section_round_trips_and_refuses_malformed_input`.
5. **HEAD grows by the varint and nothing more.** For a HEAD of one index with 100 segments of
   1 MiB each, **every segment's dictionaries known**, the section adds
   4 + (4 + name) + 4 + 3 × 100 bytes over the same HEAD without it. Test:
   `head::tests::lengths_cost_three_bytes_a_megabyte_segment`.
6. **A wrong length is refused.** On `MemoryStore`:
   - `open_at` with a recorded length one byte short fails `FormatError::Corrupt`;
   - one byte long fails with the store's out-of-range error, never a segment;
   - one byte long over `pstore-testkit`'s `Broken` with `Defect::ShortReadsPastTheEnd`, which
     answers a range past the end with a short body as S3 does, fails with exactly
     `Corrupt("recorded length disagrees")`. That is the case production meets, and the only
     one that reaches the check (spec review);
   - a segment shorter than `SUFFIX_FETCH` opens with a known length, reading from offset 0.
   - Test: `pstore-format` `reader::tests::a_recorded_length_is_checked_against_the_footer`.
7. **Every existing test passes, unchanged except for the new field** in the struct literals.
   This includes the server's cost and depth tests, which count requests. `./scripts/gates.sh`,
   and `./scripts/depth.sh`.
8. **Mutation:** the incremental sweep of the changed lines misses 0. `./scripts/mutants.sh`.
