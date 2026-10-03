# M45 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `MemoryStore` and test stores built
over it. Every figure is a count of requests or bytes, none a timing, so none is provisional.
**No Azure or Azurite was involved**: criterion 1's suffix-refusing store stands in for the
behaviour C-14 measured, and the Azure adapter is still row 32's.

Commands: `cargo test -p pstore-engine --test lengths`, `cargo test -p pstore-engine --lib head`,
`cargo test -p pstore-format --lib reader`.

⚠️ **A test that fails to compile is not counted as red.** These tests name a new field, so
each criterion names the hand mutation seen to fail it.

1. **No suffix read where the length is known.**
   `every_open_reads_a_range_when_head_knows_the_length`: over a store whose suffix reads fail,
   the run makes 0 suffix reads. It covers write, fold, the dense, text and sparse legs, an
   ordered query, a filtered aggregate, a scan, two warms, a compaction, and a replica made and
   queried. The same run over the plain store answers the same.
   - Killed by hand: `segment_targets` passing no length; the replica copying no length.
2. **The same requests.** `a_known_length_costs_what_the_suffix_cost`: a per-key counting store
   records the same reads and the same bytes for every key but HEAD's, with lengths as without.
   Killed by hand: `segment_targets` passing no length.
3. **Unknown still works.** `a_head_without_lengths_reads_by_suffix`: with every length
   stripped, the same answers come by suffix reads, and the next fold records a length for its
   one new segment only.
4. **The section round-trips and refuses malformed input.**
   `the_lengths_section_round_trips_and_refuses_malformed_input`:
   - lengths 1, 127, 128, 2^32 and `u64::MAX` round-trip, and so does 0 as unknown;
   - refused: a truncated varint, an 11-byte one, one past `u64`, a non-shortest one, a wrong
     count, and an unknown index;
   - with no length known, the encoding is byte for byte M32's.
   - Added in code review: lengths with no dictionaries known still force that section. Killed
     by hand: the forcing removed.
5. **HEAD grows by the varint and nothing more.** `lengths_cost_three_bytes_a_megabyte_segment`:
   100 one-MiB segments add exactly 4 + (4 + 3) + 4 + 300 bytes. Killed by hand: every known
   length written as 2^40.
6. **A wrong length is refused.** `a_recorded_length_is_checked_against_the_footer`, on
   `MemoryStore`:
   - one byte short is `Corrupt`;
   - one byte long is the store's error;
   - over `Broken` with `ShortReadsPastTheEnd`, one byte long is exactly
     `Corrupt("recorded length disagrees")`;
   - a 2-row segment shorter than 8 KiB opens from offset 0.
   - Killed by hand: the length check removed.
7. **Every existing test passes, unchanged except for the new field and two byte pins.**
   `./scripts/gates.sh`: green by the pre-commit hook on M45.1. `cargo test --workspace` was
   red at first only on `patch.rs` and `served_epoch.rs`, each by exactly 18 bytes (the
   section for one index `docs` with one segment), and they were moved with that arithmetic in
   their comments. Their read counts are unchanged. `./scripts/depth.sh`: "ok: depth and byte
   gates hold at gate scale".
8. **Mutation.** `cargo mutants --in-diff` over M45's source diff (`e4153e8..`, staged before
   the code-review test), testing `pstore-engine`, `pstore-format` and `pstore-query`: 63
   mutants, 46 caught, 14 unviable, 3 timeouts, **0 missed**. The timeouts are
   the compaction-attempts function replaced whole or its retry test negated, and they hang the retry loops
   that call it. The forcing mutant (`||` as `&&` in `encode`) was caught before the
   code-review test was added, and that test also kills it by hand.

**Not covered by any test, said so (code review):** a compaction retry's length comes from its
re-seal, but the bytes do not depend on the key, so the abandoned attempt's length would be the
same and no test can tell them apart. The fresh segment's length is set on a store the engine
owns and no test observes.

**Residue for row 32:** the Azure adapter. `as_of` refs, HEADs from before M45, and HEADs an
older node rewrote still read by suffix, and on Azure those still fail.
