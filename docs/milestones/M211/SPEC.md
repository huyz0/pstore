# M211: expanded grouping: `Min`, `Max`, `Avg` and numeric buckets

## Serves
Workstream F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): the
"expanded grouping" of the F sorting/grouping row, as the
[Vespa comparison](../../comparisons/vespa-2026-10-29.md) row 5 names it: min, max and avg,
and numeric buckets. D-34 unchanged.

Not served: nested groups (a group's own sub-groups) and hits per group. A hard per-group
quota of hits is `limit_per` (M146); nested groups stay open in DELIVERY.md.

## Delta
Today an aggregate is `Count` or `Sum` (M12, M210), and a `group_by` entry is an attribute
name or a named `ForEachUnique` group (M210).

After, three more aggregates, each `[kind, attribute]` over the attribute's **numbers**
(int, uint64, float; anything else, and an absent attribute, is skipped, as `Sum` skips it):
- `["Min", a]` and `["Max", a]`: the smallest and largest number, reported as it is stored
  and keyed (`1.0` reports `1`, as a group key does); `null` when the group holds none;
- `["Avg", a]`: the numbers' sum over their count, a float; `null` when there are none. Its
  sum is `Sum`'s, so an average of integers is exact up to 2^53 per term.

And one more named group, `{"band": ["Bucket", a, width]}`, `width` a positive finite number:
a row whose `a` is a number is in the group keyed by its bucket's lower bound,
`floor(a / width) * width`, computed in integers when both are integers, else in floats; any
other value, and no value, is the `null` group. Buckets order and are bounded by `top_k` as
keys are (M12). A `Bucket` entry may sit beside plain entries and a `ForEachUnique` one; it
explodes nothing.

Refused with `bad_request` before any blob request: `Min`, `Max` or `Avg` of `id`; a bucket
`width` that is not a positive finite number, or a `Bucket` entry not shaped
`[\"Bucket\", attribute, width]`, naming `Bucket`. Every other limit is M12's and M210's.

Facets and a multi-query's aggregations take both, through the same parse (M164, M210).
Unchanged: `Count`, `Sum`, the count fast path, and every existing shape.

## Acceptance criteria
1. Server, over 40 rows whose `price` is an int, a float, a string or absent in a fixed
   pattern, `Min`, `Max`, `Avg`, `Sum` and `Count` of `price`, unfiltered and filtered,
   ungrouped and `group_by: ["color"]`, through folded and unfolded rows, equal a reference
   over the rows written; a group with no number reports `null` for all three.
2. Types: `Min`/`Max` report an int as an int and a non-integral float as a float; `Max` of
   a uint64 above `i64` reports it exactly; `Avg` of `[1, 2]` is `1.5`.
3. Buckets: `{"band": ["Bucket", "price", 10]}` groups equal a reference's
   `floor(price / 10) * 10` groups, the non-number rows in the `null` group last; a width of
   `2.5` groups floats by float bounds; a negative price falls in the bucket below zero
   (`-1` in `-10`).
4. Combined and bounded: `Bucket` beside `color` and a `ForEachUnique` entry equals the
   reference's groups; at `top_k` 2 the two smallest buckets are exact across two segments.
5. Multi and facets: a multi-query holding the criterion 1 aggregation, and a ranked query's
   facets grouped by a `Bucket`, equal the same aggregation alone.
6. Refusals, each `bad_request` with no blob request: `["Min", "id"]`; `["Avg"]`; widths `0`,
   `-1`, `"10"`; `["Bucket", "price"]`; `{"b": ["Bucket", "price", 10], "c": ...}` of two.
7. Every existing test passes unedited, but for `aggregate.rs`'s refusal case
   `["Avg", "price"]`, now accepted: it becomes `["Median", "price"]`, refused by the same
   message.

## Test plan
Unit tests in `pstore-query`'s `aggregate.rs` for 2, and for 3's integer and float bucket
arithmetic; server tests in a new `crates/pstore-server/tests/it/group_expanded.rs` for 1,
3, 4, 5 and 6. Red before: every new test fails (the parent refuses `Min`, `Max`, `Avg` and
`Bucket`). Mutations, each killed:
- `Min` and `Max` swapped, or a merge taking the other side's value (1, 4);
- `Avg` divided by the rows rather than the numbers (1: absent and string prices);
- a bucket computed by truncation rather than floor (3: the negative case);
- an integer bucket computed in floats (3: a uint64 above 2^53 keeps its bucket exact, in the unit test);
- the width check accepting 0 (6).

## RA budget
Unchanged: the aggregates fold in memory per row, as `Sum` does; a bucket is a key.

## Risks
- A float average differs from a reference summing in another order by rounding; the test
  compares within 1e-9 relative, as M12's sums do.

## Tasks
- M211.1: aggregates, buckets, parse, tests, docs. DELIVERY.md's F grouping row names M211.
