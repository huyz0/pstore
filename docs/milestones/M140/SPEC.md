# M140: order by up to eight attributes

## Serves
Workstream F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "up to
eight sort keys handle ties/nulls/types and ascending/descending direction", and the
"Up to 8 attribute sort keys | One key only" row of [the
comparison](../../comparisons/turbopuffer-2026-10-25.md). Extends
[M9e](../M9e/SPEC.md)'s order by attribute. D-34's depth budget is unchanged.

## Delta
Today `rank_by` is `[attribute, "asc" | "desc"]`: one key, ties broken by the id.

After, `rank_by` may also be an array of 1 to 8 such pairs,
`[["a", "asc"], ["b", "desc"]]`. Rows are ordered by the first key; rows it ties are
ordered by the second; and so on; rows every key ties are ordered by id, ascending.
- Each key keeps M9e's rules for its own direction: the groups (bools, numbers,
  datetimes, strings, arrays) in order, reversed for `desc`; absent last in both
  directions; numbers one group by exact value; arrays tie among themselves; strings
  bytewise. `id` may be a key.
- Refused with `bad_request`: no keys, more than 8, an attribute named twice, a pair that
  is not `[string, "asc" | "desc"]`. The one-pair form is unchanged, and so is every
  other refusal (`rank_by` beside a relevance ranking, `top_k + offset` past 10,000).

How: `pstore_query::OrderBy` stays one key. A new `pstore_query::Order` holds 1 to 8 of
them, built from one (`From<OrderBy>`) or checked from a list (`Order::new`). The
`Selector` and the engine's `ordered`/`ordered_as` take an `Order`; a caller with one
`OrderBy` passes it through `From`, so existing callers change by at most a `.into()`.
The selector compares a row against its worst kept row key by key, borrowed, and owns a
row's values only when it keeps it, as M119 made one key do.

Unchanged: the rounds (three, no resolve round), the 10,000 bound, `offset`, results'
shape, and every one-key answer.

## Acceptance criteria
1. The selector, over pseudo-random rows mixing ints, floats, strings, bools, datetimes,
   arrays and absent values with many ties, for 1, 2 and 3 keys in every mix of
   directions and caps from 0 to past the row count, keeps exactly a reference sort's
   prefix (the reference: compare key by key with M9e's per-key rule, then id), and never
   holds more than its cap.
2. One key orders exactly as before: `pstore-query`'s order tests and the engine's and
   server's order tests pass, their only edit a `.into()` where a call passes an
   `OrderBy`.
3. Engine: an order by two keys over two segments, a delete vector and unfolded rows
   returns the reference order, paged by `offset` and `limit`.
4. Server: `rank_by: [["a","asc"],["b","desc"]]` with `top_k` and `offset` returns the
   reference order's page; `[["a","asc"]]` answers as `["a","asc"]` does.
5. Server refusals, each `bad_request` before any blob request: `[]`, nine pairs,
   `[["a","asc"],["a","desc"]]`, `[["a"]]`, `[["a","up"]]`, `[["a","asc"], "b"]`.

## Test plan
Unit tests in `crates/pstore-query/src/order.rs`; engine
`crates/pstore-engine/tests/it/order.rs`; server a new
`crates/pstore-server/tests/it/order_keys.rs`. Red before: 1, 3, 4 and 5 (no `Order`,
the server refuses the list form); 2 is a guard. Mutations, each to be killed:
- only the first key compared (1, 3, 4); keys compared in reverse order (1);
- a later key's direction taken from the first (1);
- the id tie-break dropped or reversed (1);
- the bound of 8 off by one, or a repeated attribute accepted (5).

## RA budget
Unchanged: HEAD, segments with delete vectors, blocks; no resolve round, no LIST. CPU per
row offered grows with the keys compared, up to 8.

## Risks
- Segment pruning by zone maps is by filter only, as today, so a multi-key order reads
  what a one-key order reads.

## Tasks
- M140.1: `Order`, the selector, the engine's signature, the server's parsing, docs.
