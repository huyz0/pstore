# M68 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. The layout claims about Parquet and Lance
in SPEC.md are from the formats' published layouts and are **not measured against live files**;
what is measured is the pinned reader.

Command: `cargo test -p pstore-format --test external`: 5 tests.

1. `row_ranges_are_computed`: `base + row * stride` for `dims * 4` bytes; `None` at, past and
   at `usize::MAX`.
2. `pin_refuses_a_column_that_does_not_fit`: zero dims, a stride under the vector, a last row
   past the object, an overflowing base, and an overflowing stride are each refused; the exact
   fit, zero rows, and a row ending on the object's last byte are accepted (and one byte more
   is not). Seen red on a test fault first: `rows + 2` fits because of the file's trailer.
3. `read_rows_returns_the_files_floats`: a file with a header, 24-byte records and a trailer;
   rows 0, 7 and 199,999 equal the written floats; duplicates and rows past the end dropped.
4. `read_rows_reads_only_the_rows`: three rows of a 4.8 MB object move under a twentieth of
   it. Seen red at 1,000 rows, where the coalescer merged the whole 24 KB file.
5. `a_changed_object_is_refused`: same length with other bytes, then a longer object, are both
   `Changed`; a deleted object is an error; nothing asked reads nothing.
6. **Gates:** `./scripts/gates.sh` green by the pre-commit hook on this commit. **Mutation:**
   `cargo mutants` over `external.rs`: **40 mutants, 32 caught, 8 unviable, 0 missed.**

**Not covered**, as the spec states: Parquet footers and Lance manifests, HEAD naming a
source, an index over the column, a write path.
