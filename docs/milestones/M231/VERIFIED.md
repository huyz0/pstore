# M231 verification

How to run the tests: `cargo test -p pstore-format --test it columnar` and
`cargo test -p pstore-format --lib columnar`. The benchmark: `./scripts/columns.sh` and
`./scripts/columns.sh 100000`.

Red runs: the tests name `pstore_format::columnar` and `with_row_blocks`, which the parent does
not have, so they were red by not compiling. The mutations below show what each one tests.

Mutations were applied one at a time by a script and reverted, against both test modules.
16 of 16 were killed: bit packing off by one; the GCD dropped on decode; the sign flip dropped
on encode; the delta codec choice inverted; deltas decoded unsigned; a dictionary ordinal range
check off by one; presence inverted; the decimal exponent check removed; the decimal exponent
off by one; the decimal round-trip check removed; trailing bytes accepted; column order
unchecked; mixed kinds written as one typed column; lengths that do not cover the bytes
accepted; LZ4 never used; the reader ignoring the version. No `cargo mutants` run is claimed.

1. `columnar_blocks_answer_as_row_blocks_do` (1, 9, 200 and 3,000 rows; `rows_where`,
   `rows_at`, `ids_at`, a projection, and equal zones) and
   `a_columnar_block_decodes_to_the_rows_written` (300 seeded blocks of 1 to 300 rows over
   every kind, gaps and mixed names; all, projected and picked; floats compared bit for bit):
   green. Unit tests: `every_width_round_trips`, `ascending_values_take_the_delta_codec`,
   `redundant_bytes_are_compressed_and_random_ones_are_not`.
2. `a_truncated_or_corrupt_columnar_block_is_refused` (every cut of 20 blocks, a trailing
   byte, every single-byte flip without a panic, an unknown layout), and
   `a_dictionary_ordinal_past_its_entries_is_refused`, `lengths_must_cover_the_bytes_exactly`,
   `a_decimal_exponent_past_fifteen_is_refused`, `attribute_columns_out_of_order_are_refused`:
   green.
3. Fixtures that pin old bytes or versions now write `with_row_blocks()` and keep their
   recorded values: `typed_values.rs` (nine tests), `fields.rs`
   `a_segment_of_one_or_two_fields_is_written_as_before`, and `reader.rs`'s `segment` helper
   (its padding compresses below the suffix read otherwise). The v3 version is asserted in
   `columnar_blocks_answer_as_row_blocks_do`.
4. `./scripts/depth.sh`: green (depth 1 + 1 for sparse and text, bytes equal to their recorded values; "depth and byte gates hold at gate scale").
5. `./scripts/columns.sh`, `provisional` (in-memory store, 1 MiB coalescing gap, one 4-core
   container, medians of 31 runs; times relative within a run). M230 row blocks against M231:

   | | 20,000 rows: rows / columnar | 100,000 rows: rows / columnar |
   |---|---|---|
   | segment bytes | 4,747,027 / 1,553,844 (-67%) | 23,706,997 / 7,366,623 (-69%) |
   | write | 54 / 56-63 ms | 431 / 439 ms |
   | decode every row | 14.49 / 12.56 ms | 98.7 / 76.6 ms |
   | resolve 10 hits | 0.716 / 0.200 ms | 2.55 / 0.90 ms |
   | `category =` (uniform) mask | 4.81 / 2.50 ms | 22.7 / 11.9 ms |
   | `customer =` one batch mask | 0.241 / 0.120 ms | 1.86 / 0.94 ms |
   | `price < 100` mask | 4.39 / 1.73 ms | 20.8 / 8.6 ms |

   Every filter's bytes fell by the same ~3x; no mask or resolve time rose. Adopted by
   criterion 5's rule, as the default of a setting (criterion 6). Re-run after M231.2 at
   20,000 rows: 4,747,027 / 1,547,072 B, decode every row 13.47 / 13.42 ms, resolve 0.736 /
   0.202 ms, write 47 / 72 ms (the write column is the noisiest number the bench prints).
6. `storage_overrides_round_trip_last`, `an_override_wins_over_the_default_only_where_set`
   (`cargo test -p pstore-engine --lib head`), `an_override_wins_over_the_default_for_every_later_seal`,
   `a_cluster_default_of_rows_is_overridden_per_index` (`cargo test -p pstore-engine --test it
   storage`), `the_cluster_default_is_read_from_the_environment`,
   `an_index_overrides_the_cluster_default_and_says_so` (`cargo test -p pstore-server --test it
   storage`), and the OpenAPI module's `request_bodies_name_the_fields_the_server_parses` with
   `StoragePatch` probed: green. 7 of 7 mutations killed: the override ignored at seal; the
   default replaced by `true`; the byte's on/off codes swapped; an unset override written; a
   patch replacing rather than merging; the server's default not passed to engines;
   `PSTORE_BLOCK_LAYOUT=rows` read as columnar.
   CI's `cargo mutants` on #58 then found three more gaps. `set_storage`'s retry guard missed six
   mutations, and now `a_storage_change_that_keeps_contending_reports_contention_not_a_lost_race`
   (`retry_ceiling.rs`) catches them: it checks the error kind and the 24 refusals. `Head::encode`
   missed both `||` mutated to `&&` in its filter of held indexes, and the rejects-only and
   quarantine-only cases in `storage_overrides_round_trip_last` now catch them. For each of these,
   the test was seen red under the mutant. The third gap is `Storage::byte`'s `|` mutated to `^`,
   which is equivalent because the bits are disjoint. It is excluded in `.cargo/mutants.toml`.
   CI's shard 2 never reported: its runner was killed both times. The cause was that `put_var`'s
   `while v >= 0x80` mutated to `<`, and `pack`'s `bits -= 8` mutated to `+=`, push bytes until
   memory runs out. Both loops are now bounded counts: ten bytes at most for a varint, and
   `bits / 8` bytes a value. A scoped `cargo mutants` over `put_var` and `pack` caught 19 of 19.
   The columnar exclusions' line numbers moved with these edits.
7. Pinned to rows with assertions unchanged: `typed_values::a_float_column_prunes_its_blocks`,
   `typed_values::an_int_column_still_prunes_for_either_literal`,
   `datetime::a_datetime_column_prunes_its_blocks`, `patch::a_plain_fold_reads_what_it_did_before_m13`,
   `served_epoch::a_read_only_process_reports_the_epoch_it_served` (each failed under the
   columnar default: pruning ratios of ~48% against a bound of 25%, measured byte counts +11 to
   -15). Columnar held by `columnar_blocks_prune_for_float_and_int_columns` and
   `a_datetime_column_prunes_its_columnar_blocks` (kept blocks' bytes, beyond a filter keeping
   none, at most a quarter of every block's): green.

Downsides, measured: (a) **rollback**: a pre-M231 binary refuses a v3 segment
(`UnsupportedVersion`); `PSTORE_BLOCK_LAYOUT=rows`, or an index's override, is the switch, and
a segment already columnar needs a rewrite (docs/deploy.md). A storage patch costs one HEAD
read and one CAS PUT; an override adds a name and a byte to every HEAD read. (b) **Write CPU**: +0 to
+15% across runs (two codecs tried per column, LZ4). (c) **Tiny blocks**: per-column headers
can exceed what a 2 or 3-row block saves; after varint headers a server fold read 15 bytes
fewer and a 3-row query 11 bytes more than row blocks. (d) **Pruning saves fewer bytes in
absolute terms**: the fixed 8 KiB open now dominates small queries. (e) Decoding is new code
on a corruption path, held by the fuzzing above.

Gates, in a cloud container: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` (the two IPv6 peer tests this container cannot run skipped; nothing else failed), `scripts/depth.sh`, `check-links.sh`, `check-verified.py`, `gen-clients.py --check`, `check-slos.py`, `build-index.py --check`, `check-milestone-ids.py`, `check-poison.sh` and `check-portable.sh` passed, on the M231 and M232 tree together. `cargo llvm-cov -p pstore-format`: `columnar.rs` 95.08% regions, 97.65% lines; `scripts/coverage.sh --changed`: `head.rs` 96.44%, the server's `lib.rs` 97.23% regions. The workspace floor is CI's full run.
