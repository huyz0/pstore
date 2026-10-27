# M139: affected ids for by-filter operations

## Serves
Workstream C of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("actual
affected counts/IDs"), the last row of C in [DELIVERY.md](../../comparisons/DELIVERY.md)
still pending: [M130](../M130/SPEC.md) refused `affected_ids` beside a by-filter operation
because its ids were unbounded. [M136](../M136/SPEC.md) bounded them and
[M137](../M137/SPEC.md) made the bound resumable. Under D-56 in [memory
management](../../research/09-rust-stack/memory-management.md).

## Delta
Today `affected_ids` beside `delete_by_filter` or `patch_by_filter` is refused with
`bad_request`.

After, at `PSTORE_APPLIED_WRITES=5`, it is served:
- Each by-filter operation of an `affected_ids` write carries its own row bound,
  `10,000 − n`, where n is the distinct ids the write names (M130's count). Its effective
  bound is the smaller of that and the engine's (M136), in the scan's retention (so the
  fold holds no more) and in fold order.
- A write with a by-filter operation that names 10,000 ids would leave a bound of 0, and
  a partial operation could never progress, so it is refused at the door with
  `bad_request`: n must be below 10,000.
- Past its bound a plain operation is refused whole (`rejected`, no ids), as M136; a
  partial one (M137) changes at most its bound's rows, lists their ids, and says
  `rows_remaining`. Resuming lists each pass's ids.
- So a write's lists hold at most 3n entries for its named ids (one id can be upserted,
  patched and deleted) plus 10,000 − n per by-filter operation, of which there are at
  most two: at most 30,000 entries, and the outcome object with them.
- The ids are what `resolve` changed, as for per-id operations (M130): distinct, sorted,
  a row set aside is in no list.

How it is carried: the operation carries `$rows`, an integer, a reserved attribute. A
fold that predates it ignores it and would apply up to the engine's bound, recording up
to 100,000 ids; so it needs every node at M139: level 5, which includes 1 to 4 (every
level list in `main.rs` gains `5`; the binary's parsing is untested, as for levels 2 to
4). Below 5 the refusal is unchanged, `bad_request` rather than the other levels'
`unsupported_durability`, deliberately: M130's test pins it and its meaning has not
changed.

Unchanged: by-filter operations without `affected_ids`; per-id ids; the outcome format.

## Acceptance criteria
1. Engine: with an engine bound of 3, an operation carrying `$rows` 2 is refused whole
   when it admits 3 (plain), and the fold kept 2 rows for it (M136's hook), not 3; when
   partial it changes 2 and says `rows_remaining`.
2. Engine: with an engine bound of 3, an operation carrying `$rows` 5 is refused when it
   admits 4: the smaller bound wins.
3. Server, level 5: a partial `delete_by_filter` with `affected_ids` lists exactly the ids
   each pass deleted; the passes' lists are disjoint and together are every matching id.
4. Server, level 5: a `patch_by_filter` with `affected_ids` lists exactly the ids it
   patched, not one that already held the value.
5. Server, level 5: a write naming 9,997 distinct ids (deletes of absent ids) beside a
   plain `delete_by_filter` admitting 3 rows deletes them; naming 9,998 refuses it
   (`rejected` 1, no row deleted).
6. Below level 5 `affected_ids` beside a by-filter operation is refused as before: M130's
   `affected_ids_refusals_cost_nothing` passes unchanged.
7. Server, level 5: an `affected_ids` write naming 10,000 ids beside a by-filter
   operation is refused with `bad_request` before any blob request.
8. Every existing applied, idempotency, filter-row and by-filter test passes unchanged.

## Test plan
Engine: `crates/pstore-engine/tests/it/filter_rows.rs`, through a public call taking the
operation's bound. Server: `crates/pstore-server/tests/it/allow_partial.rs` for 3 and 4
(engine bound 3 by `set_filter_rows_for_test`), and 5 with the default engine bound. Red
before: 1 to 5. Mutations, each to be killed:
- `$rows` ignored (1); the larger bound wins (2);
- the server's bound is 10,000 rather than 10,000 − n (5), or off by one (5);
- the server still refuses at level 5 (3); serves below 5 (6);
- ids not recorded for by-filter changes (3, 4);
- the scan retains by the engine's bound only (1, through the hook);
- a write naming 10,000 ids with a by-filter operation is served (7).

## RA budget
No request is added. The outcome object grows by the ids listed, at most 10,000 per
by-filter operation of a write that asked. No LIST, nothing scaling with records beyond
that bound.

## Risks
- A plain by-filter operation that applied without `affected_ids` (up to 100,000 rows) is
  refused with it past 10,000 − n. The docs say to add `allow_partial`, which pages it.
- Level 5 is one more operator step; it includes every lower level.

## Tasks
- M139.1: engine `$rows`, per-operation bounds in the scan and fold order, the public call;
  server level 5, the bound, docs.

## Review
Spec review round 1 (blocking on one major, amended): a write naming 10,000 ids left a
bound of 0, on which a partial operation never progresses; it is refused (criterion 7).
Minors taken: "at most 30,000" counted right; level 5 in every list of `main.rs`, and
the `bad_request` below it stated as deliberate; the scan's per-operation bound is now
checked through the kept-rows hook.
