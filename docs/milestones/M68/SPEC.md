# M68 — Vectors left in the customer's file: a pinned external column

**Serves:** OQ-169 / backlog row 72 — *can an index be built over a customer's own Parquet or
Lance files, in their bucket, without copying the vectors?* This milestone **answers the
three questions the open question names, and delivers the one piece that makes the answer
usable**: reading a row's vector out of an external object by a computed ranged read, pinned to
the object's version. It does not parse Parquet or Lance.

## The answers (OQ-169's three questions)

1. **Is a row's vector addressable by a ranged read?** Only when the column is stored as
   fixed-width, uncompressed values. Parquet pages are compressed and encoded per page, with
   the page offsets in the footer, so a row's vector costs a footer read plus a page read plus
   a decompress: not a computable range, and a data-dependent chain (footer, then page).
   Lance stores fixed-size-list columns contiguous and uncompressed by default, so a row is
   `base + row * dims * 4`. ⚠️ Stated from the formats' published layouts, **not** measured
   against live files here.
2. **Price.** A segment that holds codes and a row map stores no float vectors, so its size is
   the codes (RaBitQ, ~dims/8 bytes a row) plus a map. A rerank fetch is one ranged read per
   candidate against the customer's object, coalesced, the same request count as pstore's own
   `vector_rows` and no more bytes (`dims * 4` a candidate).
3. **When the source changes.** Lance appends a manifest version and Parquet writes a new file:
   neither mutates in place, so a segment **pins** the object (its length and CAS tag) and a
   read refuses a changed one. HEAD naming a pinned source is a later milestone.

**Decision:** a lake-native index is feasible exactly for fixed-width uncompressed columns
(Lance, and Parquet written without compression and with a PLAIN fixed-length column); general
Parquet needs a footer-parsing reader and costs a chain, which D-34 forbids on the query path
unless it is the opt-in rerank.

## Delta

`pstore_format::external::ExternalColumn { key, base, dims, stride, rows, len, tag }`:
- `row_range(row)`: the byte range of one row's vector, `base + row * stride` for `dims * 4`
  bytes; `None` past `rows`.
- `pin(store, key, base, dims, stride, rows)`: records the object's length and CAS tag, and
  refuses a column whose last row ends past the length, a `stride < dims * 4`, `dims == 0`, or
  an overflow.
- `read_rows(store, rows)`: verifies the pin (length and tag unchanged, else
  `ExternalChanged`), reads the named rows by ranged read, skipping duplicates and rows past the
  end, and returns each as `Vec<f32>`.

**Not covered:** parsing Parquet footers or Lance manifests; HEAD naming a source; building an
index over the column; a write path. Each is a milestone of its own.

## Acceptance criteria

1. `row_ranges_are_computed`: ranges are `base + row * stride` for `dims * 4` bytes, `None` at
   and past `rows`; a stride wider than the vector leaves the gap unread.
2. `pin_refuses_a_column_that_does_not_fit`: zero dims, a stride under the vector's width, a
   last row past the object's end, and an overflowing `rows * stride` are each refused.
3. `read_rows_returns_the_files_floats`: against a synthetic file with a header, interleaved
   records and a trailer, duplicates and past-end rows dropped, values equal.
4. `read_rows_reads_only_the_rows`: bytes moved are the rows' ranges (a thousandth of the
   file), measured with the accounted store.
5. `a_changed_object_is_refused`: after an overwrite (same length, then a different length) a
   read is `ExternalChanged`.
6. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the new module misses 0.
