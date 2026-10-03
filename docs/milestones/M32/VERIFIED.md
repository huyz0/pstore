# M32 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0), against
`MemoryStore` behind a store that counts dictionary reads. Every number is a request or byte
count, not a latency.

Commands: `cargo test -p pstore-engine --test dict_skip --test replicate --lib`, and
`-p pstore-server --test patch --test served_epoch`.

⚠️ **A test that fails to compile is not counted as red.** Each criterion names a behavioural
red on the parent, or the hand mutation that was seen to fail it.

1. **No 404 for a dictionary HEAD says is absent.** `vector_only_segments_are_asked_for_no_dictionary`:
   1 text and 1 sparse dictionary read. The parent made 9, 8 of them 404s, measured on a
   probe before the spec. Killed: the open round ignoring either flag.
2. **A sparse query over a mixed index answers.** `a_sparse_query_over_a_mixed_index_answers`.
   The parent refused it with `Corrupt`, a defect spec review found. A leg naming another
   field is still refused. Killed: the exemption dropped.
3. **A dictionary that exists is still read**, and answers are unchanged.
   - `a_dictionary_that_exists_is_still_read`, and `mixed_index_answers_are_unchanged`, whose
     baseline is the same index with every ref forgotten.
   - Killed: the text flag inverted, and the text flag read from the sparse bit.
4. **Unknown asks.** `an_old_head_and_a_resurrected_ref_still_ask`: 9 reads when HEAD forgets,
   and 9 for `as_of` refs resurrected from the graveyard. Killed: unknown read as none.
5. **The section round-trips and refuses malformed input.**
   `the_dicts_section_round_trips_and_refuses_malformed_input`.
   - The "nothing known" HEAD is pinned against the hex the parent's own encoder produced.
   - Killed: the count check (once a case with more flags than segments was added after it
     first survived), the flag range widened, an absent index skipped, and the section
     always written.
   - `an_empty_quarantine_changes_no_byte` passes unchanged.
6. **Every maker records it.** `every_maker_records_its_dictionaries` (fold, a compaction retried
   after a lost CAS, a compaction, a branch) and `a_replica_records_its_sources_dictionaries`.
   Killed: the fold, the compaction and the replica each dropping it.
   - **Equivalent, by hand:** the retry keeping the first seal's flags, since a retry
     re-seals identical rows.
   - The fresh segment's flags are killed by `the_statistics_include_the_unfolded_rows` and
     `a_sparse_leg_reaches_unfolded_rows`.
7. **Gates.**
   - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `441908f` and on this ledger's
     commit.
   - The sweep over M32's source diff (`3ec3084..441908f`): 46 mutants, 35 caught, 7
     unviable, 3 timeouts.
     - Two timeouts replace the compaction's attempt loop whole (pause-based tests).
     - The third negates the merge's emptiness check, and also hangs one.
   - **1 missed**: `|` as `^` in the dictionaries byte, equivalent over disjoint bits. To make the
     criterion's 0 true rather than argued, the byte became a sum (`a89e6b9`), whose four
     mutations were each killed by hand.
   - ⚠️ Not `./scripts/mutants.sh` itself: `cargo mutants --in-diff` was run directly.
   - ⚠️ Amended at implementation: two server tests pin a HEAD read's bytes, and each moves
     by 37 (patch.rs 859 → 896, served_epoch.rs +37 each). No read count moves.

**Residue (code review, minor):**
- a section naming one index twice is accepted, and the last entry wins;
- over an index with no sparse field at all, a misspelled sparse field answers empty.

Spec review took two rounds; the first blocked on the sparse refusal, `as_of`, the fresh
segment and a rolling upgrade. Code review took one round, which passed.
