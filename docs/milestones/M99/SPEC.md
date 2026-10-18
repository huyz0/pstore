# M99 — Unsigned, date and timestamp attribute columns

**Serves:** backlog row 75. M95 reads a Parquet attribute column only when its physical value
*is* its value, so `INT32`, `INT64`, `FLOAT` and `DOUBLE` with no logical type, or a signed
integer one. Every column whose logical type reinterprets those bytes is refused today:
- unsigned integers,
- dates,
- timestamps.

They are still fixed-width. A value is 4 or 8 bytes in the same pages, so M95's extents read
them unchanged. Only the type check and the decode change.

Strings and booleans are not fixed-width, and neither are Lance scalar columns: they are
**M100** and **M101**. Zone maps are later.

## What is true today

These facts come from reading the tree at `96206cd`. Nothing here was measured.

- **The type check, `parquet::locate_flat(.., scalar = true)`:**
  - It maps the physical type to `AttrKind {Int32, Int64, Float, Double}`.
  - It refuses with "the column has a logical type (unsigned, decimal, date, time or
    timestamp) …" unless the converted type is absent or `INT_8..=INT_64` and the logical type
    is absent or a signed `INTEGER`.
- **The `LogicalType` union is not decoded.** `logical_is_signed_int` keeps only one bit:
  whether it is a signed `INTEGER`.
- **The value types.** `AttrValue` is `Int(i64)` or `Float(f64)`, and `AttrKind::decode` turns
  4 or 8 little-endian bytes into one.
- **The refusal is pinned.** `a_logical_type_its_physical_value_does_not_carry_is_refused`
  asserts that `scalars.parquet`'s `u32`, `u64`, `date`, `ts_ns`, `ts_ms` and `dec` are refused.
  The fixture is written by pyarrow 26.

## Delta

1. **The logical type is decoded.**
   - `Leaf.logical` becomes a `Logical`, one of:
     - `Integer { signed }`,
     - `Date`,
     - `Timestamp(unit)`, the unit being millis, micros or nanos,
     - `Other`.
   - It comes from the union's fields 10, 6 and 8. Inside `TIMESTAMP` it reads field 2's
     `TimeUnit` (union fields 1, 2, 3); `isAdjustedToUTC` is ignored, see Risks.
   - Every other member is `Other`.
2. **One pure classification, `scalar_kind(physical, converted, logical)`.**
   - It returns the `AttrKind`, or the reason for a refusal. The logical type, when set,
     decides. Otherwise the converted type does.
   - The table is below. Any other combination is refused, including decimal, time, interval,
     `JSON`, `UUID`, `FLOAT16`, and a logical type on a physical type it cannot sit on. The
     refusal message still says "logical type", and now names the physical type.

     | Physical | Logical | Converted | `AttrKind` |
     |---|---|---|---|
     | INT32 | none, or `Integer{signed}` | none, `INT_8..INT_32` | `Int32` (as today) |
     | INT64 | none, or `Integer{signed}` | none, `INT_64` | `Int64` (as today) |
     | FLOAT / DOUBLE | none | none | `Float` / `Double` (as today) |
     | INT32 | `Integer{unsigned}` | `UINT_8..UINT_32` | `UInt32` |
     | INT64 | `Integer{unsigned}` | `UINT_64` | `UInt64` |
     | INT32 | `Date` | `DATE` | `Date` |
     | INT64 | `Timestamp(ms / µs / ns)` | `TIMESTAMP_MILLIS / _MICROS` | `TimestampMillis / Micros / Nanos` |

   - An integer's declared width (`bitWidth`, or `UINT_8` against `UINT_64`) is not checked
     (code review). The physical type is what is read, so an unsigned integer on INT64 is a `u64`
     and one on INT32 a `u32` whatever width was claimed, and every value reads back as stored.
     An `IntType` without its required `isSigned` is `Other`.
   - The converted-type column of the table holds only when no logical type is set. When both
     are set, pyarrow writes them consistently, and the logical type is read.
3. **Values.**
   - `AttrValue` gains `UInt(u64)` and `DateTime(i64)`. `DateTime` is microseconds since the
     Unix epoch, the unit `pstore_format::Value::DateTime` already uses.
   - `UInt32` decodes as `Int` (every `u32` is an `i64`), and `UInt64` always as `UInt`, so the
     variant depends on the kind and never on the value.
   - `Date`: days × 86 400 000 000. `TimestampMillis`: ms × 1000. `TimestampMicros`: as is.
     `TimestampNanos`: ns divided by 1000, rounded toward negative infinity.
   - A product that overflows `i64` (a date or millisecond timestamp more than ~292 000 years
     from 1970) is `None` from `decode`, so `read_all` gives `Damaged`, as for a value of the
     wrong length.
   - `as_f64` covers both new variants.
4. **No other crate changes.** `pstore-index`'s `search_attr` already passes `AttrValue` to the
   caller's `keep`; its match arms are the caller's. The engine and API do not see attributes
   (row 76's decision).

## Acceptance criteria

1. **Unsigned columns read back.** `unsigned_and_temporal_columns_read_back_exactly`
   (`crates/pstore-format/tests/parquet.rs`): `scalars.parquet`'s `u32` (3 000 000 000 + r)
   reads as `Int`, and `u64` (2^63 + r) as `UInt`, row for row, all 300 rows.
2. **Dates and timestamps read back.** In the same test:
   - `date` is `DateTime(r × 86 400 000 000)`;
   - `ts_ms` is `DateTime(r × 1000)`;
   - `ts_ns` is `DateTime(r div_euclid 1000)`.
   - From the new fixture `scalars-typed.parquet`, also written by pyarrow 26:
     - `u8`, `u16` and a UTC `ts_us`, whose values are negative too;
     - `ts_ns_neg`, negative ns floored;
     - `daten` and `u64n`, both with nulls every 7th row, read back with their nulls.
3. **What is still refused, is refused by name.**
   `a_logical_type_its_physical_value_does_not_carry_is_refused`:
   - `dec` and the new fixture's `time` still give "logical type";
   - `flag`, `name` and `ts96` still give `BOOLEAN`, `BYTE_ARRAY` and `INT96`.
   - The five names now read are removed from its refusal list, and criterion 1 asserts them
     read.
4. **The classification is the table.** `scalar_kind_is_the_table` (a parquet.rs unit test)
   covers:
   - every row of the table, by logical type alone and by converted type alone;
   - a logical type winning over a contradicting converted one;
   - `Date` on INT64, `Timestamp` on INT32, `Integer{unsigned}` on FLOAT, and `TIME_MILLIS`,
     `DECIMAL` and `Other`, each refused.
5. **The union decodes.** `a_logical_type_union_decodes_each_member` (a unit test over
   hand-written Thrift bytes) replaces `a_logical_type_is_signed_only_as_an_integer_with_is_signed`
   and keeps its three cases: signed, unsigned, and another member holding a true bool 2 is
   `Other`. It adds:
   - `DATE`;
   - `TIMESTAMP` in each of the three units, with the UTC flag before and after the unit;
   - a `TIMESTAMP` without a unit, which is `Other`.
6. **Decode edges.** `an_unsigned_or_temporal_kind_decodes` covers:
   - each new kind's width, and a value of the wrong length being `None`;
   - a `u32` above `i32::MAX`, and a `u64` above `i64::MAX`;
   - nanoseconds floored (−1 ns is `DateTime(-1)`, −1001 ns is −2);
   - a date and a millisecond timestamp that overflow, giving `None`, beside the last one that
     does not.

   `a_scalar_column_of_each_kind_decodes` adds `as_f64` of both new variants.
7. **An external filter on a timestamp.** `a_filter_on_a_timestamp_column_equals_the_exact_filtered_cosine`
   (`crates/pstore-index/tests/external.rs`): a new fixture `vectors-ts.parquet`, written by the
   committed script, holds `good.vectors.f32`'s rows beside a `ts` column (timestamp ms, r × 10^6).
   Every existing fixture is left byte for byte as it is.
   - `search_attr` keeping `DateTime(t)` with t ≥ 150 000 000 000 µs (rows 150 to 299) equals the exact filtered cosine
     at a full window, as M95's own criterion does.
8. **Gates.**
   - Every new or changed test is seen red first.
   - `./scripts/gates.sh` passes, and so do the coverage floors (95/95).
   - `./scripts/mutants.sh` misses 0 on the changed lines, or each miss is argued equivalent.

## Test plan

| Test | Catches (mutation) |
|---|---|
| `scalar_kind_is_the_table` | a table row dropped or swapped; converted read over logical; a physical check removed |
| `a_logical_type_union_decodes_each_member` | a union field id or time unit swapped; the `TIMESTAMP` struct read past its unit |
| `unsigned_and_temporal_columns_read_back_exactly` | sign extension of `u32`; a unit factor wrong; floor → truncate on negative ns |
| `an_unsigned_or_temporal_kind_decodes`, `a_scalar_column_of_each_kind_decodes` | the overflow checks removed; a width wrong; `as_f64` of a new variant |
| `a_logical_type_…_is_refused` | the decimal and time refusals lost |
| `a_filter_on_a_timestamp_column_…` | the end-to-end path: a kind the fold of pin → read → mask drops |

## RA budget

**Zero new requests.** The footer is read once, as today, and the same extents are read
for the same rows.

## Risks

- **`isAdjustedToUTC = false`.** A local timestamp is read as if it were UTC. That is the
  value the writer stored, and nothing in the file says which zone it was in. Documented on
  `AttrValue::DateTime`.
- **Nanoseconds lose their last three digits.** `DateTime` is µs, so a filter cannot tell
  apart two ns values in the same µs. This is the same precision `Value::DateTime` has.
- **A pre-M95 refusal list in a caller.** The library now accepts columns it used to refuse. No
  caller outside the workspace exists.

## Tasks

- **M99.1** `Logical` decode and `scalar_kind`, and their unit tests.
- **M99.2** The `AttrKind`/`AttrValue` variants, the fixtures and the integration tests.
- **M99.3** Review, the gates, the ledger, and the backlog row updated.
