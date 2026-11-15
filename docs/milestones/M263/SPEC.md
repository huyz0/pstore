# M263: `Min` and `Max` over strings, bools, uuids and datetimes

## Serves
The [turbopuffer gap list](../../comparisons/turbopuffer-gaps-2026-11-15.md), "Shipped by the
vendor after the pinned spec": the vendor's `Min`/`Max` take any scalar but `bytes`.
Workstream F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md). No D- or OQ-
id covers it.

Today `Min` and `Max` ([M211](../M211/SPEC.md)) see numbers only: an attribute holding only
strings answers `null`.

## Delta
1. **An opt-in per aggregate**: `["Min", attribute, {"non_numeric": true}]`, and `Max` alike.
   Opted in, when the attribute's admitted values include **no number**, `Min` and `Max`
   answer the smallest and largest of its **string, bool, datetime and uuid** values, ordered
   as a group key orders them (`Key`: bool, then datetime by instant, then string bytewise,
   then uuid by its bytes; `false < true`). Reported as the value is: a string as a string, a
   bool as a bool, a datetime as RFC 3339, a uuid hyphenated lowercase.
2. **When any admitted value is a number, the answer is plain `Min`'s**: the numeric extreme,
   every non-number ignored.
3. **Without the option, or with `false`, nothing changes**: `Min` and `Max` see numbers
   only, and an attribute holding none answers `null`, as M211 pins.
4. **Bytes and arrays** are ignored, opted in or not, and `Avg` stays numbers only. Options
   on any other aggregate, an options object other than `{"non_numeric": bool}`, or a third
   element that is not an object, are refused `400` before any blob request.
5. **Everywhere `Min`/`Max` run**: ungrouped, grouped, merged across segments and unfolded
   rows. The block path (M232) is untouched: block statistics exist only for an attribute
   every value of which, in the segment, is a number, so a block never holds the others.

**Why an opt-in, not a default and not a setting.** A client doing arithmetic on `Min` today
gets a number or `null`; a default that answers a string breaks it, and M211's tests pin
`null` for an attribute holding only strings. That is a measured downside, so by the settings
rule the default cannot change. A per-aggregate opt-in leaves every existing answer and test
as it is, which a cluster default and per-index override would add nothing to: the caller
that wants the vendor's semantics, the facade above all, asks for it in the query.

**Not changed:** `Sum`, `Count`, `Avg`, plain `Min`/`Max`, the block path, the other refusals.

## Acceptance criteria
1. **Each kind.** `min_max_over_each_scalar_kind`, in
   `crates/pstore-server/tests/it/group_expanded.rs`: over rows whose attributes hold only
   strings (`"pear"`, `"apple"`, `"fig"`), only bools, only datetimes, only uuids, opted-in
   `Min` and `Max` answer the golden extreme in the reported form, and plain `Min` answers
   `null`: over unfolded rows, one segment, a segment beside unfolded rows, and two segments
   holding different extremes.
2. **Numbers win.** `min_max_numbers_win_over_other_kinds`: an attribute holding numbers and
   strings answers the numeric extremes, equal to plain `Min` and `Max`.
3. **Grouped.** `min_max_of_strings_per_group`: grouped by another attribute, each group's
   string extremes.
4. **Ignored.** `min_max_ignore_bytes_and_arrays`: bytes and arrays alone answer `null`, and
   `Avg` of strings answers `null`.
5. **Refused.** `min_max_options_are_refused_when_malformed`: each malformed option `400`,
   naming why, with no blob request; `{"non_numeric": false}` is plain `Min`.
6. **Unchanged.** `min_max_and_avg_equal_a_reference_through_folded_and_unfolded_rows`
   (M211), its assertions unchanged, passes.
7. **Gates:** fmt, clippy, the workspace tests (the two IPv6 tests NOT-RUN in this
   container), and the named mutations below observed red.

## Test plan
AC1 to AC3 are seen red against the parent, which refuses a three-element aggregate.

| AC | Mutation |
|---|---|
| 1 | `MinAny` answering the largest other; the merge dropping the other side's others; the option ignored; the smallest kept as the first seen |
| 2 | an other preferred over a number |
| 4 | bytes admitted |
| 5 | an options object with extra keys accepted |
| 6 | plain `Min` folding the others and answering them (each alone is equivalent) |

## RA budget
Unchanged: an aggregation reads what it reads today.

## Risks
- **Mixed kinds.** An attribute holding strings in some rows and datetimes in others answers
  by the key order across kinds, which is pstore's own; the vendor's schema forbids it.

## Tasks
- **M263.1** The spec.
- **M263.2** `Aggregate::MinAny`/`MaxAny`, the accumulator, the total, the server's option
  and rendering, tests, docs.
