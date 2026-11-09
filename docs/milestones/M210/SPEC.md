# M210: named `ForEachUnique` groups and unary `Count`

## Serves
Workstream F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Unary
Count, attribute Sum, grouped counts and named `ForEachUnique` groups work in multi.
Duplicate elements in one array do not double-count the same unique group." D-34 unchanged.

## Delta
Today `group_by` (M12) is 1 to 8 attribute names, and an array value keys its row as the
whole array. An aggregate is `[kind, attribute]`; `["Count"]` is refused.

After, a `group_by` entry is an attribute name, as before, or a **named group**: an object
of one entry, `{"tag": ["ForEachUnique", "tags"]}`. Its key `tag` names the group's field in
`aggregation_groups`; its value explodes the attribute `tags`:
- an array value puts the row in one group per **unique** element, keyed as M12 keys values
  (`1` and `1.0` are one element; a null element (M175) is its own group). An element twice
  in one row's array counts the row once in that group;
- a scalar value puts the row in that value's group, as a plain entry does;
- an absent attribute or an empty array puts the row in the absent group (`null`), as a
  plain entry puts a row without the attribute.

With plain entries beside it, a row is in one group per element, each combined with the
row's plain keys. Groups are ordered by key and bounded by `top_k` exactly as M12's are: one
(row, element) is offered as one keyed row, so a key among the `top_k` smallest is complete.
`meta.unfolded_hits` counts an unfolded row once when any of its groups was kept.

`["Count"]` is accepted and equals `["Count", "id"]`: the rows admitted.

Facets (M65) and a multi-query's aggregation (M164) take `group_by` through the same parse,
so both accept named groups and unary `Count` unchanged.

Refused with `bad_request`, before any blob request:
- a second `ForEachUnique` entry ("group_by may hold one ForEachUnique entry"): two would
  multiply a row's groups by both arrays' lengths;
- an object entry that is not one name mapped to `["ForEachUnique", attribute]`, naming
  `ForEachUnique`;
- a name that is also an aggregate label, as a plain name already is, or another entry's
  name or attribute ("a group would carry it twice").

Unchanged: plain `group_by` entries keep keying a row by the whole array; the count fast
path (no `group_by`); every limit (8 entries, 16 labels, `top_k` up to 10,000).

## Acceptance criteria
1. Server, over 30 rows whose `tags` hold 0 to 3 elements drawn from 5, some arrays holding
   one element twice, some rows a scalar `tags`, some none: `{"tag": ["ForEachUnique",
   "tags"]}` with `["Count"]` and `["Sum", "n"]`, with and without a filter, equals a
   reference that counts each row once per unique element, through folded and unfolded rows.
2. Duplicates: a row whose `tags` is `["a", "a", "b"]` alone gives groups `a` and `b`, each
   count 1 and sum equal to the row's `n`; `[1, 1.0]` gives one group.
3. Combined: `group_by: [{"tag": ["ForEachUnique", "tags"]}, "color"]` equals the reference's
   (element, color) groups, ascending by (element, color).
4. Bound: at `top_k` 2 over rows spread across two folded segments, the two smallest
   elements' groups are exactly the reference's.
5. Multi and facets: a multi-query holding the criterion 1 aggregation equals it alone, and a
   ranked query's `facets` grouped by a named group equals the same aggregation over the
   same filter.
6. Unary count: `{"n": ["Count"]}` equals `{"n": ["Count", "id"]}` filtered and not, and
   takes the count fast path's depth (0 segments read) unfiltered.
7. Refusals, each `bad_request` naming what is stated, no blob request: two `ForEachUnique`
   entries; `{"t": ["Avg", "tags"]}`; `{"t": "tags"}`; an object of two entries; a name equal
   to a label; a name equal to a plain entry.
8. Every existing test passes unedited, but for `aggregate.rs`'s refusal case
   `{"n": ["Count"]}`, which this milestone accepts (criterion 6 asserts it).

## Test plan
Unit tests in `pstore-query`'s `aggregate.rs` for 2 and 4's merge; server tests in a new
`crates/pstore-server/tests/it/group_each.rs` for 1, 3, 4, 5, 6, 7. Red before: every new
test fails (the parent refuses an object entry and `["Count"]`). Mutations, each killed:
- elements not deduplicated (2);
- a scalar or absent value given no group (1);
- the exploded key's position swapped with a plain key's (3);
- an unfolded row counted once per element in `unfolded_hits` (1, by the meta);
- the second-`ForEachUnique` refusal removed (7); unary Count counting nothing (6);
- a kept group's key holding the whole array rather than the element (4).

## RA budget
Unchanged: an aggregation reads what it read; exploding happens in memory per row. CPU per
row is its array's unique elements, bounded by the write path's array limit.

## Risks
- A client that grouped by an array attribute to get whole arrays keeps that with a plain
  entry; a named group is opt-in.
- The vendor does not say how duplicates in one array count; pstore counts the row once a
  group, which the plan requires. The migration notes say so.

## Tasks
- M210.1: the parse, the aggregator, tests, docs. DELIVERY.md's F aggregate row names M210.
