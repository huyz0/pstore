# M9h — Attribute types: float, bool, arrays, datetime; `Contains` and `ContainsAny`

**Serves:** the types rows of
[`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md) ("`float`,
`bool`, `datetime`, arrays; `Contains`/`ContainsAny`"). Eighth of M9.

⚠️ **Split before starting**, because the survey found three separable changes, not one:
- **M9h.1** — `float` and `bool` scalars. JSON already tells them apart, so they need no
  schema.
- **M9h.2** — arrays of scalars, and `Contains`/`ContainsAny`. Specified when M9h.1 lands.
- **M9h.3** — `datetime`. JSON has no datetime, so an RFC 3339 string is a datetime only if
  something *declares* it. **There is no attribute schema today**: M7d's schema is the vector
  width, text field and metric, and M9a stores each value's type with the value. Declaring
  one is new HEAD state, so it gets its own task, specified when M9h.2 lands.

Until each lands, what it adds stays `400 bad_request`, as today.

## M9h.1 — `float` and `bool`

### Delta

**Values.** `Value` gains `Float(f64)` and `Bool(bool)`.
- The block and bundle codec gives them tags `2` and `3`, after `Int = 0` and `Str = 1`.
  Tags 0 and 1 are unchanged, so every existing segment and bundle stays readable.
- (Spec review, M1.) A segment that holds a float or a bool is written with footer
  `VERSION = 2`, which a binary from before M9h.1 refuses at `open` (`UnsupportedVersion`).
  This matters because the refusal has to happen at open. The old reader ignores trailing
  bytes, so without the version bump it would open the segment, prune on its integer zones
  alone, and silently drop a float row it never decoded. A bundle holding one is refused at
  replay (`unknown value tag`). A segment that holds neither stays `VERSION = 1` with **the
  same bytes as before**. The new reader reads both versions.
- `Value`'s `Eq` and `Ord` become hand-written: a float compares by `f64::total_cmp`, and
  the variant order is unchanged, with the new variants after it. That structural order is
  **never** what a filter, a zone check or an order-by uses (spec review, M3): they compare
  numbers numerically, as below, where `-0.0 == 0.0`.
- `check_storable` refuses a non-finite float (NaN or ±∞) where documents enter the engine.
  JSON cannot express one, but the engine API can.

**Over HTTP.**
- The split between `int` and `float` is whatever serde_json parsed: an `i64` is an `int`
  and an `f64` is a `float`. So `2.0` and `1e3` are floats, and so are the integer literals
  serde_json itself parses as `f64` (spec review, M4): anything beyond `u64`, anything below
  `i64::MIN`, and `-0`. Those are stored as the floats they became. The server cannot see
  the literal, and refusing them would need serde_json's `arbitrary_precision` feature
  workspace-wide, which this milestone does not take on.
- An integer in `(i64::MAX, u64::MAX]` arrives as a `u64` and is **refused**, as today.
- A `true` or `false` is a `bool`.
- A value comes back as the same JSON number and type. `2.0` returns as `2.0`, not `2`, and
  `1e3` returns as `1000.0`.
- Still refused: `null`, an object, and an array (until M9h.2).

**Filters.**
- `Eq`, `Lt`, `Lte`, `Gt`, `Gte`, `In`, and their negations, take a float or a bool.
- **Numbers compare as numbers**: an `int` and a `float` are compared **exactly**. The
  comparison is not a cast through `f64`, so `9007199254740993` is greater than
  `9007199254740992.0`.
- `-0.0` equals `0.0`.
- Bools compare with `false < true`.
- A bool against a number, and either against a string, is false, as a cross-type
  comparison is today.
- `In` admits a row equal to any member **under these rules**, so `2.0` is in `[1, 2]` and
  `-0.0` in `[0]`. It no longer uses `Value` equality.
- The legacy `pstore_format::Filter` (the engine's `scan`) keeps `Value` equality on
  purpose, and its `could_match` answers `true` for a float or bool literal (spec review, m1).

**Zone maps.** A float attribute is pruned like an integer.
- **Where they live.** Only a `VERSION = 2` segment has anything after the existing zones
  in the `Blocks` payload: a `u8` flag. `1` means zone maps are on, and one float table per
  block follows. `0` means the segment is zone-free (the fallback), and nothing follows.
  Bytes left after that are refused as corrupt (spec review, m3). The float `(min, max)` are
  computed numerically. Both fall under `INDEX_BUDGET` and the zone-free fallback.
- **Pruning.** A block is skipped for a numeric literal only if **both** of these rule it
  out, compared exactly:
  - its integer zone for the name;
  - its float zone for the name.
- **An absent zone** (restated at spec review, M2):
  - **Flag `1`:** zone maps are known to be on, so an absent zone of either type means no
    row of that type holds the name.
  - **`VERSION = 1`:** the segment holds no float, so the float side always rules out, and
    an absent int zone rules nothing out, as today.
  - **Flag `0`:** the zone-free fallback. Both sides rule nothing out.
- **Bools** have no zone and never prune.

**rank_by.**
- Numbers form one group, ints and floats interleaved by exact value, with ties broken by
  id.
- Ascending order is bools (`false` first), then numbers, then strings, then absent.
  Descending order is strings, then numbers, then bools, then absent.
- For the int-and-string indexes that exist today, both orders are unchanged.

**Does not change:** the depth of any query, what a request costs when no float is stored,
or any request that carries no float or bool.

### Acceptance criteria

1. **Round trip.** A write of `1.5`, `2.0`, `-0.25`, `1e3`, `true` and `false` returns the
   same JSON numbers and types, `2.0` as `2.0`. That holds both from the unfolded memtable and
   after a fold.
2. **Comparison.**
   - `["n","Gt",1.5]` admits `int 2` and `float 1.75`, not `int 1`.
   - `["n","Eq",2]` admits `float 2.0`.
   - `["n","In",[1,2.5]]` admits `int 1` and `float 2.5`.
   - `["b","Eq",true]` admits only `true`.
   - A bool never equals `1`.
   - `int 9007199254740993` is `Gt` `float 9007199254740992.0`, and is not `Eq` to it.
   - `-0.0` is `Eq` to `0` and to `0.0`.
   - `["n","In",[1,2]]` admits `float 2.0`, and `["n","In",[0]]` admits `float -0.0`.
3. **Pruning pays.** 2,000 rows are folded with `x = i + 0.5`, one attribute per row. A
   rank_by over `x` filtered by `["x","Lt",64.0]` reads at most a quarter of the bytes of
   the same query filtered by the equivalent `["Not",["x","Gte",64.0]]`, which cannot
   prune. The same holds for the integer literal `["x","Lt",64]`. An int column,
   `x = i`, prunes by the same margin for both `Lt 64` and `Lt 64.0`: its segment is still
   `VERSION = 1`.
4. **Pruning is sound.** One attribute holds ints in some rows and floats in others, across
   blocks. The values include `-0.0`, `0.0`, `0`, `2^53 + 1` and its float neighbour.
   Three segments are tested:
   - these rows with zone maps (flag `1`);
   - the same rows sealed zone-free, forced with an index budget between the zone-free and the zone-mapped index sizes
     (flag `0`) -- below the zone-free size the segment is refused instead;
   - an int-only segment (`VERSION = 1`).
   In each, for each operator, each literal in a set that includes those values, and each
   of `In` and `Not`, the segment admits the same rows through `rows_where` and
   `could_admit` as brute-force `admits` over every row.
5. **Bytes kept.** A segment of int, string and vector rows is byte-for-byte the segment the
   writer produced before M9h.1, pinned by a hash recorded on the base commit.
6. **rank_by order.** Ascending order over `true`, `false`, `1`, `1.5`, `2`, `"a"` and an
   absent row gives `false, true, 1, 1.5, 2, "a", absent`. Descending gives `"a", 2, 1.5, 1,
   true, false, absent`. Existing order tests pass untouched.
7. **Refusals.** Each of these is `400`, from a write or from a filter:
   - an integer in `(i64::MAX, u64::MAX]`, e.g. `9223372036854775808`;
   - `null` as a value;
   - an object;
   - an array.
   The engine refuses NaN and ±∞. `18446744073709551616` is stored and returned as the f64 2^64, as
   serde_json parsed it; it serialises as `1.8446744073709552e19`, so the test compares JSON
   values rather than text. The two refusal tests that listed `1.5`, `true` and
   `[1, 2.5]` drop those members, because this milestone gives them meaning; every other
   member stays.
8. `./scripts/gates.sh` passes; `./scripts/mutants.sh` over the diff misses 0.

### Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | a float is refused at the door | a tag swapped; a float rendered as an int |
| 2 | a float is refused in a filter | a cast through `f64`; `-0.0` unequal; a bool made a number |
| 3 | a float column never prunes | the float table unread |
| 4 | (after 3) | a missing zone read as "no rows" under flag `0` or `VERSION = 1`; one side's zone alone deciding; `-0.0` ordered below `0.0` |
| 5 | (a regression guard; seen red with the version and flag always written) | `VERSION = 2` for a float-free segment |
| 6 | a float sorts as a string | the group order; an int and a float in different groups |
| 7 | accepted by a lax door | a bound unchecked |

### RA budget

Unchanged. The float table lives in the `Blocks` payload, so it arrives with the suffix
read, as the integer zones do.

### Risks

- Rolling back past M9h.1 once a float or bool is stored fails loudly on those segments and
  bundles. So does every node still on the old binary during a rolling deploy (spec review,
  M1).
- A name holding both ints and floats carries two zones per block, which spends more of
  `INDEX_BUDGET`. Past that budget, the existing fallback seals the segment without zones.

## M9h.2 — arrays, `Contains`, `ContainsAny`

To be specified when M9h.1 lands.

## M9h.3 — `datetime`, declared

To be specified when M9h.2 lands.
