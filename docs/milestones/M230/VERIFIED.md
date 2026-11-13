# M230 verification

How to run the tests: `cargo test -p pstore-format --test it equality` and
`cargo test -p pstore-query --test it equality_pruning`. The benchmark: `scripts/columns.sh`
(and `scripts/columns.sh 100000`).

Red runs: on the parent's `filter.rs` (the section written, never consulted)
`equality_prunes_blocks_without_the_literal` failed (kept all 8 blocks for `customer = c3`), and
`equality_pruning_never_drops_a_matching_block` failed its non-vacuity floor (326 blocks
pruned by zone maps alone against 7,017 with the section; the floor is 3,000). The
`pstore-format` tests name an API the parent does not have, so they were red by not compiling;
the mutations below are what shows each one tests something.

Mutations were applied one at a time by a script and reverted, against both test modules.
11 of 11 were killed: the open bit ignored; datetime, uuid and bytes rows leaving a block
closed; bool bits swapped at build; string probes ignored; the saturation cut removed; the
selectivity cut removed; a string order comparison pruned by the section; a bool order
comparison pruned; string equality never consulting the section; the writer's spare ignoring
the sketch and the section's own directory entry; the fit check loosened by 64 bytes. No
`cargo mutants` run is claimed here; CI runs it on the pull request.

1. `equality_prunes_blocks_without_the_literal`: green after. `Eq` and `In` on strings and
   `Eq` on bools keep exactly the blocks holding the literal; `And`, `Or`, `Not` compose.
2. `equality_pruning_never_drops_a_matching_block`: green after, 60 segments x 80 random
   predicates (`Eq`, `In`, `Lt`, `Gte` under `And`, `Or`, `Not`) over strings, bools,
   datetimes, uuids, bytes, numbers and arrays, with string spellings of the datetimes and
   uuids. Killed "open bit ignored", "datetime rows leave a block closed" and both order
   comparison mutations.
3. `a_segment_without_the_equality_section_answers_as_before` and
   `the_equality_section_never_narrows_the_trigram_sketch`: green after, at 10 to 20,000 rows.
   Existing byte fixtures: `a_typed_segment_without_datetimes_keeps_its_bytes` and
   `a_dated_segment_without_uint64s_keeps_its_bytes` pin bytes recorded before M9h.3 and M172;
   their segments now earn an equality section, so both write with
   `without_equality_section_for_test()` and keep their recorded hashes unchanged.
4. `many_string_attributes_never_change_the_blocks`,
   `a_filter_that_would_be_mostly_ones_is_not_written` and
   `the_section_never_costs_the_open_a_second_read` (every budget 100 to 1,500 bytes): green.
   `scripts/depth.sh`: see the gates line.
5. `./scripts/columns.sh`, `provisional` (in-memory store with the object-store backend's 1 MiB
   coalescing gap, one 4-core container, median of 31 runs; times relative within one run):

   | 20,000 rows, 40 blocks of 500 | M229 blocks / GETs / bytes / ms | M230 blocks / GETs / bytes / ms |
   |---|---|---|
   | `customer =` one batch (clustered) | 40 / 1 / 4,580,160 / 4.81 | 2 / 1 / 234,504 / 0.24 |
   | `customer in` 3 batches | 40 / 1 / 4,580,160 / 4.88 | 3 / 2 / 1,289,772 / 0.38 |
   | `customer =` absent value | 40 / 1 / 4,580,160 / 4.77 | 0 / 0 / 0 / 0.00 |
   | `archived = true` (clustered) | 40 / 1 / 4,580,160 / 4.34 | 4 / 1 / 469,008 / 0.44 |
   | `category =` (uniform, 40 values) | 40 / 1 / 4,580,160 / 4.78 | 40 / 1 / 4,580,160 / 4.77 |
   | `sku =` (unique, random) | 40 / 1 / 4,580,160 / 4.75 | 40 / 1 / 4,580,160 / 4.72 |
   | `promo = true` (1%, random) | 40 / 1 / 4,580,160 / 4.39 | 39 / 1 / 4,572,828 / 4.35 |

   At 100,000 rows (49 blocks of 2,040, 256-bit filters): one batch 49 to 4 blocks
   (22.9 MB to 1.9 MB, 25.1 to 1.9 ms); an absent value 49 to 6 blocks; `archived` 49 to 5.
   Segment 4,744,375 to 4,747,044 B (+0.06%); meta region 4,173 to 6,825 B, inside the
   8 KiB suffix the open already reads; write 51 to 54 ms. Unprunable filters' mask time
   unchanged within noise (4.78 / 4.77 ms).

Downsides, measured: (a) **more GETs** when kept blocks are not neighbours: `customer in` 3
batches went from 1 coalesced GET of 4.58 MB to 2 GETs of 1.29 MB, and at 100,000 rows one
batch from 1 GET to 4 (its false positives are scattered). A GET is priced, bytes in-region
are not, so a tenant paying per request pays up to one GET per kept run; the bytes and decode
saved (19x and 20x here) dominate latency. (b) **Budget**: filters narrow as blocks grow
(512 bits at 20,000 rows, 256 at 100,000), and an attribute with ~1,000 distinct values a block
gets none. (c) **Memory**: each open segment holds its section, here 2,652 B. (d) Write CPU:
within noise. (e) Uniform and random-rare values gain nothing, as predicted.

Gates, in a cloud container: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `scripts/depth.sh` (depth 1 + 1 for sparse and text, bytes equal to their lists), `check-links.sh`, `check-verified.py`, `build-index.py --check`, `check-milestone-ids.py`, `check-poison.sh` and `check-portable.sh` passed. `cargo test --workspace --no-fail-fast` passed but for the two typed-value byte fixtures named in criterion 3 (fixed as stated) and the two IPv6 peer tests this container cannot run (skipped). `scripts/coverage.sh --changed`: `equality.rs` 97.5% regions, 97.9% lines; the 95% floor is CI's full run.
