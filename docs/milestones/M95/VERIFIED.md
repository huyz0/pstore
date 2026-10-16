# M95 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. Nothing here is a latency; byte counts
and row sets are exact.

Commands:
- `cargo test -p pstore-format --lib --test parquet`
- `cargo test -p pstore-index --test external`

1. **Located and read back.** `a_scalar_column_is_located_and_read_back`: `id` of `good.parquet`
   and `multipage.parquet` reads back as `Int(row)` for every row in one `read_all`.
   - Seen red with `locate_scalar` absent (compile).
2. **Every encoding.** `a_scalar_column_is_read_in_every_encoding`: exactly 24 flat fixtures read
   `Int(row)` for every row; `optional-snappy` and `optional-dictionary` are refused as for a
   vector.
3. **Each type, with nulls.** `scalar_columns_of_each_type_read_back_with_their_nulls`
   (`scalars.parquet`, `scalars-zstd.parquet`, `scalars-v2-zstd.parquet`, from
   `generate_scalars.py`); `a_scalar_column_of_each_kind_decodes` (extremes, exact `FLOAT`
   widening, `DOUBLE` bits including a subnormal and `-0.0`, wrong width is no value, widths 4/4/8/8).
4. **Refusals.** `a_non_scalar_column_is_refused_with_its_type`;
   `a_logical_type_its_physical_value_does_not_carry_is_refused` (unsigned, date, timestamp,
   decimal; seen red before the `signed_plain` check: a `uint32` read back negative);
   `nulls_that_cannot_be_read_in_place_are_refused_by_name`;
   `a_column_claiming_too_many_rows_is_not_read`; `the_attribute_row_cap_is_inclusive`.
5. **Filter equals the exact filtered cosine.** `a_filter_on_the_files_own_column_equals_the_exact_filtered_cosine`.
6. **Another object or length.** `an_attribute_of_another_object_or_length_is_refused` (`Corrupt`,
   no byte moved); `a_null_attribute_reaches_keep_as_none`.
7. **Nothing admitted, no vector read.** `an_attribute_that_admits_nothing_reads_no_vector`.
8. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `cargo mutants --no-config --in-diff` over the changed source lines.
     - `pstore-index`: 9 mutants, 9 caught.
     - `pstore-format`, first run: 72 mutants, 6 missed, 2 timeouts. Killed by hand:
       `AttrKind::width` as 0/1 by the width assertions; the `rows > MAX` boundary (`>=`, `==`,
       `<`) by `the_attribute_row_cap_is_inclusive` (guard extracted to `check_attribute_rows`);
       `&&` as `||` in `logical_is_signed_int` by
       `a_logical_type_is_signed_only_as_an_integer_with_is_signed`; the `(6, 5)` converted-type
       arm by `a_converted_type_is_read_from_the_schema_element`.
     - ⚠️ One survivor, **equivalent**: deleting the `(2, 2)` arm (`isSigned` false) in
       `logical_is_signed_int`; the fallback skips a bool, which consumes nothing, and
       `is_signed` is already false.
     - Re-sweep: 73 mutants, 64 caught, 8 unviable, 1 missed (the equivalent one above).

**Code review:** two majors fixed (a logical type the physical value does not carry was read
as signed; a null row's path to `keep` was untested), plus minors. Review rounds stayed within budget.

**Not covered**, as the spec states: Lance scalar columns; string, boolean, unsigned, decimal
and temporal attributes; nulls in compressed pages; zone maps; an engine or API surface.
