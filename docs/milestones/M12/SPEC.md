# M12 — Aggregations

**Serves:** the `aggregate_by` / `group_by` row of
[`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md), the
aggregation section of [`query-path.md`](../../research/08-query-engine/query-path.md)
("counts, sums … with zone maps and precomputed per-segment partial aggregates"), and OQ-68.

## What is true today

A query can rank rows or order them (M9e), but it cannot count or sum them. Counting an index
through the API means paging every id out of it.

## Delta

**The request.**

```json
{"aggregate_by": {"n": ["Count", "id"], "total": ["Sum", "price"]},
 "group_by": ["color"], "filters": [...], "top_k": 10, "consistency": ...}
```

- `aggregate_by` maps 1 to 16 labels to an aggregate:
  - `["Count", "id"]` counts the matching rows;
  - `["Count", attr]` counts the matching rows where `attr` is present;
  - `["Sum", attr]` sums the matching rows' **numeric** values of `attr`. A string, bool,
    datetime, array or absent value adds nothing.
- `group_by` is optional: 1 to 8 attribute names.
- Every `consistency` level applies, as for a `rank_by` order, and so does `as_of`.

**What a sum is.**
- The integers are summed exactly, in `i128`.
- With no float among the values, the sum is an integer. Outside the `i64` range, it is
  reported as the nearest `f64`.
- With any float, the sum is an `f64`: the floats summed in the order the rows are visited,
  plus the exact integer sum converted once (code review: the implementation is more exact
  than "each integer converted" and the spec follows it). The visit order is not stable across
  folds, so the last bits may differ.
- A sum over no numeric value is `0`.
- A float sum that overflows to ±∞ is reported as `null`, since JSON cannot carry it (spec
  review).

**The response.**
- Without `group_by`: `{"aggregations": {"n": 42, "total": 17.5}, "results": [], "meta": …}`.
- With it: `"aggregation_groups": [{"color": "red", "n": 3, "total": 9}, …]`. Groups are
  in ascending key order, and there are at most `top_k` of them.
- `meta` is a query's: `epoch`, `consistency`, `cost`, `session` and `staleness_ms`.
  `unfolded_hits` is the number of the process's unfolded rows the aggregation counted: an
  admitted row whose group fell outside the `top_k` smallest was not counted (code review).

**What a group is.**
- A group's key is the tuple of its rows' `group_by` values, absent as `null`.
- Numbers group by numeric value, so `1` and `1.0` are one group. An integral float within
  the `i64` range is reported as an integer, exactly, even above 2^53. This is the filters'
  equality (M9h.1), not `Value`'s structural one. A datetime is never a number.
- Arrays group as whole values, element by element, with the same scalar rules, so `[1]` and
  `[1.0]` are one key.
- **The order** (spec review, M1):
  - A scalar key orders by type group as a `rank_by` ascending does: bools, numbers,
    datetimes, strings, arrays, and absent last. Within a group it orders by value: `false` before
    `true`, numbers by numeric value, datetimes by instant, and strings bytewise.
  - Arrays order lexicographically by element, a proper prefix first. (`rank_by` ties
    arrays; a group has no id to break the tie, so it needs this total order.)
  - A tuple of several `group_by` attributes orders lexicographically, by attribute in
    request order.
  - Two keys are one group exactly when this order calls them equal.

**Bounded memory, exact answers.**
- Only the `top_k` smallest keys are held: a key larger than every kept key, when the
  selector is full, is dropped. Once the selector is full, its largest kept key only falls,
  so a dropped key never returns, and every kept key's aggregate is complete.
- Per segment, each keeps its own `top_k` smallest, and the results are merged. A key among
  the global `top_k` smallest is among every segment's own `top_k` smallest, so the merge is
  exact.

**The engine and the query crate.**
- `pstore_query::aggregate` visits admitted rows exactly as `select` does: two rounds (open,
  then the admitted blocks). The visiting is shared, not copied.
- The process's unfolded rows are offered after the segments, shadowed as for an order.
- `Engine::aggregate_as` does for aggregation what `ordered_as` does for an order: the same
  HEAD and fresh-view handling, `strong` probes, `bounded` cache and fallback.

**The count fast path.** When there is no filter and no `group_by`, and every aggregate is
`["Count", "id"]`, the count may be HEAD's arithmetic.
- **When** (spec review, M3): the decision is made on the fresh view from the **same**
  `head_and_fresh_as` call as the full path, never on the memtable read beforehand. With
  any unfolded row or delete for the index in that view, it takes the full path.
- **Live reads only** (code review, B1): `Head::as_of` rebuilds a buried segment and a buried
  delete vector with a count of 0. So a past epoch's arithmetic is wrong after a compaction,
  a drop or a replaced vector, and an `as_of` count takes the full path, which reads the
  vectors themselves.
- **An index with no segment and nothing unfolded** is `404`, on both paths, as for an order
  (`exists`).
- **Levels** (spec review, M2): every level runs as on the full path. `strong` still probes
  its lanes, a `session` token is still checked, and a `bounded` hit reads nothing. So
  "1 read" is `eventual`'s cost.

The count itself is each segment's rows less its deleted rows (HEAD carries both), as
`index_stats` counts documents. So it is **one read**, and no segment is opened. That is
[`query-path.md`](../../research/08-query-engine/query-path.md)'s "answered entirely from the
index section".

**OQ-68, answered for this scope:** no DataFusion. Count, sum and a bounded group map over
the existing visitor are a few hundred lines. DataFusion's weight buys SQL, which nothing
here asks for.

**Refusals, each `400 bad_request`:**
- `aggregate_by` with `vector`, `text`, `rank_by`, `offset`, `include_attributes` or
  `exclude_attributes`;
- `group_by` without `aggregate_by`;
- an unknown aggregate, or `Sum` of `id`;
- 0 or more than 16 labels, or 0 or more than 8 `group_by` attributes;
- a label equal to a `group_by` attribute;
- `top_k` over 10,000, an order's bound (code review);
- an aggregation inside a multi-query. That is deferred, because a multi-query's results are
  lists of rows.

**Does not change:** any other query, the format, or any request count except the new path's.

## Acceptance criteria

1. **Count and sum.** (Test data is chosen so that no float sum cancels near 0.)
   - Over 2,000 folded rows plus unfolded writes, which include an upsert and a delete of
     folded ids, the aggregates equal a brute-force computation over the same rows. That
     covers `Count id`, `Count attr`, `Sum` of an int column, and `Sum` of a mixed int and
     float column (float within `1e-9` relative).
   - This holds with and without a filter, and at `as_of` a past epoch.
2. **Groups.**
   - `group_by` one and two attributes: every group and its aggregates equal brute force.
   - `top_k = 3` returns exactly the 3 smallest keys, with complete aggregates, over at
     least 3 segments whose key sets differ. In one segment, one of the global 3 smallest
     keys is that segment's own 3rd smallest (spec review, M4).
   - **Memory** (spec review, M4): an aggregator offered 1,000 distinct keys with
     `top_k = 3` holds at most 3 groups at every step, asserted by a unit test as
     `Selector`'s bound is.
   - Array keys `[1]`, `[1.0]` and `[1, 2]`: the first two are one group, ordered before the
     third.
   - `1` and `1.0` form one group, and absent is `null` and last.
3. **Fast path.** An unfiltered, ungrouped `Count id` at `eventual`, over 2,000 folded rows
   in 4 segments with one of them carrying deletes:
   - costs exactly **1 read**, and equals the full path's count;
   - with one unfolded write, or one unfolded delete, takes the full path and still equals
     brute force;
   - `as_of` a past epoch, after the segments that held it were compacted, equals brute force
     over that epoch's rows (code review, B1);
   - with only an unfolded delete, on a reader holding nothing else, it takes the full path
     and equals brute force (code review, M2);
   - a missing index is `404`.
4. **Depth.** With a filter, an aggregation's depth is at most 3.
5. **Consistency.** On the fast-path shape and on a filtered one:
   - a `strong` aggregation is refused while another process has an unfolded write;
   - a `session` one with that write's token is refused too;
   - a `bounded` hit reports `staleness_ms`, and reads no HEAD. On the fast-path shape it reads
     nothing at all (spec review round 2: a filtered hit still reads its segments).
6. **Refusals.** Each case above is `400`, and its message names the aggregation rule
   broken. Several would otherwise be 400 for an unrelated reason, such as "needs vector"
   (spec review, minor).
7. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `aggregate_by` unknown | a shadowed or deleted row counted; a float summed as an int |
| 2 | as 1 | a dropped key re-admitted; a per-segment keep of fewer than `top_k` |
| 3 | as 1 | the fast path taken with unfolded rows; deletes not subtracted |
| 4 | as 1 | a row round added |
| 5 | as 1 | the level ignored |
| 6 | accepted | a check unchecked |

## RA budget

- The full path costs the same as a `rank_by` order with the same filter: HEAD, one open
  round, and one round of admitted blocks. **Depth 3.**
- The fast path is 1 read, at depth 1.
- Memory is `top_k` groups per segment, plus one partial per label.

## Risks

- A high-cardinality `group_by` with a large `top_k` holds that many groups per segment.
  `top_k` is the bound, and there is no separate cap.
- Float sums depend on visit order. This is stated above rather than hidden.

## Tasks

- **M12.1** — criteria 1–7.
