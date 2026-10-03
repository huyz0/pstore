# M40 — A filter never names a reserved attribute

**Serves:** [BACKLOG](../BACKLOG.md) row 56, which [M36](../M36/VERIFIED.md) opened and
[M37](../M37/VERIFIED.md) narrowed.

## What is true today

- **Reserved attributes** begin with `$`: `$metric`, `$fts`, `$trgm` and `$text`, plus the
  operation marks `$op`, `$cond` and `$unset`. A tombstone is marked by the attribute named
  `""`. They travel with a row from the door to the fold, and are stripped before anything is
  sealed or served (`stripped`). A document or a patch naming one is refused at the door.
- **The API's filter parser accepts them** (`clause`, behind `predicate`). It parses query
  filters and every write's condition: the `*_condition`s, `patch_by_filter` and
  `delete_by_filter`. So `["$metric", "Eq", 2]` and `["", "Eq", null]` are accepted.
  - **Query filters already answer the same, folded or not** (spec review, correcting the
    first draft): they read the fresh view and segments, whose rows are stripped. So they
    match nothing. `the_metric_a_row_carries_is_never_returned_or_filtered_on`
    (`pstore-server/tests/distance.rs`) pins it: `200`, `results: []`.
  - **A write's condition is what differs.** Inside a fold, `resolve`, `by_filter` and the
    deferred rows' `keep` judge conditions against rows that are still unfolded in the same
    fold. Those carry their stamps, where a sealed row does not. So a condition on `$metric`
    admits an unfolded row and refuses the same row once sealed. `$text` answers
    inconsistently by fold stage: M37 stamps sealed rows after `prepare`, so `resolve` and
    `by_filter` see it, and `keep`, which runs first, does not (spec review, round 2).
- **The library's `Engine::scan`/`search`** takes the legacy `pstore_format::Filter` (`Eq`, `Gt`,
  `Lt`), and applies it to unfolded rows **before** stripping them. So `Eq("$metric", 2)`
  matches a euclidean row until it is folded. Not reachable from the API.
- **The row's other two items:**
  - a text field recorded from a row that is never sealed is by design. It is consistent with
    the field the fold judged by ([M36](../M36/VERIFIED.md), test 2c).
  - The fresh view indexes this engine's own field while the schema records none. It holds
    only this engine's rows, **but a default engine's fresh view can match a `body` writer's
    ordinary `text` attribute until a fold** ([M37](../M37/VERIFIED.md)). This item stays
    open, narrowed:
    - The server never configures a text field, so every server engine and every row it
      writes uses the default field, and no server can reach it.
    - A library embedding engines over different fields can.

## Delta

1. **The API refuses a filter naming a reserved attribute**: a name beginning with `$`, or
   empty, at any depth (under `And`, `Or`, `Not`), for every operator. It answers 400 naming
   the attribute. Every leaf is parsed by one arm of `clause`, so one check covers query
   filters and every write's condition.
2. **`Engine::scan` filters rows as it serves them**, stripped first, so `Eq("$metric", …)`
   matches no row, folded or not. Complete, because:
   - segment rows (`live_rows`, both branches) are already stripped;
   - deferred rows (`$op`, `$cond`, `$unset`) are dropped before the filter;
   - `search` inherits the change;
   - no `as_of` scan reads unfolded rows.
3. Row 56 is narrowed to the library-only fresh-view item.

**Not changed:**
- the engine's `Predicate`s and fold conditions, which a library caller may build with any
  name: (1) closes the API's door;
- the other fields that take an attribute name: `rank_by`, `group_by`, `aggregate_by`'s
  attribute, and `include_attributes`/`exclude_attributes` (spec review, minor 3). They read
  stripped rows only, so a `$` name there answers the same, folded or not. It is left alone
  deliberately.

## Acceptance criteria

1. **The API refuses a reserved name.**
   - Each of these answers 400 naming the attribute:
     - a query whose filter is `["$metric", "Eq", 2]`;
     - one nesting `["$text", "Eq", "x"]` under `Not` inside `And`;
     - one naming `""`.
   - A patch by filter and a delete by filter naming `$op` each answer 400 and write nothing.
   - **Parent:** 200.
   - An ordinary attribute in the same positions is still accepted.
   - ⚠️ `the_metric_a_row_carries_is_never_returned_or_filtered_on` **changes**: its filter on
     `$metric` answered `200` with no results, and now answers 400. The refusal is the
     stronger answer: a filter that could never match is now said to be wrong rather than
     answered empty. Its other assertion, that no result carries `$metric`, is unchanged.
2. **A scan's filter sees what it serves.** A euclidean row, unfolded, scanned with
   `Filter::Eq("$metric", Value::Int(<euclidean's code>))`, matches no row. After a fold it
   still matches none.
   - **Parent:** the unfolded row matched.
3. **Gates.** `./scripts/gates.sh` is green, and the sweep over M40's source diff misses 0.

## Test plan

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1 | `a_filter_naming_a_reserved_attribute_is_refused` (`pstore-server/tests/filters.rs`) | accepted; the check only at the top level; an exact name instead of the `$` prefix |
| 1 | `the_metric_a_row_carries_is_never_returned_or_filtered_on`, amended as above | |
| 2 | `a_scan_filters_what_it_serves` (`pstore-engine/tests/filters.rs`: a `scan` test over the legacy `Filter`, beside that file's `Predicate` tests) | the unfolded row matched |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Any | 0 | 0 | 0 | 0 | unchanged: a refusal is before any request |

## Risks

- **A client filtering on a `$` name is refused.**
  - A query filter on one matched nothing.
  - A condition on one matched only rows still unfolded in the same fold.
  - Revealer: the 400 names the attribute.

## Tasks

- **M40.1** — The refusal, the scan's order, tests 1–2.
- **M40.2** — The ledger, `BACKLOG.md` row 56 narrowed, and the roadmap row.
