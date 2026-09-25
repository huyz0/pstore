# M8m — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

⚠️ **How "killed" was verified:** each named mutant applied by hand, one at a time, and the one
test seen FAILED (by the author, and again independently by the round-1 reviewer); then the
sweep in criterion 4. Line numbers are the measurement's.

1. **Each named mutant killed**, by the test named:
   `a_segment_is_its_data_blocks_its_index_and_its_footer` (`index_section_len` → `Some(0)`,
   `Some(1)`; `data_end` → `0`, `1`); `a_vectors_section_with_no_rows_reads_as_no_vectors`
   (`382:35` guard → `true`, `382:40` `>`→`>=`, each a division by zero);
   `a_vectors_section_too_narrow_for_a_row_does_not_displace_the_postings` (`591:82` `>`→`>=`:
   `Err(Truncated)`). All pass: `cargo test -p pstore-format`.
2. **The rewrites** — `vector_rows` returns on `per_row == 0`, then on `rows.is_empty()`;
   `put_impact` has no guard and converts with `u8::try_from`. Their new mutants, by hand:
   `+ 128` → `-` and `*` fail `a_sparse_field_round_trips` and `a_scan_reconstructs_a_sparse_field`;
   `per_row == 0` → `!=` fails `every_row_decodes_to_its_own_vector`. The reviewer found one
   behaviour change the spec had denied — public `write_list` with a `U8` `max` below the largest
   `|w|` — recorded in the spec's amendment; nothing in the tree passes one.
3. **The exclusion** — `cargo mutants --list --file crates/pstore-format/src/sparse.rs`: 235 with
   it, 249 without, the 14 being exactly the f16 `|`→`^` (author; the reviewer diffed the two
   lists). **NOT-RUN** as a test: an equivalence has no failing input. The reviewer checked the
   disjoint-bits argument path by path, including the subnormal carry.
4. **Every `pstore-format` mutant** — [`sweep/confirm.txt`](sweep/confirm.txt), on the final
   tree: `./scripts/mutants.sh --check '^crates/pstore-format/src/' --test-workspace=false --test-package pstore-format`
   **758 tested in 21m, 47 missed, 662 caught, 49 unviable** (31 of the missed ours, the rest
   other crates' struct-field deletions this filter admits); those 31 re-run with every package
   that can see the crate: **31 tested in 19m, 31 caught**. So 0 missed.
5. **The full gate** — `./scripts/gates.sh` on the final tree: all fifteen PASS.
