# M91 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, pyarrow-written fixtures.

Command: `cargo test -p pstore-format`; `cargo test -p pstore-index --test external`.

1. `lists_of_floats_read_back_every_row`: `list-v1`, `list-v2`, `list-required` and `list-groups`
   locate with 8 dimensions and every row reads back exactly; `list.parquet` too.
2. `a_list_column_that_cannot_be_read_in_place_is_refused_by_name`: snappy, double, ragged, null
   list, null item, nested, struct, and lists of different lengths across row groups
   (`list-dims`) are each refused with their reason.
3. `levels_decode_in_rle_and_bit_packed_runs`, `repetition_levels_give_the_row_length`,
   `definition_levels_must_all_be_the_maximum` and `bit_widths_are_those_of_the_largest_level`
   (unit), plus `a_page_claims_between_one_and_the_cap_values` and
   `a_dictionary_offset_of_zero_means_none` for the page and chunk bounds, and
   `a_column_after_a_list_is_found_past_the_whole_list`.
4. `an_index_over_each_codec_finds_the_exact_neighbours`: the list files join the others and match
   the exact cosine top 10.
5. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants --no-config --in-diff`, `parquet/list.rs` and `parquet.rs`): first sweep 254
   mutants, ~23 missed; second 226, 17 missed and 1 timeout (boundary checks the page's own size
   made redundant, removed; the rest given unit tests and a fixture); final sweep: **203 mutants, 198 caught, 5
   unviable, 0 missed, 0 timeouts.**

**Not covered:** compressed list pages; dictionary-encoded lists; `list<double>`; ragged lists.
