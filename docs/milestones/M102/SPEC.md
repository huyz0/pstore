# M102 — Lance 2.1 bit-packed integer attribute columns

**Serves:** backlog row 75. [M101](../M101/SPEC.md) reads a Lance scalar attribute stored flat,
and refuses one stored with `InlineBitpacking`. pylance 13 bit-packs most 2.1 integer, date and
timestamp columns, so in 2.1 those columns are still refused. This milestone reads them.

Booleans, strings and nullable columns stay refused, by name. Zone maps are later.

## What is true today

These facts come from reading the tree after M101 and from pylance 13's files. Nothing here was
measured.

- **What M101 reads.** `v21::pieces` reads a `MiniBlock` page whose value compression is
  `Flat(bits)`. It refuses `InlineBitpacking` (compression field 5) as bit-packed.
- **What a bit-packed page is.** These facts were read from `lance-scalars/v21`. A script that
  decodes every packed column of that 2,600-row dataset reproduced each value.
  - The page has two buffers, as a flat mini-block page has:
    - the chunk index, one `u16` per chunk, `words << 4 | log2(values)`;
    - the chunks.
  - A chunk is an 8-byte header (`u16` levels bytes = 0, `u16` values bytes `vlen`, padding),
    then the values, padded to 8 bytes. Its size is `(words + 1) × 8`.
  - The values are:
    - the chunk's bit width `W`, as a value of the column's width `T` (32 or 64 bits);
    - then 1,024 values of `W` bits in FastLanes' order, `W × 128` bytes.

    So `vlen = T/8 + W × 128`. A last chunk of fewer rows is still packed as 1,024 values.
  - `W` varies by chunk: 10, 11 and 12 for `r` over 0..2600. A negative value takes the full
    width.
  - FastLanes' order, for `L = 1024 / T` lanes:
    - the `row`-th `W`-bit field of lane `lane` is value
      `ORDER[row / 8] × 16 + (row % 8) × 128 + lane`;
    - a lane's bits run through its words `L × k + lane`, low bit first;
    - `ORDER = [0, 4, 2, 6, 1, 5, 3, 7]`.
- **Why a run cannot hold it.** A row's bits are not a fixed byte range, so the whole chunk is
  read and unpacked. M75 already reads a compressed Parquet page this way: an `Extent` with
  `compressed > 0` is fetched whole by `ExternalColumn::read_rows_bytes` and decoded by its
  `codec`.

## Delta

1. **A chunk is a compressed extent.** A bit-packed chunk becomes one `Extent`:
   - `offset` is the chunk's start (its header);
   - `compressed` is the chunk's bytes, header included;
   - `codec` is `CODEC_FASTLANES` (0x80), pstore's own number, outside Parquet's range;
   - `plain_len` is how many of the chunk's 1,024 values come before the extent's first row
     (0 for a whole chunk).

   No new `AttrSource` is added. `AttrSource::Fixed` and `read_rows_bytes` read it.
2. **The page.** In `v21::pieces`, a scalar of 32 or 64 bits whose value compression is exactly
   `InlineBitpacking(bits)`, with the column's own `bits`, is read.
   - Each chunk index entry is checked:
     - at most 1,024 values;
     - `words × 8` at least `T/8` (room for the width);
     - and at most `T/8 + T × 128` padded (every bit of every value).
   - The rows and bytes must add up to the page, as M90's flat chunks must.
   - A packed page at another width is still refused, as bit-packed at another width than the
     column's.
3. **A deletion** cuts a compressed extent without moving it. Each piece keeps `offset` and
   `compressed` and adds its skipped rows to `plain_len`.
4. **The read.** `read_rows_bytes` unpacks a `CODEC_FASTLANES` page with
   `lance::fastlanes::unpack`. It reads row `r` at value `r − first_row + plain_len`.
   - `unpack` returns `None`, which gives `Damaged`, for any of these:
     - a level byte;
     - a chunk whose length is not its `vlen` padded;
     - a `W` past `T`;
     - a `vlen` that is not `T/8 + W × 128`.
   - The object's length and tag are checked first (`Changed`), as for every compressed page.

**Not changed:** M101's flat path, the vector reader, Parquet, the engine and the API. The HEAD
encoding of an `Extent` already carries `compressed`, `codec` and `plain_len`.

## Acceptance criteria

1. `bit_packed_lance_columns_read_back` (`tests/lance.rs`). In `lance-scalars/v21` and in
   `v21-deleted` (several fragments, with rows deleted inside chunks):
   - `i32`, `date`, `ts_ms`, `ts_us`, `ts_ns` and `small` read back row for row;
   - a deleted row is `None`;
   - in `v21` every run is a `CODEC_FASTLANES` chunk. In `v21-deleted`, at least one is: there
     the writer stored some 1,000-row fragments flat, so one column mixes flat and packed pages.

   M101's refusals of `v21` `i32` and `ts_ms` as bit-packed become these reads.
2. `the_fastlanes_order_is_unpacked_at_every_width` (unit). A chunk packed bit by bit by a
   test-side packer, written from the order above, unpacks to its values:
   - at widths 0, 1, 7, 31 and 32 in `u32`;
   - at widths 0, 33 and 64 in `u64`;
   - at the top value of widths 13, 21 and 32.

   `a_chunk_that_disagrees_with_its_header_is_refused` returns `None` for each of these:
   - a width past the value's;
   - a width the body is not sized for;
   - a short or long chunk;
   - a level byte;
   - a lying `vlen`;
   - a 2-byte value.
3. `a_damaged_packed_chunk_is_refused_not_misread` (`tests/lance.rs`). These are `Damaged`:
   - an edited level byte, values length, or bit width (33, or one the body is not sized for);
   - a chunk changed before the pin.

   A chunk changed after the pin is `Changed`.
4. `a_bit_packed_chunk_is_a_width_and_at_most_its_values_bits` (unit) covers `chunks_of`:
   - it accepts a chunk at the word bounds (1 and 1,025 words of 8-byte values);
   - it refuses one word past them, and a chunk of more than 1,024 values.

   `only_inline_bitpacking_at_the_values_width_is_bit_packed` covers `bit_packed` and
   `scalar_refusal`.
5. `deleted_rows_are_cut_out_of_a_bit_packed_chunk_by_its_skip` (unit):
   - a cut keeps the chunk's offset;
   - it sets `plain_len` to the rows skipped;
   - a second cut adds to it.
6. Gates, as M101's: fmt, clippy, tests, the coverage floors, and 0 missed mutants on the
   changed lines.

## RA budget

- **The pin:** M101's, plus nothing. M90 already reads each page's chunk index.
- **`read_all`:** as M75's compressed pages: one ranged GET per chunk, all in one round, plus a
  `head` and a tag for each object.

## Risks

- **One writer.** The order and the header were read from pylance 13's output, as M90's layouts
  were. A chunk that does not decode to its declared sizes is refused. A flipped bit inside the
  packed values is not detected: Lance stores no checksum, as Parquet stores none by default.
