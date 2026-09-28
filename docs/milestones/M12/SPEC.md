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
- With any float, the sum is an `f64`, the integers converted, summed in the order the rows
  are visited. That order is not stable across folds, so the last bits may differ.
- A sum over no numeric value is `0`.

**The response.**
- Without `group_by`: `{"aggregations": {"n": 42, "total": 17.5}, "results": [], "meta": …}`.
- With it: `"aggregation_groups": [{"color": "red", "n": 3, "total": 9}, …]`. Groups are
  in ascending key order, and there are at most `top_k` of them.
- `meta` is a query's: `epoch`, `consistency`, `cost`, `session` and `staleness_ms`.
  `unfolded_hits` is the number of the process's unfolded rows the aggregation counted.

**What a group is.**
- A group's key is the tuple of its rows' `group_by` values, absent as `null`.
- Numbers group by numeric value, so `1` and `1.0` are one group. An integral float within
  the `i64` range is reported as an integer. This is the filters' equality (M9h.1), not
  `Value`'s structural one.
- Arrays group as whole values.
- Keys order as a `rank_by` ascending does: bools, numbers, datetimes, strings, arrays, and
  absent last.

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

**The count fast path.** When there is no filter and no `group_by`, every aggregate is
`["Count", "id"]`, and the index has nothing unfolded in this process, the count is HEAD's
arithmetic. It is each segment's rows less its deleted rows (HEAD carries both), as
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
- an aggregation inside a multi-query. That is deferred, because a multi-query's results are
  lists of rows.

**Does not change:** any other query, the format, or any request count except the new path's.

## Acceptance criteria

1. **Count and sum.**
   - Over 2,000 folded rows plus unfolded writes, which include an upsert and a delete of
     folded ids, the aggregates equal a brute-force computation over the same rows. That
     covers `Count id`, `Count attr`, `Sum` of an int column, and `Sum` of a mixed int and
     float column (float within `1e-9` relative).
   - This holds with and without a filter, and at `as_of` a past epoch.
2. **Groups.**
   - `group_by` one and two attributes: every group and its aggregates equal brute force.
   - `top_k = 3` returns exactly the 3 smallest keys, with complete aggregates, over at
     least 3 segments whose key sets differ.
   - `1` and `1.0` form one group, and absent is `null` and last.
3. **Fast path.** An unfiltered, ungrouped `Count id` over 2,000 folded rows in 4 segments,
   one of them with deletes, costs exactly **1 read** and equals the full path's count. With
   one unfolded write it takes the full path and still equals brute force.
4. **Depth.** With a filter, an aggregation's depth is at most 3.
5. **Consistency.** A `strong` aggregation is refused while another process has an unfolded
   write, and a `session` one with that write's token is too. `bounded` reports
   `staleness_ms`.
6. **Refusals.** Each case above is `400`.
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
