# M70 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store. The Parquet fixtures are
**real files written by pyarrow 26** (`crates/pstore-format/tests/data`, regenerable with
`generate.py`), so the reader is checked against another implementation's bytes, not only
against this repository's own writer.

Commands:
- `cargo test -p pstore-format --test parquet --lib`: 6 integration + the Thrift unit tests.
- `cargo test -p pstore-index --test external`: 7 tests.

1. `locate_finds_the_real_files_vectors`: `good.parquet` (3 row groups of 100) and
   `multipage.parquet` (2,600 rows in several pages): every row read back through the located
   extents equals the float the generator wrote, including page and row-group boundaries; the
   `id` column is refused as not a vector.
2. `locate_refuses_what_cannot_be_reached`: the real snappy, dictionary, optional and
   `LIST<FLOAT>` files are each refused with a reason naming it (compressed, dictionary,
   REQUIRED, repetition levels); a missing column, a short, empty, text, truncated, zero-footer,
   huge-footer and bad-magic file are refused; flipping every third byte of the footer is never
   a panic and is refused more than ten times.
   `locate_refuses_each_shape_it_cannot_read` builds the shapes pyarrow never writes with a
   small Thrift-compact writer in the test: a type length of 6, 0 and -4, a wrong physical type,
   an optional and a repeated column, a codec, a dictionary offset, an unsupported encoding in the
   chunk and in a page, a chunk type that disagrees, a compressed page, a page with levels, a
   dictionary or v2 page, counts that disagree, a chunk past the file, a chunk shorter than its
   page, a page of no values, pages that run out and pages that overshoot.
3. `varints_and_zigzag`, `fields_lists_and_skips` and `bools_and_nesting_depth` (unit tests in `parquet.rs`): varints and zigzag, field
   deltas and long-form ids, short and long lists, nested structs, maps, doubles and bytes;
   bools as struct fields (no payload) and as list elements (a byte each); depth 32 allowed and
   33 refused; 100 sibling structs are not nesting.
4. `extents_address_rows`: ranges at the first and last row of each extent and past the end;
   `from_extents` refuses none, a zero-row run, a gap, an overlap, a start past 0, a run past the
   object by one byte, an overflow and zero dims, and accepts the exact fit; a changed object is
   refused for a located column.
5. `an_index_over_a_parquet_file_finds_the_exact_neighbours`: build, reopen (the blob is
   byte-identical) and search `multipage.parquet`; at a window over every row, the top 10 for four
   queries equal an exact cosine over the generator's floats, each query row is its own nearest
   neighbour, a changed object is refused, and damaging the first run's row count in the blob is
   refused.
6. **Gates:** `./scripts/gates.sh` green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`):
   - `parquet.rs` and `external.rs`: the first sweep, 428 mutants, 392 caught, 15 unviable,
     **21 missed**. Each was killed by a named test or removed by restructuring: the Thrift
     depth and bool cases by `bools_and_nesting_depth`; the page-header defaults (a `-1`
     sentinel two of the misses were equivalent through) replaced by `Option`s; the footer-size
     bound tightened to `size + 12` (magic, footer, length, magic) and tested at the boundary;
     the type-length, page-size, level and overshoot checks by
     `locate_refuses_each_shape_it_cannot_read`; the extra struct field by
     `the_writer_builds_what_locate_reads`; the run-width check by the `one byte past` case. The
     re-sweep of the changed and previously missed lines: **82 mutants, 82 caught, 0 missed.**
   - `pstore-index` `external.rs` (changed lines): 8 mutants, 7 caught, 1 unviable, 0 missed.

**What OQ-169 now has:** an answer (M68), a pinned read (M68), an index over a fixed-width
column (M69), and the column found in a real Parquet file's footer (this milestone), so a
customer names a file and a column.

**Not covered**, as the spec states: Lance manifests (protobuf; a caller supplies the single
run), compressed, dictionary, optional and `LIST<FLOAT>` Parquet columns, v2 data pages,
nested columns, clustering, filters, HEAD naming the index, an engine or API surface.
