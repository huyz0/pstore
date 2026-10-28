# M172: uint64 attributes

## Serves
Workstream D of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Round-trip
uint64 boundaries including values above JavaScript's safe integer range ... No lossy
conversion". Closes `uint` in the "Scalar attributes" row of
[the comparison](../../comparisons/turbopuffer-2026-10-25.md); persistent declarations and
uint64 IDs are later milestones'. Extends [M171](../M171/SPEC.md)'s request-declared types
the same way.

## Delta
Today an integer attribute is an `i64`. A write or filter integer in `(i64::MAX, u64::MAX]`
is refused ("integers must fit i64"), and a write's `schema` declares `datetime`, `uuid` or
`bytes`.

After, `schema` may also declare an attribute `"uint64"`, with M171's rules (per request,
scalar or array, a declaration no document carries refused, the text field not declarable,
a patch's attributes read by it). Its value is a JSON integer in `0..=u64::MAX`, stored as
an unsigned 64-bit integer whatever its size, and answered as that JSON integer, exactly.
Anything else is refused, `bad_request`, naming the attribute, nothing buffered: a negative
integer, a number with a fraction or an exponent that serde_json reads as a float (`5.0`,
`1e3`), a string, a bool.

A uint64 is a number. It compares with ints and floats by exact numeric value in every
filter (`Eq`..`Gte`, `In`, `Any*`, `Contains*` and their negations), in a boost's `Eq` and
its numeric functions (as the `f64` they already score in), in `rank_by` (inside the number
group), in facets, `group_by` and `limit_per` (`5` as a uint64 and `5` as an int are one
group, as `1` and `1.0` are today), and in `$ref_new` conditions. A number group reports an
integer whenever its value is one: an integral float in `[2^63, 2^64)` is reported as that
unsigned integer, as one below `2^63` is reported as an int today, so a bucket holding the
float `2^63` and the uint64 `2^63` answers `9223372036854775808` whichever row came first.
That changes the spelling, never the value, of such a float's facet today. A filter or boost literal
in `(i64::MAX, u64::MAX]` is now read as that unsigned number rather than refused; an
undeclared write of one is still refused, its message naming the `uint64` declaration. A
`sum` with no uint64 in it is unchanged (an int within `i64`, else a float). One with a
uint64 in it and no float is that integer while it is within `i64` or `u64`, and a float
beyond both.

The format gains value tag 8 (8 bytes, little-endian) in segment blocks and bundles, and
tag 8 in the deferred-condition codec. Blocks gain a uint64 zone per name, `(min, max)`,
carried in a new zone-map flag value 3: flag 2's float and datetime tables, then a uint64
table per block. The writer emits flag 3 only when some block holds a uint64 zone, so a
segment without one keeps its bytes. A typed segment with zone maps on (flags 1-3) records
every uint64 row in a zone, and a uint64 value makes a segment typed (as a float does), so an absent uint64 zone there means no uint64 row holds the
name, as for ints and floats. A reader before M172 refuses tag 8 and flag 3 as unknown:
deploy readers first; an older fold decodes such a `$ref_new` condition as unreadable, as
M171's.

Unchanged: every other type and its answers, a write with no declaration, an integer
literal within `i64`.

## Acceptance criteria
1. A write declaring `n: uint64`, scalars and arrays, including `0`, `2^53 + 1`,
   `i64::MAX`, `i64::MAX + 1` and `u64::MAX`, answers the same integers in a projection,
   in the unfolded view, after a fold, and after a restart.
2. Filters: `Eq`, `NotEq`, `In`, `Lt`, `Gte` and `AnyGt` over a name holding ints, floats
   and uint64s, with literals at and around `i64::MAX`, `2^63` and `u64::MAX` (ints,
   floats and unsigned), give the rows a reference computed in the test with exact `i128`
   and float comparison; a boost `Eq` on `u64::MAX` fires; a `$ref_new` condition over a
   uint64 applies.
3. `rank_by` asc and desc over ints, floats and uint64s orders them numerically, `i64::MAX`
   before `i64::MAX + 1` before `1.9e19`; a facet over `5` (uint64) and `5` (int) is one
   bucket, and the float `2^63` and the uint64 `2^63` are one bucket answering
   `9223372036854775808` in either row order; `sum` over `u64::MAX` alone is that integer,
   over `u64::MAX` and the int `-1` is `u64::MAX - 1`, and over the ints `i64::MAX` and `1`
   is still a float.
4. Refused, 400, nothing buffered: `-1`, `5.0`, `1e3`, `"5"`, `true` declared uint64; an
   undeclared `i64::MAX + 1`, whose message names `uint64`; a declaration no document
   carries.
5. Format: every tag round-trips; tag 8 short of 8 bytes is refused as corrupt in blocks
   and decodes as unreadable in the condition codec; flag 3 round-trips its uint64 table
   and an unknown flag 4 is refused; a segment with no uint64 keeps flag 2's or flag 1's bytes; the zone pruning
   property prunes no block holding a satisfying uint64 row, and a block of uint64s
   entirely below a `Gt` literal is pruned, in a segment holding only ints and uint64s.
6. Existing tests pass. The ones extended (the zone pruning property, the codecs'
   fixtures, the key order, the numeric comparison property) gain uint64s and lose none of
   their cases.

## Test plan
Server tests in `crates/pstore-server/tests/it/uint64.rs`; format tests beside `docs.rs`'s,
`reader.rs`'s and `cmp_numbers`; filter and pruning tests beside `compare` and
`could_admit`. Red before: 1 to 4 on the parent (the declaration is refused). Mutations,
each to be killed:
- `u64::MAX` answered as a float, or as `i64` wrapped (1);
- a uint64 against an int compared as `i64` (2), and against a float through `f64` (2);
- the uint64 zone not recorded, or its `min`/`max` swapped (5);
- flag 3 written when no uint64 zone exists (5);
- `5` uint64 and `5` int as two facet buckets (3);
- a negative or fractional value accepted (4);
- the condition codec's tag 8 dropped (2, 5);
- a uint64 not marking the segment typed (5); the old refusal message kept (4);
- an integral float at `2^63` reported as a float (3); a uint64 sum answered as `i64`
  wrapped or as a float (3).

## Review
Spec review round 1 (three majors, all taken): which value a mixed bucket reports; the sum
rule kept for int-only sums; a uint64 makes a segment typed. Minors taken: the older fold's
reading of a condition; criterion 4 checks the message.

## RA budget
Unchanged: no request is added. A uint64 zone is a name and 16 bytes per block that holds
one, as an int zone is.

## Risks
- A mixed-version cluster: an old reader refuses a segment with tag 8 or flag 3. Readers
  first; the refusal is loud.
- External sources (Parquet, Lance) keep mapping unsigned columns as today.

## Tasks
- M172.1: the format's variant, tag, zone and flag; `Number`'s unsigned case and exact
  comparison; filter, order, grouping and sum cases.
- M172.2: the server's declaration, parsing, literals and answers, docs.
