# M101 — Lance scalar attribute columns

**Serves:** backlog row 75. [M95](../M95/SPEC.md), [M99](../M99/SPEC.md) and
[M100](../M100/SPEC.md) read an attribute from a Parquet file. An external index over a **Lance**
dataset ([M82](../M82/SPEC.md), [M87](../M87/SPEC.md), [M90](../M90/SPEC.md)) still has no
attribute: a caller must supply the values itself. This reads a fixed-width number, date or
timestamp from the same dataset the vectors are in.

Lance 2.1's bit-packed integers, booleans, strings and nullable columns stay refused, by name.
They are recorded open in row 75. Zone maps are later.

## What is true today

These facts come from reading the tree after M100 and pylance 13's output. Nothing was measured.

- **What the reader reads.** `lance::locate` reads only a float32 fixed-size list, through the
  manifest, every fragment's data file, and each page.
  - 2.0: `encoding_is_plain_floats` accepts `Nullable(NoNull) -> FixedSizeList(dims) ->
    Nullable(NoNull) -> Flat(32)`.
  - 2.1: `v21::pieces` accepts a `MiniBlock` or `FullZip` page whose value compression is
    `FixedSizeList(dims) -> Flat(32)`, with no nulls.
- **What a scalar is, in pylance 13's files.** These were read from datasets written for this
  milestone (`generate_lance_scalars.py`):
  - The manifest's logical type is one of `int32`, `int64`, `uint32`, `uint64`, `float`,
    `double`, `date32:day`, `timestamp:<ms|us|ns|s>:<zone or ->`, `bool`, `string` and `int16`.
  - 2.0: `Nullable(NoNull) -> Flat(bits)`, the same path as a vector's items, with no list
    around it. A column with nulls is `Nullable(SomeNulls)`.
  - 2.1: a `MiniBlock` page whose value compression is `Flat(bits)` (field 1), with the same
    layer and chunk index as a vector's. For most integer columns of 2,600 rows, the writer
    chose `InlineBitpacking` (field 5) instead, even where values are negative. Floats and
    integers that use all 64 bits stayed `Flat`.
- **What a Lance pin gives.** `ExternalColumn::pin_lance` returns an `ExternalColumn` of several
  objects, one a data file. A deleted row is a gap, so it is not present. `ExternalAttribute`'s
  `Fixed` source reads any `ExternalColumn`, and it reads a gap as `None`.
- **What `search_attr` checks.** It compares the attribute's `key()` and `rows()` with the
  index's column. For a dataset of several data files, that compares only the first data file.

## Delta

1. **The leaf, by kind.**
   - `lance::locate` keeps its signature and behaviour.
   - A new `pub(crate) lance::locate_attribute(store, manifest, column)` returns the same
     `Dataset` and an `AttrKind`. Both go through one walk of the manifest and fragments.
   - The kind comes from the manifest's logical type:

     | Logical type | `AttrKind` | Width |
     |---|---|---|
     | `int32` / `int64` | `Int32` / `Int64` | 4 / 8 |
     | `uint32` / `uint64` | `UInt32` / `UInt64` | 4 / 8 |
     | `float` / `double` | `Float` / `Double` | 4 / 8 |
     | `date32:day` | `Date` | 4 |
     | `timestamp:ms:*` / `timestamp:us:*` / `timestamp:ns:*` | `TimestampMillis` / `Micros` / `Nanos` | 8 |

   - The time zone after the unit is ignored, as M99 ignores `isAdjustedToUTC`.
   - Anything else is refused as `Unsupported`, naming the logical type and saying which kinds
     are read: `bool`, `string`, `int8`, `int16`, `timestamp:s:*`, a list, and the rest.
   - A width is 4 or 8 bytes, so a scalar is a 1- or 2-lane `Located` (`dims` = width / 4), as
     M95's is.
2. **The page, by shape.** `locate_file` and `v21::pieces` take a `Shape`: either
   `Vector(dims)` or `Scalar(bits)`.
   - The 2.0 check is `Nullable(NoNull) -> Flat(bits)` with buffer `(0, 0)`. A
     `Nullable(SomeNulls)` page is refused as holding nulls.
   - The 2.1 check is a value compression of exactly `Flat(bits)`.
     - `InlineBitpacking` is refused as bit-packed. Any other compression is refused by the
       mini-block layout's existing message, as not an uncompressed run.
     - A layer other than an all-valid item is refused as holding nulls.
   - The size checks become rows × width, where the width is `dims × 4` or `bits / 8`.
     M82's and M90's messages are kept for a vector.
3. **The pin.** `ExternalAttribute::pin_lance(store, manifest, column)` pins as
   `ExternalColumn::pin_lance` does: each data file's tag and length are checked. It returns
   `AttrSource::Fixed`, and `read_all` is unchanged.
   - `ExternalAttribute::pin_lance_latest(store, dataset, column)` follows the hint, as M89's
     does.
4. **The object check covers every data file.** `ExternalAttribute::keys()` returns every
   object's key: one for `Chunks`, or the column's key and its `more`. `search_attr` refuses an
   attribute whose `keys()` differ from the index column's, with the M95 message.

**Not changed:** the Parquet path; `read_all`; the vector reader's accepted encodings; the engine
and the API (row 76).

## Acceptance criteria

1. **Read back in every layout.** `lance_scalar_columns_read_back_in_every_layout`
   (`crates/pstore-format/tests/lance.rs`). The fixtures, written by `generate_lance_scalars.py`
   with pylance 13, hold 2,600 rows. Every value is a function of the row r:
   - `one` (2.0, one file) and `many` (2.0, 3 fragments, rows 1000 to 1009 deleted) each read
     back these columns, row for row:
     - `i32`, `int64`, `f32`, `f64` and `u64` (as `UInt`);
     - `date`, `ts_ms`, `ts_us` (UTC) and `ts_ns`, floored as M99's are.
   - In `many`, the 10 deleted rows are `None` and every other row is present.
   - From `v21`, the 2.1 dataset, `int64`, `f32`, `f64` and `u64` read back.
2. **What is refused, by name.** `a_lance_column_not_read_as_an_attribute_is_refused_by_name`:
   - `b` is refused as `bool`, `s` as `string`, `i16` as `int16`, and `ts_s` as `timestamp:s`;
   - `emb` is refused as not a scalar;
   - `i32n` is refused as holding nulls, in both `one` and `v21`;
   - `v21`'s `i32` and `ts_ms` are refused as bit-packed;
   - a missing column is refused as such.
3. **The kind is the table.** `a_lance_logical_type_is_its_kind` (a unit test) covers every row
   of the table, `timestamp` with and without a zone, and `timestamp:s:-`, `int16`, `bool` and
   `fixed_size_list:float:8`, each refused.
4. **The vector reader is unchanged.** Every existing test in `tests/lance.rs` and
   `tests/lance21.rs` passes unchanged, messages included.
5. **An external filter on a Lance column.**
   `a_filter_on_a_lance_column_equals_the_exact_filtered_cosine`
   (`crates/pstore-index/tests/external.rs`):
   - An index over `many`'s `emb`, with `search_attr` keeping `int64 ≥ -600 << 40` (rows 900
     and on), equals the exact filtered cosine at a full window, for queries at rows 0, 1005
     and 2000. The deleted rows 1000 to 1009 fall inside the filter and are excluded; 1005 is
     the query's own row.
   - An attribute whose second data file's key was edited is refused as being in another
     object.
6. **Gates.**
   - Each new test is seen red first.
   - `./scripts/gates.sh` passes, and so do the coverage floors (95/95).
   - `./scripts/mutants.sh` misses 0 on the changed lines, or each miss is argued equivalent.

## Test plan

| Test | Catches (mutation) |
|---|---|
| `lance_scalar_columns_read_back_in_every_layout` | a width wrong; a kind swapped; deleted rows read; the 2.1 `Flat` check |
| `a_lance_column_not_read_…_is_refused_by_name` | a refusal lost; nulls or bit-packing let through |
| `a_lance_logical_type_is_its_kind` | a table row dropped or swapped; the unit parsed wrong |
| the existing Lance tests | the vector shape's checks |
| `a_filter_on_a_lance_column_…` | the end-to-end path; the every-object check |

## RA budget

- **The pin:** the same requests `ExternalColumn::pin_lance` makes. That is the manifest, then
  for each data file a `head`, a tag, the footer, the column table entry and the column's
  metadata, plus a 2.1 page's chunk index. These scale with data files and pages, so with
  bytes.
- **`read_all`:** M95's Fixed path: the present rows' extents, one round, fan-out.
- It is off the hot path, as M95's is.

## Risks

- **Most 2.1 integer columns are refused.** pylance 13 bit-packs them. Reading
  `InlineBitpacking` means decoding FastLanes' transposed layout. It is recorded as open in row
  75, not guessed at here.
- **The layouts were read from one library** (pylance 13), as M82's and M90's were. A page from
  another writer that does not match is refused, never misread.

## Tasks

- **M101.1** `Shape`, the kind table, `locate_attribute`, and the unit test.
- **M101.2** The pins, `keys()`, the fixtures, and the integration tests.
- **M101.3** Review, the gates, the ledger, and the backlog row.
